//! The entries of the administered objects named by id or directory: the
//! invite pair, the webhook family and the emoji-pack trio.

use serde_json::{Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::{ModAction, ModerationLog, put, record};
use crate::{error::AppResult, models::user::User};

/// Records an `invite_create`: `@admin created invite: {invite_id}`.
///
/// The admin API's `POST /users/invite_token` logs here. The invite is no
/// user, so — like a deleted status — its id rides in `subject_id` as
/// text and the subject columns stay empty.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn invite_create(
    pool: &PgPool,
    actor: &User,
    invite_id: Uuid,
) -> AppResult<ModerationLog> {
    invite_action(pool, actor, invite_id, "invite_create", "created").await
}

/// Records an `invite_revoke`: `@admin revoked invite: {invite_id}`.
///
/// The admin API's `POST /users/revoke_invite` logs here. The invite row
/// is gone when this records, which is exactly why the id is kept: the log
/// line is the surviving trace of the revoked invite.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn invite_revoke(
    pool: &PgPool,
    actor: &User,
    invite_id: Uuid,
) -> AppResult<ModerationLog> {
    invite_action(pool, actor, invite_id, "invite_revoke", "revoked").await
}

/// The two invite clauses share their sentence shape and their
/// `subject_id` key; only the action name and the verb differ.
pub(super) async fn invite_action(
    pool: &PgPool,
    actor: &User,
    invite_id: Uuid,
    action: &str,
    verb: &str,
) -> AppResult<ModerationLog> {
    let message = format!("@{} {verb} invite: {invite_id}", actor.nickname);
    let mut data = Map::new();
    put(
        &mut data,
        "subject_id",
        Value::String(invite_id.to_string()),
    );
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

/// Records a `webhook_create`: `@admin created webhook: {webhook_id}`.
///
/// The admin API's `POST /webhooks` logs here. A webhook is no user, so —
/// like an invite — its id rides in `subject_id` as text and the subject
/// columns stay empty.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn webhook_create(
    pool: &PgPool,
    actor: &User,
    webhook_id: Uuid,
) -> AppResult<ModerationLog> {
    webhook_action(pool, actor, webhook_id, "webhook_create", "created").await
}

/// Records a `webhook_update`: `@admin updated webhook: {webhook_id}`.
///
/// The admin API's `PATCH /webhooks/{id}` logs here.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn webhook_update(
    pool: &PgPool,
    actor: &User,
    webhook_id: Uuid,
) -> AppResult<ModerationLog> {
    webhook_action(pool, actor, webhook_id, "webhook_update", "updated").await
}

/// Records a `webhook_delete`: `@admin deleted webhook: {webhook_id}`.
///
/// The admin API's `DELETE /webhooks/{id}` logs here. The row is gone when
/// this records, which is exactly why the id is kept: the log line is the
/// surviving trace of the deleted webhook.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn webhook_delete(
    pool: &PgPool,
    actor: &User,
    webhook_id: Uuid,
) -> AppResult<ModerationLog> {
    webhook_action(pool, actor, webhook_id, "webhook_delete", "deleted").await
}

/// Records a `webhook_rotate_secret`:
/// `@admin rotated secret for webhook: {webhook_id}`.
///
/// The admin API's `POST /webhooks/{id}/rotate_secret` logs here. The new
/// secret itself never reaches the log: the line names the row, not its
/// secret material.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn webhook_rotate_secret(
    pool: &PgPool,
    actor: &User,
    webhook_id: Uuid,
) -> AppResult<ModerationLog> {
    webhook_action(
        pool,
        actor,
        webhook_id,
        "webhook_rotate_secret",
        "rotated secret for",
    )
    .await
}

/// The four webhook clauses share their sentence shape and their
/// `subject_id` key; only the action name and the verb differ.
pub(super) async fn webhook_action(
    pool: &PgPool,
    actor: &User,
    webhook_id: Uuid,
    action: &str,
    verb: &str,
) -> AppResult<ModerationLog> {
    let message = format!("@{} {verb} webhook: {webhook_id}", actor.nickname);
    let mut data = Map::new();
    put(
        &mut data,
        "subject_id",
        Value::String(webhook_id.to_string()),
    );
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

/// Records an `emoji_pack_create`: `@admin created emoji pack: {name}`.
///
/// The pack admin API's `POST /api/v1/pleroma/emoji/pack` logs here. A
/// pack is no user and has no id — its directory name is its identity — so
/// the name rides in `subject_id` as text and the subject columns stay
/// empty, as with the invite and webhook actions.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn emoji_pack_create(
    pool: &PgPool,
    actor: &User,
    pack: &str,
) -> AppResult<ModerationLog> {
    emoji_pack_action(pool, actor, pack, "emoji_pack_create", "created").await
}

/// Records an `emoji_pack_update`: `@admin updated emoji pack: {name}`.
///
/// The pack admin API's `PATCH /api/v1/pleroma/emoji/pack` logs here.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn emoji_pack_update(
    pool: &PgPool,
    actor: &User,
    pack: &str,
) -> AppResult<ModerationLog> {
    emoji_pack_action(pool, actor, pack, "emoji_pack_update", "updated").await
}

/// Records an `emoji_pack_delete`: `@admin deleted emoji pack: {name}`.
///
/// The pack admin API's `DELETE /api/v1/pleroma/emoji/pack` logs here. The
/// pack is gone when this records, which is exactly why the name is kept:
/// the log line is the surviving trace of the deleted pack.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn emoji_pack_delete(
    pool: &PgPool,
    actor: &User,
    pack: &str,
) -> AppResult<ModerationLog> {
    emoji_pack_action(pool, actor, pack, "emoji_pack_delete", "deleted").await
}

/// The three emoji-pack clauses share their sentence shape and their
/// `subject_id` key (holding the pack's name); only the action name and
/// the verb differ.
pub(super) async fn emoji_pack_action(
    pool: &PgPool,
    actor: &User,
    pack: &str,
    action: &str,
    verb: &str,
) -> AppResult<ModerationLog> {
    let message = format!("@{} {verb} emoji pack: {pack}", actor.nickname);
    let mut data = Map::new();
    put(&mut data, "subject_id", Value::String(pack.to_owned()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{models::moderation_log::test_support::stored, util::id::new_id};

    /// The `invite_create` and `invite_revoke` entries: their sentences
    /// name the invite id, the id rides in `subject_id` as text (the
    /// invite is no user, and a revoked one has no row left), and the
    /// subject columns stay empty.
    #[tokio::test]
    async fn invite_helpers_record_their_entries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let invite = new_id();
        let created = invite_create(&pool, &actor, invite)
            .await
            .expect("invite_create records");
        assert_eq!(created.action, "invite_create", "the action column");
        assert_eq!(
            created.message,
            format!("@mod created invite: {invite}"),
            "the sentence names the invite id"
        );
        assert_eq!(
            created.data.get("subject_id").and_then(Value::as_str),
            Some(invite.to_string().as_str()),
            "the invite rides in subject_id"
        );
        assert!(
            created.subject_ids.is_empty() && created.subject_nicknames.is_empty(),
            "an invite is not a user subject"
        );
        let revoked = invite_revoke(&pool, &actor, invite)
            .await
            .expect("invite_revoke records");
        assert_eq!(revoked.action, "invite_revoke", "the action column");
        assert_eq!(
            revoked.message,
            format!("@mod revoked invite: {invite}"),
            "the revoke sentence, same shape"
        );
        assert_eq!(
            revoked.data.get("subject_id").and_then(Value::as_str),
            Some(invite.to_string().as_str()),
            "and the revoked invite keeps its id in the data map"
        );
    }
}
