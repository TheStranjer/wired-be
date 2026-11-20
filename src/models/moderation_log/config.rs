//! The configuration-change entry: the changed pairs a settings write
//! records and the map it stores.

use serde_json::{Map, Value};
use sqlx::PgPool;

use super::{ModAction, ModerationLog, put, record};
use crate::{error::AppResult, models::user::User};

/// One changed entry of a [`config_change`] record: the whitelisted pair
/// the admin API wrote, and whether the write deleted it rather than
/// upserting it.
#[derive(Clone, Copy, Debug)]
pub struct ConfigChange<'a> {
    /// The configuration group, colon-spelled (`:pleroma`).
    pub group: &'a str,
    /// The configuration key, colon-spelled (`:instance`, `:mrf`, …).
    pub key: &'a str,
    /// Whether the entry was deleted rather than written.
    pub delete: bool,
}

/// Records a `config_change`: `@admin changed config: :pleroma/:instance,
/// :pleroma/:mrf`.
///
/// One row per `POST /api/v1/pleroma/admin/config` that changed
/// something, its `configs` data array naming every pair and whether it
/// was written or deleted. The subject columns stay empty: the change is
/// the instance's own state, not a user's, so the pairs live in `data`
/// the way a report's or a relay target's does.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn config_change(
    pool: &PgPool,
    actor: &User,
    changes: &[ConfigChange<'_>],
) -> AppResult<ModerationLog> {
    let names: Vec<String> = changes
        .iter()
        .map(|change| format!("{}/{}", change.group, change.key))
        .collect();
    let message = format!("@{} changed config: {}", actor.nickname, names.join(", "));
    let mut data = Map::new();
    let configs = changes
        .iter()
        .map(config_change_map)
        .collect::<Vec<Value>>();
    put(&mut data, "configs", Value::Array(configs));
    record(
        pool,
        actor,
        ModAction {
            action: "config_change".to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// One `configs` entry of the [`config_change`] data map.
pub(super) fn config_change_map(change: &ConfigChange<'_>) -> Value {
    let mut entry = Map::new();
    put(&mut entry, "group", Value::String(change.group.to_owned()));
    put(&mut entry, "key", Value::String(change.key.to_owned()));
    put(&mut entry, "delete", Value::Bool(change.delete));
    Value::Object(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::moderation_log::test_support::stored;

    /// The `config_change` entry: the sentence names every changed pair
    /// in order, the `configs` data array spells each as
    /// `{group, key, delete}`, and the subject columns stay empty — the
    /// instance itself is what changed.
    #[tokio::test]
    async fn config_change_lists_the_changed_pairs() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let entry = config_change(
            &pool,
            &actor,
            &[
                ConfigChange {
                    group: ":pleroma",
                    key: ":instance",
                    delete: false,
                },
                ConfigChange {
                    group: ":pleroma",
                    key: ":mrf",
                    delete: true,
                },
            ],
        )
        .await
        .expect("config_change records");
        assert_eq!(
            entry.message, "@mod changed config: :pleroma/:instance, :pleroma/:mrf",
            "the sentence names the pairs, in request order"
        );
        assert_eq!(entry.action, "config_change", "the action column");
        assert!(
            entry.subject_ids.is_empty() && entry.subject_nicknames.is_empty(),
            "no user is the subject of a config change"
        );
        let configs = entry
            .data
            .get("configs")
            .and_then(Value::as_array)
            .expect("the data names its configs");
        assert_eq!(
            configs,
            &vec![
                serde_json::json!({"group": ":pleroma", "key": ":instance", "delete": false}),
                serde_json::json!({"group": ":pleroma", "key": ":mrf", "delete": true}),
            ],
            "each pair with its write-or-delete flag"
        );
    }
}
