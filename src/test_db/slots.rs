//! Slot databases: naming, leasing by advisory lock, creation from the
//! template and opening a slot's pool.

use sqlx::{
    Connection as _, PgConnection, PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

use super::{
    DB_PREFIX, MAX_SLOTS, PUBLIC_SCHEMA, SLOT_IDLE_TIMEOUT, SLOT_LOCK_CLASS, SLOT_MAX_CONNECTIONS,
    TEMPLATE_TABLES, TestDbError,
    databases::{create_database, database_exists, drop_database},
    reset::reset_slot,
};

/// The name of slot `slot` of fingerprint `fp`.
pub(super) fn slot_name(fp: &str, slot: i32) -> String {
    format!("{DB_PREFIX}{fp}_{slot}")
}

/// Leases the lowest free slot by taking its advisory lock on `conn` — the
/// caller's maintenance connection, which is therefore kept alive as the lease
/// itself.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if a lock call fails and
/// [`TestDbError::Setup`] if no slot up to [`MAX_SLOTS`] is free.
pub(super) async fn lease_slot(conn: &mut PgConnection) -> Result<i32, TestDbError> {
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
pub(super) async fn template_tables(
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
pub(super) async fn public_tables_of(
    options: PgConnectOptions,
) -> Result<Vec<String>, TestDbError> {
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
pub(super) async fn public_tables(conn: &mut PgConnection) -> Result<Vec<String>, TestDbError> {
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
pub(super) async fn open_slot(
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
pub(super) async fn create_slot(
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
pub(super) async fn prepare_slot(
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
pub(super) async fn slot_pool(options: &PgConnectOptions) -> Result<PgPool, TestDbError> {
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
pub(super) async fn carries_schema(
    pool: &PgPool,
    expected: &[String],
) -> Result<bool, TestDbError> {
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
pub(super) async fn close_pool(pool: PgPool) {
    pool.close().await;
}

#[cfg(test)]
mod tests {
    use super::super::{
        MIGRATIONS_TABLE, TestDb,
        template::owned_fingerprint,
        test_db,
        test_support::{
            database_of, holders, lease_released, leased_fingerprint, maintenance_conn, mark,
            marked, next_position, slot_of,
        },
    };
    use super::*;
    use crate::MIGRATOR;

    /// How many leases [`a_reused_slot_is_empty_and_restarted`] may be handed
    /// before the slot it waits for comes back to it. The suite's
    /// database-backed tests have grown, and a released slot is the lowest
    /// free number every other leasing test scans to first, so the walk back
    /// to it can take many passes; the tries are cheap (each leases, checks
    /// and frees again) and the assertion at the end is unchanged.
    const REUSE_TRIES: usize = 32;
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
}
