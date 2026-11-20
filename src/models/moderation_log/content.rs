//! The content and federation entries: the status pair, the chat-message
//! delete, the relay pair, the instance delete, the emoji reload and the
//! backup.

use serde_json::{Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::data::{status_data, user_map};
use super::sentences::status_update_sentence;
use super::{ModAction, ModerationLog, put, record};
use crate::{error::AppResult, models::user::User};

/// Records a `status_update` of `status_id`, naming only the parts that
/// changed: `@admin updated status ##{id}, set sensitive: 'true',
/// visibility: 'private'`.
///
/// `None` for either part means untouched, and naming neither still reads
/// Pleroma's `set visibility: ''` fall-through; the subject map is its
/// `status_to_map`: `{type: "status", id}`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn status_update(
    pool: &PgPool,
    actor: &User,
    status_id: Uuid,
    sensitive: Option<bool>,
    visibility: Option<&str>,
) -> AppResult<ModerationLog> {
    let message = status_update_sentence(&actor.nickname, status_id, sensitive, visibility);
    record(
        pool,
        actor,
        ModAction {
            action: "status_update".to_owned(),
            subjects: Vec::new(),
            message,
            data: status_data(status_id, sensitive, visibility),
        },
    )
    .await
}

/// Records a `status_delete`: `@admin deleted status ##{status_id}`. The
/// deleted status may have no row left to name, so it rides in
/// `subject_id` as text, exactly like Pleroma's clause reads it.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn status_delete(
    pool: &PgPool,
    actor: &User,
    status_id: Uuid,
) -> AppResult<ModerationLog> {
    let message = format!("@{} deleted status ##{status_id}", actor.nickname);
    let mut data = Map::new();
    put(
        &mut data,
        "subject_id",
        Value::String(status_id.to_string()),
    );
    record(
        pool,
        actor,
        ModAction {
            action: "status_delete".to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records a `chat_message_delete`: `@admin deleted chat message ##{message_id}`.
///
/// A withdrawn chat message leaves no live row to name, so its id rides in
/// `subject_id` as text — the same shape [`status_delete`] gives a deleted
/// post, with the chat-message wording Pleroma's `get_log_entry_message`
/// clause spells.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn chat_message_delete(
    pool: &PgPool,
    actor: &User,
    message_id: Uuid,
) -> AppResult<ModerationLog> {
    let message = format!("@{} deleted chat message ##{message_id}", actor.nickname);
    let mut data = Map::new();
    put(
        &mut data,
        "subject_id",
        Value::String(message_id.to_string()),
    );
    record(
        pool,
        actor,
        ModAction {
            action: "chat_message_delete".to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records a `relay_follow`: `@admin followed relay: target`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn relay_follow(pool: &PgPool, actor: &User, target: &str) -> AppResult<ModerationLog> {
    relay_action(pool, actor, target, "relay_follow", "followed").await
}

/// Records a `relay_unfollow`: `@admin unfollowed relay: target`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn relay_unfollow(pool: &PgPool, actor: &User, target: &str) -> AppResult<ModerationLog> {
    relay_action(pool, actor, target, "relay_unfollow", "unfollowed").await
}

/// The two relay clauses share their sentence and their `target` key;
/// only the action name and the verb differ.
pub(super) async fn relay_action(
    pool: &PgPool,
    actor: &User,
    target: &str,
    action: &str,
    verb: &str,
) -> AppResult<ModerationLog> {
    let message = format!("@{} {verb} relay: {target}", actor.nickname);
    let mut data = Map::new();
    put(&mut data, "target", Value::String(target.to_owned()));
    record(
        pool,
        actor,
        ModAction {
            action: action.to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records a `delete_instance`: `@admin deleted instance: host`. The
/// subject is a domain, not an account, so it rides in `data.target` —
/// the same spot a relay target occupies.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn delete_instance(pool: &PgPool, actor: &User, host: &str) -> AppResult<ModerationLog> {
    let message = format!("@{} deleted instance: {host}", actor.nickname);
    let mut data = Map::new();
    put(&mut data, "target", Value::String(host.to_owned()));
    record(
        pool,
        actor,
        ModAction {
            action: "delete_instance".to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records a `reload_emoji`: `@admin reloaded emoji`. There is no subject
/// — the action resyncs the whole on-disk pack set — so beside the
/// actor/action/message keys [`record`] owns, the data map stays empty.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn reload_emoji(pool: &PgPool, actor: &User) -> AppResult<ModerationLog> {
    let message = format!("@{} reloaded emoji", actor.nickname);
    record(
        pool,
        actor,
        ModAction {
            action: "reload_emoji".to_owned(),
            subjects: Vec::new(),
            message,
            data: Map::new(),
        },
    )
    .await
}

/// Records a `create_backup`: `@admin requested account backup for @user`.
///
/// Pleroma's `AdminAPIController.create_backup` clause: the account whose
/// export was staged rides in `data.subject` (Pleroma's `subject` map) and
/// in the subject columns, since the action speaks about that one account.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn create_backup(pool: &PgPool, actor: &User, target: &User) -> AppResult<ModerationLog> {
    let message = format!(
        "@{} requested account backup for @{}",
        actor.nickname, target.nickname
    );
    let mut data = Map::new();
    put(&mut data, "subject", Value::Object(user_map(target)));
    record(
        pool,
        actor,
        ModAction {
            action: "create_backup".to_owned(),
            subjects: vec![target],
            message,
            data,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{models::moderation_log::test_support::stored, util::id::new_id};

    /// `status_update` and `status_delete` store their sentences and
    /// subject maps, and the relay pair stores its target.
    #[tokio::test]
    async fn status_and_relay_helpers_record_their_entries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let post = new_id();
        let changed = status_update(&pool, &actor, post, Some(true), Some("private"))
            .await
            .expect("status_update records");
        assert_eq!(
            changed.message,
            format!("@mod updated status ##{post}, set sensitive: 'true', visibility: 'private'"),
            "the status_update entry"
        );
        assert_eq!(
            changed
                .data
                .get("subject")
                .and_then(Value::as_object)
                .and_then(|inner| inner.get("type"))
                .and_then(Value::as_str),
            Some("status"),
            "the status subject map says what it is"
        );
        let deleted = status_delete(&pool, &actor, post)
            .await
            .expect("status_delete records");
        assert_eq!(
            deleted.message,
            format!("@mod deleted status ##{post}"),
            "the status_delete entry"
        );
        assert_eq!(
            deleted.data.get("subject_id").and_then(Value::as_str),
            Some(post.to_string().as_str()),
            "the deleted status rides in subject_id"
        );
        let followed = relay_follow(&pool, &actor, "https://relay.example")
            .await
            .expect("relay_follow records");
        assert_eq!(
            followed.message, "@mod followed relay: https://relay.example",
            "the relay_follow entry"
        );
        let unfollowed = relay_unfollow(&pool, &actor, "https://relay.example")
            .await
            .expect("relay_unfollow records");
        assert_eq!(
            unfollowed.message, "@mod unfollowed relay: https://relay.example",
            "the relay_unfollow entry"
        );
        assert_eq!(
            unfollowed.data.get("target").and_then(Value::as_str),
            Some("https://relay.example"),
            "the relay target rides in the data map"
        );
    }

    /// `chat_message_delete` stores Pleroma's chat-message sentence with the
    /// withdrawn message's id in `subject_id` — the shape the admin chat
    /// endpoint logs on every delete.
    #[tokio::test]
    async fn the_chat_message_delete_entry_states_its_subject() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let message = new_id();

        let deleted = chat_message_delete(&pool, &actor, message)
            .await
            .expect("chat_message_delete records");

        assert_eq!(deleted.action, "chat_message_delete", "the action column");
        assert_eq!(
            deleted.message,
            format!("@mod deleted chat message ##{message}"),
            "Pleroma's chat_message_delete sentence"
        );
        assert_eq!(
            deleted.data.get("subject_id").and_then(Value::as_str),
            Some(message.to_string().as_str()),
            "the withdrawn message rides in subject_id"
        );
        assert!(
            deleted.subject_ids.is_empty(),
            "a chat message is no user subject: the columns stay empty"
        );
    }

    /// The federation admin pair: `delete_instance` states the domain in
    /// the sentence and carries it as `target`; `reload_emoji` names its
    /// action and nothing else — the whole pack set is its subject.
    #[tokio::test]
    async fn the_federation_entries_state_their_subjects() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;

        let deleted = delete_instance(&pool, &actor, "peer.example")
            .await
            .expect("delete_instance records");
        assert_eq!(
            deleted.message, "@mod deleted instance: peer.example",
            "the delete_instance sentence"
        );
        assert_eq!(
            deleted.data.get("action").and_then(Value::as_str),
            Some("delete_instance"),
            "the action names the entry"
        );
        assert_eq!(
            deleted.data.get("target").and_then(Value::as_str),
            Some("peer.example"),
            "the domain rides in the data map"
        );
        assert!(
            deleted.subject_ids.is_empty(),
            "a domain is no account subject"
        );

        let reloaded = reload_emoji(&pool, &actor)
            .await
            .expect("reload_emoji records");
        assert_eq!(
            reloaded.message, "@mod reloaded emoji",
            "the reload_emoji sentence"
        );
        assert_eq!(
            reloaded.data.get("action").and_then(Value::as_str),
            Some("reload_emoji"),
            "and its action"
        );
        assert!(
            reloaded.data.get("target").is_none(),
            "there is no subject to name: {}",
            reloaded.data
        );
    }
}
