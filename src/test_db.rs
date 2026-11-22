//! Pre-migrated, pooled test databases (decision 13): the fixture every
//! database test starts from.
//!
//! `#[sqlx::test]` gives each test its own database — it drops and creates a
//! database, runs every [`MIGRATOR`] migration against it, and drops it again
//! at teardown. That is roughly half a second of work per test, and its
//! `CREATE DATABASE`/`DROP DATABASE` pairs take a global advisory lock, so
//! more test threads cannot hide it: at 969 database tests it *is* most of
//! this crate's test wall time. This module keeps the isolation and moves the
//! cost to once per machine:
//!
//! - A **template database**, `wired_test_{fp}_tpl`, holds the migrated
//!   schema. `{fp}` is a short fingerprint of the migration set and the
//!   connecting role (every migration's version and checksum plus the username,
//!   see `fingerprint`), so editing `migrations/` — or running the suite as
//!   another role, whose databases it could not reset — yields a different
//!   template, and this role's idle databases of the old fingerprint are
//!   dropped (ones another suite is still connected to are left for it). The
//!   first caller in a process builds it under a session advisory lock, so
//!   concurrent test binaries build — and wait for — exactly one template.
//! - **Slot databases**, `wired_test_{fp}_{n}`, are copies of the template
//!   made with `CREATE DATABASE … TEMPLATE …` on a slot's first use. That cost
//!   is paid about once per concurrent test thread, not once per test.
//! - A slot is **leased** with `pg_try_advisory_lock(SLOT_LOCK_CLASS, n)` for
//!   `n = 0, 1, 2, …` on a dedicated connection to the maintenance database.
//!   The session holding that lock owns the slot, so the lease lives exactly
//!   as long as [`TestDb`] does: dropping the fixture (or crashing the test)
//!   closes the connection and the server frees the slot. Nothing is cleaned
//!   up asynchronously, and no slot can leak. Slots are handed out lowest
//!   first and grow on demand, so the pool self-sizes to the number of test
//!   threads running at once — including the threads of test binaries running
//!   concurrently.
//! - A slot is **reset** when it is leased, not when it is released: every
//!   table of schema `public` except the migration bookkeeping table is
//!   emptied with `TRUNCATE … RESTART IDENTITY CASCADE`, and every standalone
//!   sequence (one no column owns) is restarted. The migrations insert no
//!   rows, so an emptied schema and a freshly migrated one are the same
//!   thing, and a test never sees another test's data.
//!
//! Tests therefore ask for a database instead of declaring one:
//!
//! ```no_run
//! use wired_be::test_db::test_db;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let db = test_db().await?;
//! let pool = db.pool();
//! // `pool: &PgPool` — pass it to the model functions, or clone it for the
//! // router. `db` holds the slot lease: keep it alive as long as it is used.
//! assert!(pool.size() > 0, "the slot pool is open");
//! # Ok(())
//! # }
//! ```
//!
//! The database named in `DATABASE_URL` is only ever used to run the
//! `pg_database` lookups and the `CREATE`/`DROP DATABASE` statements above;
//! like `#[sqlx::test]`, the fixture reads the environment but never mutates
//! it, and no test touches that database's own schema.

use std::{error::Error, fmt, str::FromStr as _, time::Duration};

use sqlx::{
    Connection as _, Error as SqlxError, PgConnection, PgPool, migrate::MigrateError,
    postgres::PgConnectOptions,
};
use tokio::sync::OnceCell;

mod databases;
mod reset;
mod slots;
mod template;
#[cfg(test)]
mod test_support;

use self::{
    slots::{lease_slot, open_slot, slot_name, template_tables},
    template::{ensure_template, migrator_fingerprint, template_name},
};
#[cfg(doc)]
use crate::MIGRATOR;
use crate::config::DATABASE_URL_VAR;

/// The prefix every database this module owns carries.
const DB_PREFIX: &str = "wired_test_";
/// The part of a template database's name after its fingerprint.
const TEMPLATE_SUFFIX: &str = "_tpl";
/// The advisory-lock key of the template build, as the ASCII bytes of
/// `wiredtpl`: one session at a time may create, migrate or prune.
const TEMPLATE_LOCK_KEY: i64 = 0x7769_7265_6474_706c;
/// The first half of every slot lease key, as the ASCII bytes of `wire`.
/// Lease keys are the pair `(SLOT_LOCK_CLASS, slot)`, so they cannot collide
/// with [`TEMPLATE_LOCK_KEY`], which is locked as a single key.
const SLOT_LOCK_CLASS: i32 = 0x7769_7265;
/// The highest slot number ever leased. Leases are held by live sessions
/// only, so this is a safety net against an unbounded scan rather than a real
/// cap: reaching it needs more simultaneous test threads than a machine has
/// cores.
const MAX_SLOTS: i32 = 128;
/// Connections a slot's pool opens at most — `#[sqlx::test]`'s own cap, so
/// one test still cannot take the whole server.
const SLOT_MAX_CONNECTIONS: u32 = 5;
/// How long an idle connection in a slot's pool waits before the pool closes
/// it, as in `#[sqlx::test]`: the server's connection count tracks the tests
/// that are running, not the ones that merely started.
const SLOT_IDLE_TIMEOUT: Duration = Duration::from_secs(1);
/// The schema whose tables a slot reset empties, and the only schema the
/// migrations create in.
const PUBLIC_SCHEMA: &str = "public";
/// The migration bookkeeping table: it is what makes a template's schema
/// known-good, so the reset leaves it alone.
const MIGRATIONS_TABLE: &str = "_sqlx_migrations";
/// How many bytes of the migration-set digest a fingerprint uses. Eight gives
/// a 16-hex-character name with a collision chance no test run will ever meet.
const FINGERPRINT_BYTES: usize = 8;

/// The template prepared in this process, by fingerprint: `OnceCell` makes
/// the concurrent callers of one test binary build at most one template and
/// wait for it.
static TEMPLATE: OnceCell<String> = OnceCell::const_new();
/// The tables the prepared template has: what a leased slot must still have,
/// and the reason to rebuild a slot whose schema a test changed. Read once per
/// process, like the template itself.
static TEMPLATE_TABLES: OnceCell<Vec<String>> = OnceCell::const_new();

/// A checked-out test database: a slot's [`PgPool`] plus the lease that keeps
/// that slot to this test.
///
/// The pool is reached through [`pool`](Self::pool) because the fixture owns
/// both it and the lease connection: dropping `TestDb` closes both, which
/// frees the slot for the next test. Keep it alive as long as the pool is
/// used.
#[derive(Debug)]
pub struct TestDb {
    /// The pool over this slot's database: the migrated schema of the
    /// fingerprint this test binary was built with, emptied of every row.
    pool: PgPool,
    /// The maintenance-database connection holding
    /// `pg_try_advisory_lock(SLOT_LOCK_CLASS, slot)`: the slot lease, owned by
    /// the session and released when this field drops and its socket closes.
    /// Named with a leading underscore because nothing ever reads it.
    _lock: PgConnection,
}

impl TestDb {
    /// The pool over the leased slot database.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Why no test database slot could be handed out.
#[derive(Debug)]
pub enum TestDbError {
    /// `DATABASE_URL` is unset or empty, so there is no maintenance database
    /// to lease slots from.
    MissingUrl,
    /// A database call failed: opening the maintenance connection, building
    /// the template, leasing or resetting a slot — including a
    /// `DATABASE_URL` that is not a PostgreSQL URL.
    Database(Box<SqlxError>),
    /// An invariant of the slot pool was exhausted or broken: no slot free
    /// below `MAX_SLOTS`, or an advisory unlock that reported `false`.
    Setup(String),
}

impl fmt::Display for TestDbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingUrl => write!(f, "{DATABASE_URL_VAR} is not set"),
            Self::Database(err) => write!(f, "test database setup failed: {err}"),
            Self::Setup(msg) => write!(f, "test database slots are unavailable: {msg}"),
        }
    }
}

impl Error for TestDbError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MissingUrl | Self::Setup(_) => None,
            Self::Database(err) => Some(err.as_ref()),
        }
    }
}

impl From<SqlxError> for TestDbError {
    fn from(err: SqlxError) -> Self {
        Self::Database(Box::new(err))
    }
}

impl From<MigrateError> for TestDbError {
    fn from(err: MigrateError) -> Self {
        Self::Database(Box::new(SqlxError::Migrate(Box::new(err))))
    }
}

/// Checks out a private, fully migrated, empty test database.
///
/// The returned [`TestDb`] holds a slot lease for as long as it lives, and the
/// slot was reset before it was handed over: see the [module docs](self) for
/// how the pool of slots works. The first call in a process also prepares the
/// template database for the current migration set, the one expensive step —
/// all migrations once per machine, not once per test.
///
/// # Errors
///
/// Returns [`TestDbError::MissingUrl`] if `DATABASE_URL` is unset,
/// [`TestDbError::Database`] if a call to the maintenance or slot database
/// fails, and [`TestDbError::Setup`] if no slot is free below `MAX_SLOTS`.
pub async fn test_db() -> Result<TestDb, TestDbError> {
    let maintenance = maintenance_options()?;
    let fingerprint = migrator_fingerprint(maintenance.get_username());
    ensure_template(&maintenance, &fingerprint).await?;
    let template = template_name(&fingerprint);
    let schema = template_tables(&maintenance, &template).await?;

    let mut lease = PgConnection::connect_with(&maintenance).await?;
    let slot = lease_slot(&mut lease).await?;
    let database = slot_name(&fingerprint, slot);
    let options = maintenance.database(&database);
    let pool = open_slot(&mut lease, &options, &database, &template, &schema).await?;
    Ok(TestDb { pool, _lock: lease })
}

/// The options of the database named in `DATABASE_URL`: the maintenance
/// database slots are created in, never a test target itself.
///
/// Read through [`dotenvy`], like `#[sqlx::test]` read it, so a `.env` file
/// works and the process environment is left alone.
///
/// # Errors
///
/// Returns [`TestDbError::MissingUrl`] if `DATABASE_URL` is unset or empty and
/// [`TestDbError::Database`] if it is not a PostgreSQL URL.
fn maintenance_options() -> Result<PgConnectOptions, TestDbError> {
    let url = dotenvy::var(DATABASE_URL_VAR)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or(TestDbError::MissingUrl)?;
    Ok(PgConnectOptions::from_str(&url)?)
}
