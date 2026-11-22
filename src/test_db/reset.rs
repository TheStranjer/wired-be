//! The slot reset: truncating the public tables, restarting sequences and
//! quoting identifiers.

use sqlx::{AssertSqlSafe, Executor, PgPool, Postgres};

use super::{MIGRATIONS_TABLE, PUBLIC_SCHEMA, TestDbError};

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
pub(super) async fn reset_slot(pool: &PgPool) -> Result<(), TestDbError> {
    truncate_tables(pool).await?;
    restart_sequences(pool).await
}

/// `TRUNCATE TABLE <every public table but the bookkeeping one> RESTART
/// IDENTITY CASCADE`, in one statement so the reset is a single round trip.
///
/// # Errors
///
/// Returns [`TestDbError::Database`] if a lookup or statement fails.
pub(super) async fn truncate_tables(pool: &PgPool) -> Result<(), TestDbError> {
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
pub(super) async fn restart_sequences(pool: &PgPool) -> Result<(), TestDbError> {
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
pub(super) async fn execute_ddl<'e, E>(executor: E, sql: String) -> Result<(), TestDbError>
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
pub(super) fn quoted(identifier: &str) -> String {
    let escaped = identifier.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::super::{
        test_db,
        test_support::{mark, marked, next_position},
    };
    use super::*;

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

    /// An identifier with a quote in it stays one identifier.
    #[test]
    fn quoting_keeps_an_identifier_one_word() {
        assert_eq!(quoted("plain"), "\"plain\"");
        assert_eq!(quoted("two\"words"), "\"two\"\"words\"");
    }
}
