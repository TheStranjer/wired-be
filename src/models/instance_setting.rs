//! The `instance_settings` table: instance-wide settings stored as JSON
//! under a text key.
//!
//! Pleroma keeps these in its runtime configuration
//! (`Pleroma.Config`, e.g. the `:frontend_configurations` key the API
//! serves at `/api/pleroma/frontend_configurations`); here each one is a
//! row. This module is the whole data layer for the table: [`get`] reads
//! one key's value, [`set`] writes (creating or replacing) it. The
//! admin-changeable runtime configuration lives in the separate
//! whitelist-gated store, [`config_entries`](crate::services::runtime_config)
//! (WBE-206); the MRF chain reads from there, not from this table.

use serde_json::Value;
use sqlx::PgPool;
use tracing::debug;

use crate::error::AppResult;

/// [`get`]: one key's stored JSON value, if the row exists.
const GET_SQL: &str = "SELECT value FROM instance_settings WHERE key = $1";

/// [`set`]: Pleroma-style upsert — a first write inserts, every later one
/// replaces the value and restamps `updated_at`.
const SET_SQL: &str = "INSERT INTO instance_settings (key, value) VALUES ($1, $2)
 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()";

/// Reads the value stored under `key`, `None` when no row carries it.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get(pool: &PgPool, key: &str) -> AppResult<Option<Value>> {
    let value = sqlx::query_scalar::<_, Value>(GET_SQL)
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(value)
}

/// Stores `value` under `key`, creating the row or replacing an earlier
/// value, and restamps `updated_at`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set(pool: &PgPool, key: &str, value: &Value) -> AppResult<()> {
    let written = sqlx::query(SET_SQL)
        .bind(key)
        .bind(value)
        .execute(pool)
        .await?;
    debug!(
        key,
        rows = written.rows_affected(),
        "instance setting written"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// [`get`] answers `None` for a key the table never held.
    #[tokio::test]
    async fn get_of_unknown_key_is_none() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let value = get(&pool, "frontend_configurations")
            .await
            .expect("the lookup reaches the database");
        assert_eq!(value, None, "a fresh table holds no setting");
    }

    /// [`set`] then [`get`] round-trips a value, and a second [`set`]
    /// replaces it in place: one row per key, the last write's value.
    #[tokio::test]
    async fn set_then_get_round_trips_and_overwrites() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        set(&pool, "frontend_configurations", &json!({"a": 1}))
            .await
            .expect("the first write inserts");
        set(&pool, "frontend_configurations", &json!({"b": [2, "x"]}))
            .await
            .expect("the second write upserts");

        let value = get(&pool, "frontend_configurations")
            .await
            .expect("the lookup reaches the database")
            .expect("the key was written");
        assert_eq!(value, json!({"b": [2, "x"]}), "the last write is the value");

        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM instance_settings")
            .fetch_one(&pool)
            .await
            .expect("the count runs");
        assert_eq!(rows.0, 1, "the upsert keeps one row per key");
    }

    /// Keys are independent: writing one never touches another.
    #[tokio::test]
    async fn keys_do_not_clobber_each_other() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        set(&pool, "one", &json!(1))
            .await
            .expect("the first key writes");
        set(&pool, "two", &json!("2"))
            .await
            .expect("the second key writes");
        assert_eq!(
            get(&pool, "one")
                .await
                .expect("the lookup runs")
                .expect("the row exists"),
            json!(1),
            "the first key keeps its value"
        );
    }

    /// A write restamps `updated_at`: the column is the setting's own
    /// update time, not a fixed creation stamp.
    #[tokio::test]
    async fn set_restamps_updated_at() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        set(&pool, "when", &json!(true))
            .await
            .expect("the first write inserts");
        let first: (chrono::DateTime<chrono::Utc>,) =
            sqlx::query_as("SELECT updated_at FROM instance_settings WHERE key = 'when'")
                .fetch_one(&pool)
                .await
                .expect("the timestamp reads");
        // Move the clock's read-back behind the first stamp, then rewrite.
        let moved = sqlx::query(
            "UPDATE instance_settings SET updated_at = now() - interval '1 hour' WHERE key = 'when'",
        )
        .execute(&pool)
        .await
        .expect("the rewind runs");
        assert_eq!(moved.rows_affected(), 1, "the rewind hit the row");
        set(&pool, "when", &json!(false))
            .await
            .expect("the second write upserts");
        let second: (chrono::DateTime<chrono::Utc>,) =
            sqlx::query_as("SELECT updated_at FROM instance_settings WHERE key = 'when'")
                .fetch_one(&pool)
                .await
                .expect("the timestamp reads");
        assert!(
            second.0 > first.0,
            "the upsert restamped updated_at to a later time"
        );
    }
}
