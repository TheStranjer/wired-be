//! The SQL access to the `config_entries` table: reads, writes and
//! removal of one entry, and the subkey names a write touches.

use serde_json::Value;
use sqlx::PgPool;
use tracing::debug;

use super::ConfigEntry;
#[cfg(doc)]
use super::RuntimeConfig;
use super::wire::wire_key;
use crate::error::AppResult;

/// [`stored`]: the one row's JSON value.
pub(super) const STORED_SQL: &str =
    "SELECT value FROM config_entries WHERE group_name = $1 AND key = $2";

/// [`all`]: every row, stably ordered by the composite primary key.
const ALL_SQL: &str = "SELECT group_name, key, value, inserted_at, updated_at FROM config_entries
     ORDER BY group_name, key";

/// [`write`]: insert-or-replace, restamping `updated_at` on the way
/// through — the shape Pleroma's `update_or_create` ends in.
const WRITE_SQL: &str = "INSERT INTO config_entries (group_name, key, value)
     VALUES ($1, $2, $3)
     ON CONFLICT (group_name, key)
     DO UPDATE SET value = EXCLUDED.value, updated_at = now()";

/// [`remove`]: the whole row.
const REMOVE_SQL: &str = "DELETE FROM config_entries WHERE group_name = $1 AND key = $2";

/// Reads the stored value of one entry, in storage form; `None` while the
/// store holds no row for the pair.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn stored(db: &PgPool, group: &str, key: &str) -> AppResult<Option<Value>> {
    let value = sqlx::query_scalar::<_, Value>(STORED_SQL)
        .bind(group)
        .bind(key)
        .fetch_optional(db)
        .await?;
    Ok(value)
}

/// Every stored row, ordered by `(group, key)`.
///
/// # Errors
///
/// As [`stored`].
pub async fn all(db: &PgPool) -> AppResult<Vec<ConfigEntry>> {
    let rows = sqlx::query_as::<_, ConfigEntry>(ALL_SQL)
        .fetch_all(db)
        .await?;
    Ok(rows)
}

/// Upserts `value` (storage form) for the pair verbatim — the merge of
/// overlapping subkeys is [`RuntimeConfig::set`]'s business.
///
/// # Errors
///
/// As [`stored`].
pub async fn write(db: &PgPool, group: &str, key: &str, value: &Value) -> AppResult<()> {
    let written = sqlx::query(WRITE_SQL)
        .bind(group)
        .bind(key)
        .bind(value)
        .execute(db)
        .await?;
    debug!(
        group,
        key,
        rows = written.rows_affected(),
        "config entry written"
    );
    Ok(())
}

/// Deletes the pair's whole row, answering whether one was there.
///
/// # Errors
///
/// As [`stored`].
pub async fn remove(db: &PgPool, group: &str, key: &str) -> AppResult<bool> {
    let gone = sqlx::query(REMOVE_SQL)
        .bind(group)
        .bind(key)
        .execute(db)
        .await?;
    Ok(gone.rows_affected() > 0)
}

/// The `db` field of a stored value: its subkey names when it is a
/// keyword list (object), the entry's own key when it is a plain value,
/// all colon-prefixed — Pleroma's `ConfigDB.get_db_keys`.
#[must_use]
pub fn db_keys(key: &str, value: &Value) -> Vec<String> {
    match value {
        Value::Object(map) => map.keys().map(|name| wire_key(name)).collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) => {
            vec![wire_key(key)]
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// [`db_keys`] lists stored subkeys for a keyword value and the
    /// entry's own key for a plain one (Pleroma's `get_db_keys`).
    #[test]
    fn db_keys_name_the_stored_subkeys_or_the_key() {
        assert_eq!(
            db_keys(":instance", &json!({"name": "Wired", "email": ""})),
            vec![":email".to_owned(), ":name".to_owned()],
            "stored subkeys, colon-prefixed, in key order"
        );
        assert_eq!(
            db_keys("media_proxy", &json!([1])),
            vec![":media_proxy".to_owned()],
            "a bare entry key gains the colon"
        );
        assert_eq!(
            db_keys(":media_proxy", &json!(true)),
            vec![":media_proxy".to_owned()],
            "a plain value names its entry"
        );
    }
}
