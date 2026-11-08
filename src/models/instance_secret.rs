//! The `instance_secrets` table: secret instance-wide values stored under a
//! stable name.
//!
//! Today it holds the Web Push VAPID key pair (see
//! [`crate::services::vapid`]); later tickets may add more names to the same
//! table. This module is the whole data layer for the table: [`get`] reads one
//! name's value, [`set`] writes (creating or replacing) it and
//! [`insert_if_absent`] seeds one without ever clobbering a stored value —
//! the shape a lost generation race needs. Reference: Pleroma keeps the same
//! material in runtime config (`config/runtime.exs`, the
//! `:web_push_encryption, :vapid_details` keyword); here each value is a row
//! so the key survives restarts and can be read without the process
//! environment.

use sqlx::PgPool;
use tracing::debug;

use crate::error::AppResult;

/// [`get`]: one name's stored value, if the row exists.
const GET_SQL: &str = "SELECT value FROM instance_secrets WHERE name = $1";

/// [`set`]: Pleroma-style upsert — a first write inserts, every later one
/// replaces the value.
const SET_SQL: &str = "INSERT INTO instance_secrets (name, value) VALUES ($1, $2)
 ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value";

/// [`insert_if_absent`]: an insert that yields to an existing row instead of
/// replacing it; [`set`] is the clobbering counterpart.
const INSERT_IF_ABSENT_SQL: &str =
    "INSERT INTO instance_secrets (name, value) VALUES ($1, $2) ON CONFLICT (name) DO NOTHING";

/// Reads the value stored under `name`, `None` when no row carries it.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get(pool: &PgPool, name: &str) -> AppResult<Option<String>> {
    let value = sqlx::query_scalar::<_, String>(GET_SQL)
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(value)
}

/// Stores `value` under `name`, creating the row or replacing an earlier
/// value: the config-override write, where the operator's material is the
/// last word.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set(pool: &PgPool, name: &str, value: &str) -> AppResult<()> {
    let written = sqlx::query(SET_SQL)
        .bind(name)
        .bind(value)
        .execute(pool)
        .await?;
    debug!(
        name,
        rows = written.rows_affected(),
        "instance secret written"
    );
    Ok(())
}

/// Stores `value` under `name` unless a row already carries one, reporting
/// whether this call wrote. A lost race against a concurrent first use
/// answers `false` and leaves the winner's value alone.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn insert_if_absent(pool: &PgPool, name: &str, value: &str) -> AppResult<bool> {
    let written = sqlx::query(INSERT_IF_ABSENT_SQL)
        .bind(name)
        .bind(value)
        .execute(pool)
        .await?;
    Ok(written.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`get`] answers `None` for a name the table never held.
    #[tokio::test]
    async fn get_of_unknown_name_is_none() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let value = get(pool, "vapid_private")
            .await
            .expect("the lookup reaches the database");
        assert_eq!(value, None, "a fresh table holds no secret");
    }

    /// [`set`] then [`get`] round-trips a value, and a second [`set`]
    /// replaces it: one row per name, the last write's value.
    #[tokio::test]
    async fn set_then_get_round_trips_and_overwrites() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        set(pool, "vapid_public", "first-value")
            .await
            .expect("the first write inserts");
        set(pool, "vapid_public", "second-value")
            .await
            .expect("the second write upserts");

        let value = get(pool, "vapid_public")
            .await
            .expect("the lookup reaches the database")
            .expect("the name was written");
        assert_eq!(value, "second-value", "the last write is the value");

        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM instance_secrets")
            .fetch_one(pool)
            .await
            .expect("the count runs");
        assert_eq!(rows.0, 1, "the upsert keeps one row per name");
    }

    /// [`insert_if_absent`] writes an absent name and reports it, and yields
    /// to a stored value without touching it.
    #[tokio::test]
    async fn insert_if_absent_writes_once_and_never_clobbers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        assert!(
            insert_if_absent(pool, "vapid_private", "generated")
                .await
                .expect("the first insert runs"),
            "an absent name is written"
        );
        assert!(
            !insert_if_absent(pool, "vapid_private", "loser of the race")
                .await
                .expect("the second insert runs"),
            "a stored name is left alone"
        );
        assert_eq!(
            get(pool, "vapid_private")
                .await
                .expect("the lookup runs")
                .expect("the row exists"),
            "generated",
            "the winner's value stands"
        );
    }

    /// Names are independent: writing one never touches another.
    #[tokio::test]
    async fn names_do_not_clobber_each_other() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        set(pool, "one", "1").await.expect("the first name writes");
        set(pool, "two", "2").await.expect("the second name writes");
        assert_eq!(
            get(pool, "one")
                .await
                .expect("the lookup runs")
                .expect("the row exists"),
            "1",
            "the first name keeps its value"
        );
    }
}
