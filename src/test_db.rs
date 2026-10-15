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
//!   template, and this role's databases of the old fingerprint are dropped. The first caller in a process builds it under a
//!   session advisory lock, so concurrent test binaries build — and wait for —
//!   exactly one template.
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

use sha2::{Digest as _, Sha256};
use sqlx::{
    AssertSqlSafe, Connection as _, Error as SqlxError, Executor, PgConnection, PgPool, Postgres,
    migrate::MigrateError,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tokio::sync::OnceCell;

use crate::{MIGRATOR, config::DATABASE_URL_VAR};

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

/// The fingerprint of the migrations this binary embeds and of `owner`, the
/// role it connects as: every migration's version and checksum, in order.
fn migrator_fingerprint(owner: &str) -> String {
    let migrations = MIGRATOR
        .migrations
        .iter()
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect::<Vec<_>>();
    fingerprint(&migrations, owner)
}

/// The 16-hex-character fingerprint of a migration set owned by `owner`, given
/// as its `(version, checksum)` pairs in order.
///
/// The fingerprint names the template and its slots, so the same migrations
/// always reuse the same databases and changed migrations never do. The owner
/// is part of it because a slot may only be reset and dropped by the role that
/// owns its database: a suite run under another role (CI's database user is
/// not a developer's) must not inherit databases it cannot maintain.
fn fingerprint(migrations: &[(i64, Vec<u8>)], owner: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(owner.as_bytes());
    for (version, checksum) in migrations {
        hasher.update(version.to_le_bytes());
        hasher.update(checksum);
    }
    let mut word: u64 = 0;
    for byte in hasher.finalize().iter().copied().take(FINGERPRINT_BYTES) {
        word = (word << 8) | u64::from(byte);
    }
    format!("{word:016x}")
}

/// The name of the template database of fingerprint `fp`.
fn template_name(fp: &str) -> String {
    format!("{DB_PREFIX}{fp}{TEMPLATE_SUFFIX}")
}

/// The name of slot `slot` of fingerprint `fp`.
fn slot_name(fp: &str, slot: i32) -> String {
    format!("{DB_PREFIX}{fp}_{slot}")
}

/// The fingerprint a database name of this module's own carries: the part
/// between [`DB_PREFIX`] and the last `_`, which is the slot number or
/// [`TEMPLATE_SUFFIX`]. `None` for a name this module does not own.
fn owned_fingerprint(database: &str) -> Option<&str> {
    database
        .strip_prefix(DB_PREFIX)
        .and_then(|rest| rest.rsplit_once('_'))
        .map(|(fp, _suffix)| fp)
}

/// Prepares the template database of fingerprint `fp`, at most once per
/// process: the first caller builds it, later callers of the same fingerprint
/// — including this process' other test threads — reuse it.
///
/// # Errors
///
/// Forwards whatever [`prepare_template`] returns.
async fn ensure_template(maintenance: &PgConnectOptions, fp: &str) -> Result<(), TestDbError> {
    let _prepared = TEMPLATE
        .get_or_try_init(|| prepare_template(maintenance.clone(), fp.to_owned()))
        .await?;
    Ok(())
}

/// Builds the template under the template advisory lock, remembering which
/// fingerprint was prepared.
///
/// # Errors
///
/// Forwards [`prepare_under_lock`]; the lock is released either way, and an
/// unlock that fails on the success path is reported instead.
async fn prepare_template(
    maintenance: PgConnectOptions,
    fp: String,
) -> Result<String, TestDbError> {
    let mut conn = PgConnection::connect_with(&maintenance).await?;
    let _locked = sqlx::query("SELECT pg_advisory_lock($1::bigint)")
        .bind(TEMPLATE_LOCK_KEY)
        .execute(&mut conn)
        .await?;
    let prepared = prepare_under_lock(&mut conn, &maintenance, &fp).await;
    match prepared {
        Ok(()) => {
            release_template_lock(&mut conn).await?;
            Ok(fp)
        }
        Err(err) => {
            let _released = release_template_lock(&mut conn).await;
            Err(err)
        }
    }
}

/// The work done while holding the template lock: create and migrate the
/// template if it is not there yet, then drop every database of a fingerprint
/// other than `fp` — the leftovers of earlier migration sets.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if any statement fails.
async fn prepare_under_lock(
    conn: &mut PgConnection,
    maintenance: &PgConnectOptions,
    fp: &str,
) -> Result<(), TestDbError> {
    let template = template_name(fp);
    if !database_exists(conn, &template).await? {
        create_database(conn, &template, "template1").await?;
        migrate_template(maintenance, &template).await?;
    }
    drop_stale_databases(conn, fp).await
}

/// Runs [`MIGRATOR`] against a fresh connection to database `name` and closes
/// that connection: the template must have no session attached before it can
/// be copied, and the maintenance connection `name` was created on cannot see
/// the new database's schema.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if connecting, migrating or closing
/// fails.
async fn migrate_template(maintenance: &PgConnectOptions, name: &str) -> Result<(), TestDbError> {
    let options = maintenance.clone().database(name);
    let mut conn = PgConnection::connect_with(&options).await?;
    MIGRATOR.run(&mut conn).await?;
    conn.close().await.map_err(Into::into)
}

/// Releases the template lock, reporting an unlock the server refused.
///
/// # Errors
///
/// Returns [`TestDbError::Setup`] if `pg_advisory_unlock` reports `false` and
/// [`TestDbError::Database`] if the call itself fails.
async fn release_template_lock(conn: &mut PgConnection) -> Result<(), TestDbError> {
    let released: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1::bigint)")
        .bind(TEMPLATE_LOCK_KEY)
        .fetch_one(&mut *conn)
        .await?;
    if released {
        Ok(())
    } else {
        Err(TestDbError::Setup(
            "the template lock was not held".to_owned(),
        ))
    }
}

/// Drops every database of this fingerprint's own prefix whose fingerprint is
/// not `fp`.
///
/// `WITH (FORCE)` ends the sessions still reading a stale slot — the test
/// binaries of an older build, which cannot be holding a lease of this
/// fingerprint's slots — so editing `migrations/` between two runs leaves no
/// databases behind. Only databases the connecting role owns are candidates:
/// another role's leftovers are not this role's to drop, and cost nothing but
/// disk to leave. The maintenance database is never dropped, however it is
/// named.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if a lookup or drop fails.
async fn drop_stale_databases(conn: &mut PgConnection, fp: &str) -> Result<(), TestDbError> {
    let current = current_database(conn).await?;
    let owned: Vec<String> = sqlx::query_scalar(
        "SELECT datname FROM pg_database \
          WHERE starts_with(datname, $1) \
            AND pg_get_userbyid(datdba) = current_user \
          ORDER BY datname",
    )
    .bind(DB_PREFIX)
    .fetch_all(&mut *conn)
    .await?;
    for database in owned {
        let is_stale = owned_fingerprint(&database).is_some_and(|got| got != fp);
        if is_stale && database != current {
            drop_database(conn, &database).await?;
        }
    }
    Ok(())
}

/// The name of the database `conn` is connected to.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if the lookup fails.
async fn current_database(conn: &mut PgConnection) -> Result<String, TestDbError> {
    Ok(sqlx::query_scalar::<_, String>("SELECT current_database()")
        .fetch_one(&mut *conn)
        .await?)
}

/// Whether `conn`'s server has a database named `name`.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if the lookup fails.
async fn database_exists(conn: &mut PgConnection, name: &str) -> Result<bool, TestDbError> {
    let found: Option<String> =
        sqlx::query_scalar("SELECT datname FROM pg_database WHERE datname = $1")
            .bind(name)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(found.is_some())
}

/// `CREATE DATABASE <name> TEMPLATE <template>`.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if the statement fails, e.g. because the
/// template is still in use.
async fn create_database(
    conn: &mut PgConnection,
    name: &str,
    template: &str,
) -> Result<(), TestDbError> {
    let sql = format!(
        "CREATE DATABASE {} TEMPLATE {}",
        quoted(name),
        quoted(template)
    );
    execute_ddl(conn, sql).await
}

/// `DROP DATABASE IF EXISTS <name> WITH (FORCE)`.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if the statement fails.
async fn drop_database(conn: &mut PgConnection, name: &str) -> Result<(), TestDbError> {
    let sql = format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", quoted(name));
    execute_ddl(conn, sql).await
}

/// Leases the lowest free slot by taking its advisory lock on `conn` — the
/// caller's maintenance connection, which is therefore kept alive as the lease
/// itself.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if a lock call fails and
/// [`TestDbError::Setup`] if no slot up to [`MAX_SLOTS`] is free.
async fn lease_slot(conn: &mut PgConnection) -> Result<i32, TestDbError> {
    let mut slot: i32 = 0;
    while slot <= MAX_SLOTS {
        let leased: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1::int, $2::int)")
            .bind(SLOT_LOCK_CLASS)
            .bind(slot)
            .fetch_one(&mut *conn)
            .await?;
        if leased {
            return Ok(slot);
        }
        slot = slot.checked_add(1).ok_or_else(|| {
            TestDbError::Setup(format!("slot numbers above {MAX_SLOTS} overflow"))
        })?;
    }
    Err(TestDbError::Setup(format!(
        "all {} test database slots are leased",
        MAX_SLOTS + 1
    )))
}

/// The tables of the template database `template`, read once per process:
/// the schema every leased slot must still carry.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if the template cannot be read.
async fn template_tables(
    maintenance: &PgConnectOptions,
    template: &str,
) -> Result<Vec<String>, TestDbError> {
    let options = maintenance.clone().database(template);
    let tables = TEMPLATE_TABLES
        .get_or_try_init(|| public_tables_of(options))
        .await?;
    Ok(tables.clone())
}

/// The `public` table names of database `options`, in name order.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if the database cannot be read.
async fn public_tables_of(options: PgConnectOptions) -> Result<Vec<String>, TestDbError> {
    let mut conn = PgConnection::connect_with(&options).await?;
    let tables = public_tables(&mut conn).await?;
    conn.close().await?;
    Ok(tables)
}

/// The `public` table names `conn`'s database has, in name order.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if the catalog cannot be read.
async fn public_tables(conn: &mut PgConnection) -> Result<Vec<String>, TestDbError> {
    Ok(sqlx::query_scalar(
        "SELECT tablename FROM pg_tables WHERE schemaname = $1 ORDER BY tablename",
    )
    .bind(PUBLIC_SCHEMA)
    .fetch_all(&mut *conn)
    .await?)
}

/// Hands over a pool over slot database `database`, ready for a test: the copy
/// made if the server has none, its rows truncated away, and the copy made
/// again if it turned out unusable.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if any statement fails and
/// [`TestDbError::Setup`] if even the rebuilt copy is unusable.
async fn open_slot(
    lease: &mut PgConnection,
    options: &PgConnectOptions,
    database: &str,
    template: &str,
    expected: &[String],
) -> Result<PgPool, TestDbError> {
    create_slot(lease, database, template).await?;
    match prepare_slot(options, database, expected).await {
        Ok(pool) => Ok(pool),
        Err(err) => {
            // A copy this session cannot use: half-created by a process that
            // died inside its `CREATE DATABASE`, or of a schema some earlier
            // test changed and a `TRUNCATE` cannot put back. The lease makes
            // rebuilding it safe, and the fresh copy needs no second reset.
            let _unusable = err;
            drop_database(lease, database).await?;
            create_database(lease, database, template).await?;
            prepare_slot(options, database, expected).await
        }
    }
}

/// Creates `database` as a copy of `template` unless the server already has
/// it: the first lease of a slot is its only copy, the ~130ms the template
/// pays for, once per slot rather than once per test.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if the lookup or statement fails.
async fn create_slot(
    lease: &mut PgConnection,
    database: &str,
    template: &str,
) -> Result<(), TestDbError> {
    if database_exists(lease, database).await? {
        return Ok(());
    }
    create_database(lease, database, template).await
}

/// Opens the pool over an existing slot, makes sure it still has the
/// template's tables and empties it.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if the pool cannot be opened or a
/// statement fails, and [`TestDbError::Setup`] when the schema drifted.
async fn prepare_slot(
    options: &PgConnectOptions,
    database: &str,
    expected: &[String],
) -> Result<PgPool, TestDbError> {
    let pool = slot_pool(options).await?;
    if !carries_schema(&pool, expected).await? {
        close_pool(pool).await;
        return Err(TestDbError::Setup(format!(
            "{database} no longer holds the template's schema"
        )));
    }
    if let Err(err) = reset_slot(&pool).await {
        close_pool(pool).await;
        return Err(err);
    }
    Ok(pool)
}

/// The pool over a slot database, with `#[sqlx::test]`'s limits.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if the database cannot be opened.
async fn slot_pool(options: &PgConnectOptions) -> Result<PgPool, TestDbError> {
    Ok(PgPoolOptions::new()
        .max_connections(SLOT_MAX_CONNECTIONS)
        .idle_timeout(SLOT_IDLE_TIMEOUT)
        .connect_with(options.clone())
        .await?)
}

/// Whether `pool`'s database has exactly the tables `expected` names.
///
/// A slot only needs its rows gone to be as good as new; a table a test
/// dropped or created is schema drift, which the reset cannot undo and which
/// [`open_slot`] answers by copying the template again.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if the catalog cannot be read.
async fn carries_schema(pool: &PgPool, expected: &[String]) -> Result<bool, TestDbError> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables WHERE schemaname = $1 ORDER BY tablename",
    )
    .bind(PUBLIC_SCHEMA)
    .fetch_all(pool)
    .await?;
    Ok(tables == expected)
}

/// Closes `pool`: a slot database is a throwaway, and its connections have to
/// go before the next test can take the slot over.
async fn close_pool(pool: PgPool) {
    pool.close().await;
}

/// Empties every table of [`PUBLIC_SCHEMA`] except [`MIGRATIONS_TABLE`] and
/// restarts every standalone sequence in it, through `pool`.
///
/// This is what makes a slot as good as a freshly migrated database: the
/// migrations seed no rows, so afterwards the schema holds exactly what the
/// template's does — nothing — with identity columns back at their start.
/// `CASCADE` takes the rows of tables that reference these (they are in the
/// list anyway) and `RESTART IDENTITY` covers the sequences a column owns.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if a lookup or statement fails.
async fn reset_slot(pool: &PgPool) -> Result<(), TestDbError> {
    truncate_tables(pool).await?;
    restart_sequences(pool).await
}

/// `TRUNCATE TABLE <every public table but the bookkeeping one> RESTART
/// IDENTITY CASCADE`, in one statement so the reset is a single round trip.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if a lookup or statement fails.
async fn truncate_tables(pool: &PgPool) -> Result<(), TestDbError> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables WHERE schemaname = $1 AND tablename <> $2 \
           ORDER BY tablename",
    )
    .bind(PUBLIC_SCHEMA)
    .bind(MIGRATIONS_TABLE)
    .fetch_all(pool)
    .await?;
    if tables.is_empty() {
        return Ok(());
    }
    let targets = tables
        .iter()
        .map(|table| quoted(table))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("TRUNCATE TABLE {targets} RESTART IDENTITY CASCADE");
    execute_ddl(pool, sql).await
}

/// `ALTER SEQUENCE … RESTART` for every sequence in [`PUBLIC_SCHEMA`] no
/// column owns: `TRUNCATE … RESTART IDENTITY` restarts the owned ones and
/// cannot see these, and a test that took numbers from one would otherwise
/// hand a jumped counter to the next test. The shipped schema has none, so
/// this normally sends nothing at all.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if a lookup or statement fails.
async fn restart_sequences(pool: &PgPool) -> Result<(), TestDbError> {
    let sequences: Vec<String> = sqlx::query_scalar(
        "SELECT cls.relname FROM pg_class cls \
           JOIN pg_namespace nsp ON nsp.oid = cls.relnamespace \
          WHERE nsp.nspname = $1 AND cls.relkind = 'S' \
            AND NOT EXISTS ( \
                SELECT 1 FROM pg_depend dep \
                 WHERE dep.classid = 'pg_class'::regclass \
                   AND dep.objid = cls.oid AND dep.deptype = 'a') \
          ORDER BY cls.relname",
    )
    .bind(PUBLIC_SCHEMA)
    .fetch_all(pool)
    .await?;
    for sequence in &sequences {
        let sql = format!("ALTER SEQUENCE {} RESTART", quoted(sequence));
        execute_ddl(pool, sql).await?;
    }
    Ok(())
}

/// Runs one complete DDL statement written by this module.
///
/// The statement text is dynamic only in its identifiers, which are either
/// built from [`DB_PREFIX`] and a hex fingerprint or read back from the
/// catalog and passed through [`quoted`], so no input can reach the parser as
/// syntax: hence `AssertSqlSafe`. It runs as plain SQL rather than a prepared
/// statement because PostgreSQL cannot prepare `CREATE`/`DROP DATABASE`.
async fn execute_ddl<'e, E>(executor: E, sql: String) -> Result<(), TestDbError>
where
    E: Executor<'e, Database = Postgres>,
{
    let _applied = executor.execute(AssertSqlSafe(sql)).await?;
    Ok(())
}

/// `identifier` double-quoted for SQL, with embedded quotes doubled.
///
/// Database and table names come from the catalog or from this module's own
/// prefix and fingerprint; quoting keeps either out of the parser's way
/// whatever it contains.
fn quoted(identifier: &str) -> String {
    let escaped = identifier.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    /// How many leases [`a_reused_slot_is_empty_and_restarted`] may be handed
    /// before the slot it waits for comes back to it.
    const REUSE_TRIES: usize = 8;
    /// How long a released lease may take to disappear from `pg_locks`.
    const LEASE_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
    /// The pause between two looks at `pg_locks`.
    const LEASE_POLL: Duration = Duration::from_millis(20);

    /// The fingerprint the leases in this process are named for: the embedded
    /// migrations under the role `DATABASE_URL` connects as.
    fn leased_fingerprint() -> String {
        let options = maintenance_options().expect("DATABASE_URL is set for the test suite");
        migrator_fingerprint(options.get_username())
    }

    /// The name of the database `pool` is connected to.
    async fn database_of(pool: &PgPool) -> String {
        sqlx::query_scalar::<_, String>("SELECT current_database()")
            .fetch_one(pool)
            .await
            .expect("the slot answers its own name")
    }

    /// The slot number a slot database name ends with.
    fn slot_of(database: &str) -> i32 {
        database
            .rsplit_once('_')
            .and_then(|(_fp, slot)| slot.parse::<i32>().ok())
            .unwrap_or_else(|| panic!("{database} is not a slot database name"))
    }

    /// Puts a `hashtags` row named `tag` into `pool`: the marker a reset has
    /// to clear.
    async fn mark(pool: &PgPool, tag: &str) {
        let _inserted = sqlx::query("INSERT INTO hashtags (id, name) VALUES ($1, $2)")
            .bind(Uuid::now_v7())
            .bind(tag)
            .execute(pool)
            .await
            .expect("the marker tag inserts");
    }

    /// Whether the `hashtags` row named `tag` is still in `pool`.
    async fn marked(pool: &PgPool, tag: &str) -> bool {
        sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM hashtags WHERE name = $1)")
            .bind(tag)
            .fetch_one(pool)
            .await
            .expect("the marker tag is looked up")
    }

    /// The next `list_members.position` value: one while the identities are at
    /// their start, one more every time a test takes a number from them.
    async fn next_position(pool: &PgPool) -> i64 {
        sqlx::query_scalar(
            "SELECT nextval(pg_get_serial_sequence('public.list_members', 'position'))",
        )
        .fetch_one(pool)
        .await
        .expect("the position sequence answers")
    }

    /// A connection to the maintenance database, for looking at `pg_locks`.
    async fn maintenance_conn() -> PgConnection {
        let options = maintenance_options().expect("DATABASE_URL is set for the test suite");
        PgConnection::connect_with(&options)
            .await
            .expect("the maintenance database answers")
    }

    /// The sessions holding the lease of slot `slot`, as `conn` sees them.
    async fn holders(conn: &mut PgConnection, slot: i32) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks \
              WHERE locktype = 'advisory' \
                AND classid::bigint = $1 AND objid::bigint = $2",
        )
        .bind(i64::from(SLOT_LOCK_CLASS))
        .bind(i64::from(slot))
        .fetch_one(conn)
        .await
        .expect("pg_locks answers")
    }

    /// Waits until no session holds the lease of slot `slot` any more, which
    /// the server does as soon as the connection that took it is closed.
    async fn lease_released(slot: i32) -> bool {
        let mut conn = maintenance_conn().await;
        let started = std::time::Instant::now();
        loop {
            if holders(&mut conn, slot).await == 0 {
                return true;
            }
            if started.elapsed() >= LEASE_RELEASE_TIMEOUT {
                return false;
            }
            tokio::time::sleep(LEASE_POLL).await;
        }
    }

    /// Two leases held at the same time name different slot databases: the
    /// advisory lock is what makes a slot exclusive, and both names carry the
    /// fingerprint of this binary's migrations.
    #[tokio::test]
    async fn two_leases_at_once_are_different_slots() {
        let fp = leased_fingerprint();
        let first = test_db().await.expect("the first slot leases");
        let second = test_db().await.expect("the second slot leases");
        let first_db = database_of(first.pool()).await;
        let second_db = database_of(second.pool()).await;

        assert_ne!(first_db, second_db, "one slot cannot serve two leases");
        assert_eq!(
            owned_fingerprint(&first_db),
            Some(fp.as_str()),
            "{first_db} is a slot of this migration set"
        );
        assert_eq!(
            owned_fingerprint(&second_db),
            Some(fp.as_str()),
            "{second_db} is a slot of this migration set"
        );
    }

    /// Dropping the fixture closes the lease connection and the server frees
    /// the slot: nothing to clean up asynchronously, and nothing a crashed
    /// test can leak.
    #[tokio::test]
    async fn releasing_a_lease_frees_the_slot() {
        let db = test_db().await.expect("the slot leases");
        let slot = slot_of(&database_of(db.pool()).await);
        let mut conn = maintenance_conn().await;
        assert_eq!(holders(&mut conn, slot).await, 1, "the live lease is held");

        drop(db);
        assert!(
            lease_released(slot).await,
            "the slot is free once TestDb goes: the lease is its connection"
        );
    }

    /// Leases slots until `database` comes back to this test, and hands over
    /// the fixture holding it. Every slot passed on is released and waited
    /// for, so the retries cannot pile up new databases.
    ///
    /// # Panics
    ///
    /// If a lease or a release wait fails.
    async fn reacquire(database: &str) -> Option<TestDb> {
        for _try in 0..REUSE_TRIES {
            let next = test_db().await.expect("another slot leases");
            if database_of(next.pool()).await == database {
                return Some(next);
            }
            let passed_on = slot_of(&database_of(next.pool()).await);
            drop(next);
            assert!(
                lease_released(passed_on).await,
                "the slot we passed on is free again"
            );
        }
        None
    }

    /// A slot handed to a second test is empty again: the row the first test
    /// left is gone and the identity column is back at its start, which is
    /// what makes a reused slot equal to a freshly migrated one.
    #[tokio::test]
    async fn a_reused_slot_is_empty_and_restarted() {
        let first = test_db().await.expect("the first slot leases");
        let database = database_of(first.pool()).await;
        let slot = slot_of(&database);
        mark(first.pool(), "first-tenant").await;
        assert_eq!(
            next_position(first.pool()).await,
            1,
            "the identity starts at one"
        );
        drop(first);
        assert!(lease_released(slot).await, "the slot came free");

        let db = reacquire(&database)
            .await
            .expect("the freed slot is leased again");

        assert!(
            !marked(db.pool(), "first-tenant").await,
            "the previous tenant's row was truncated away"
        );
        assert_eq!(
            next_position(db.pool()).await,
            1,
            "its identities were restarted with it"
        );
    }

    /// The slot arrives with the whole migration set applied and no rows: the
    /// template's schema, emptied — which is what the reset promises.
    #[tokio::test]
    async fn a_slot_is_migrated_and_seeded_nothing() {
        let db = test_db().await.expect("the slot leases");
        let pool = db.pool();

        let applied: i64 = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations")
            .fetch_one(pool)
            .await
            .expect("the bookkeeping table survives the reset");
        let expected = i64::try_from(MIGRATOR.migrations.len())
            .expect("a migration count fits into the bookkeeping type");
        assert_eq!(applied, expected, "the template carries every migration");

        let tables: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_tables WHERE schemaname = $1 AND tablename <> $2",
        )
        .bind(PUBLIC_SCHEMA)
        .bind(MIGRATIONS_TABLE)
        .fetch_one(pool)
        .await
        .expect("the catalog answers");
        assert!(tables > 1, "the migrated schema has tables to reset");
        let sampled: [(&str, &str); 3] = [
            ("users", "SELECT count(*) FROM users"),
            ("statuses", "SELECT count(*) FROM statuses"),
            ("hashtags", "SELECT count(*) FROM hashtags"),
        ];
        for (table, sql) in sampled {
            let rows: i64 = sqlx::query_scalar(sql)
                .fetch_one(pool)
                .await
                .unwrap_or_else(|_| panic!("{table} is readable"));
            assert_eq!(rows, 0, "{table} starts out empty");
        }
    }

    /// Running the reset on a slot that has just been used empties it and
    /// restarts its identities — the very call a lease hands over.
    #[tokio::test]
    async fn resetting_a_slot_empties_it_and_restarts_identities() {
        let db = test_db().await.expect("the slot leases");
        mark(db.pool(), "before-the-reset").await;
        assert_eq!(next_position(db.pool()).await, 1, "the sequence starts");
        assert_eq!(next_position(db.pool()).await, 2, "and advances");

        reset_slot(db.pool()).await.expect("the reset runs");

        assert!(
            !marked(db.pool(), "before-the-reset").await,
            "the row is gone"
        );
        assert_eq!(
            next_position(db.pool()).await,
            1,
            "the sequence restarted with it"
        );
    }

    /// The fingerprint is a function of the migration set: the same pairs give
    /// the same name, and any change to versions, checksums or their order
    /// gives a different one, so an edited `migrations/` never reuses a
    /// template.
    #[test]
    fn the_fingerprint_follows_the_migration_set() {
        let base = vec![(1_i64, b"abc".to_vec())];
        assert_eq!(
            fingerprint(&base, "wired"),
            fingerprint(&[(1_i64, b"abc".to_vec())], "wired"),
            "the same set fingerprints the same"
        );
        let added = [(1_i64, b"abc".to_vec()), (2_i64, b"abc".to_vec())];
        let edited = [(1_i64, b"abd".to_vec())];
        let reordered = vec![(2_i64, b"abc".to_vec()), (1_i64, b"abc".to_vec())];
        assert_ne!(
            fingerprint(&base, "wired"),
            fingerprint(&added, "wired"),
            "a new migration changes it"
        );
        assert_ne!(
            fingerprint(&base, "wired"),
            fingerprint(&edited, "wired"),
            "an edited migration changes it"
        );
        assert_ne!(
            fingerprint(&added, "wired"),
            fingerprint(&reordered, "wired"),
            "the order of the set counts"
        );
        assert_ne!(
            fingerprint(&base, "wired"),
            fingerprint(&base, "postgres"),
            "another role's databases are not ours to reuse"
        );
        assert_eq!(
            fingerprint(&base, "wired").len(),
            FINGERPRINT_BYTES * 2,
            "a fingerprint is as wide as the module says"
        );
        assert_ne!(
            fingerprint(&base, "wired"),
            fingerprint(&[], "wired"),
            "no set is not the set"
        );
    }

    /// The embedded migration set fingerprints to the database names the
    /// module documents, and only its own names carry a fingerprint back.
    #[test]
    fn the_names_are_built_from_the_fingerprint() {
        let fp = migrator_fingerprint("wired");
        let again = migrator_fingerprint("wired");
        assert_eq!(fp, again, "it is the set and the role, not the call");
        assert_ne!(
            fp,
            migrator_fingerprint("postgres"),
            "the role the databases belong to counts"
        );
        let template = template_name(&fp);
        assert_eq!(template, format!("{DB_PREFIX}{fp}{TEMPLATE_SUFFIX}"));
        assert_eq!(slot_name(&fp, 7), format!("{DB_PREFIX}{fp}_7"));
        assert_eq!(
            owned_fingerprint(&template),
            Some(fp.as_str()),
            "the template names its fingerprint"
        );
        assert_eq!(
            owned_fingerprint(&slot_name(&fp, 12)),
            Some(fp.as_str()),
            "a two-digit slot does too"
        );
        assert_eq!(
            owned_fingerprint("postgres"),
            None,
            "a foreign name is not ours"
        );
        assert_eq!(
            owned_fingerprint(DB_PREFIX),
            None,
            "the bare prefix is not ours"
        );
    }

    /// An identifier with a quote in it stays one identifier.
    #[test]
    fn quoting_keeps_an_identifier_one_word() {
        assert_eq!(quoted("plain"), "\"plain\"");
        assert_eq!(quoted("two\"words"), "\"two\"\"words\"");
    }
}
