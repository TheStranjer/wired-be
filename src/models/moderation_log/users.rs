//! The user-list actions: the lifecycle batch, the account-action lookup,
//! the creation and maintenance entries and the suggestion pair.

use std::slice;

use sqlx::PgPool;

use super::data::{subject_data, subjects_data};
use super::sentences::{nicknames_of, users_sentence};
use super::{ModAction, ModerationLog, record};
use crate::{error::AppResult, models::user::User};

/// Records `action` over `subjects` with the sentence `"{actor} {verb}
/// users: …"`: the user-list actions share Pleroma's exact wording (the
/// `deactivate`/`activate`/`delete`/`approve`/`reject`/
/// `force_password_reset` clauses of `get_log_entry_message`) and its
/// `subject` data map.
pub(super) async fn users_action(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
    action: &str,
    verb: &str,
) -> AppResult<ModerationLog> {
    let nicknames = nicknames_of(subjects);
    let message = users_sentence(&actor.nickname, verb, &nicknames);
    record(
        pool,
        actor,
        ModAction {
            action: action.to_owned(),
            subjects: subjects.iter().collect(),
            message,
            data: subject_data(subjects),
        },
    )
    .await
}

/// Records a `deactivate`: `@admin deactivated users: @alice, @bob`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn deactivate(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "deactivate", "deactivated").await
}

/// Records an `activate`: `@admin activated users: @alice, @bob`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn activate(pool: &PgPool, actor: &User, subjects: &[User]) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "activate", "activated").await
}

/// Records a `delete`: `@admin deleted users: @alice, @bob`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn delete(pool: &PgPool, actor: &User, subjects: &[User]) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "delete", "deleted").await
}

/// Records an `approve`: `@admin approved users: @alice, @bob`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn approve(pool: &PgPool, actor: &User, subjects: &[User]) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "approve", "approved").await
}

/// Records a `reject`: `@admin rejected users: @alice, @bob`.
///
/// The entry the Mastodon admin API's
/// `POST /api/v1/admin/accounts/{id}/reject` leaves behind (WBE-213),
/// Pleroma's `reject` clause of `get_log_entry_message`. The rejected
/// account's row is gone when this records — rejection DELETES the account —
/// and the entry is the surviving trace of it: the subject columns carry the
/// nickname of a row nothing joins to any more.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn reject(pool: &PgPool, actor: &User, subjects: &[User]) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "reject", "rejected").await
}

/// Records one account action the endpoint names by its submitted type and
/// no dedicated clause spells out: `@admin performed action {action}`.
///
/// That is Pleroma's `get_log_entry_message` fall-through for an action it
/// has no sentence of its own for, over the same `subject` user list its
/// `insert_log` writes.
///
/// [`crate::api::mastodon_admin`] records every
/// `POST /api/v1/admin/accounts/{id}/action` through here under the type the
/// request stated (`none`, `sensitive`, `silence`, `disable`, `suspend`);
/// the state changes those types make keep the dedicated entry of their own
/// ([`deactivate`], [`report_update`](super::report_update), …), which is why the two families
/// appear side by side in the log for one request.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn account_action(
    pool: &PgPool,
    actor: &User,
    subject: &User,
    action: &str,
) -> AppResult<ModerationLog> {
    let message = format!("@{} performed action {action}", actor.nickname);
    record(
        pool,
        actor,
        ModAction {
            action: action.to_owned(),
            subjects: vec![subject],
            message,
            data: subject_data(slice::from_ref(subject)),
        },
    )
    .await
}

/// Records a `create`: `@admin created users: @alice, @bob` — the
/// `create` clause of Pleroma's `get_log_entry_message`, the entry the
/// admin `POST /users` leaves behind (WBE-196).
///
/// The one departure from the sentence family of [`deactivate`] and its
/// siblings is where the entry puts the array: Pleroma's `create` clause
/// matches the PLURAL `"subjects"` key (its `insert_log` takes the map's
/// `subjects:` field verbatim into `data`), so this records the same user
/// maps there and the moderation-log read hands that spelling back
/// unchanged.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn create_users(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    let message = users_sentence(&actor.nickname, "created", &nicknames_of(subjects));
    record(
        pool,
        actor,
        ModAction {
            action: "create".to_owned(),
            subjects: subjects.iter().collect(),
            message,
            data: subjects_data(subjects),
        },
    )
    .await
}

/// Records a `force_password_reset`: `@admin forced password reset for
/// users: @alice, @bob`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn force_password_reset(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(
        pool,
        actor,
        subjects,
        "force_password_reset",
        "forced password reset for",
    )
    .await
}

/// Records an `updated_users`: `@admin updated users: @alice, @bob` — the
/// entry the admin credentials write leaves behind (Pleroma's
/// `AdminAPIController.update_user_credentials`).
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn updated_users(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "updated_users", "updated").await
}

/// Records a `confirm_email`: `@admin confirmed email for users: @alice,
/// @bob` — Pleroma's `confirm_email` clause of `get_log_entry_message`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn confirm_email(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(
        pool,
        actor,
        subjects,
        "confirm_email",
        "confirmed email for",
    )
    .await
}

/// Records a `resend_confirmation_email`: `@admin re-sent confirmation
/// email for users: @alice, @bob` — Pleroma's clause of the same name.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn resend_confirmation_email(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(
        pool,
        actor,
        subjects,
        "resend_confirmation_email",
        "re-sent confirmation email for",
    )
    .await
}

/// Records a `disable_mfa`: `@admin disabled MFA for users: @alice, @bob`.
///
/// Pleroma's `disable_mfa` endpoint logs nothing at all; the ticket asks
/// every admin write to leave an entry, so this sentence joins the
/// user-list family with the verb the action states, following the shape
/// of the five clauses above.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn disable_mfa(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "disable_mfa", "disabled MFA for").await
}

/// Records an `add_suggestion`: `@admin added suggested users: @alice,
/// @bob` — Pleroma's `add_suggestion` clause of `get_log_entry_message`,
/// the entry the admin `PATCH /users/suggest` leaves behind.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn add_suggestion(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(pool, actor, subjects, "add_suggestion", "added suggested").await
}

/// Records a `remove_suggestion`: `@admin removed suggested users: @alice,
/// @bob` — [`add_suggestion`]'s twin, the entry the admin
/// `PATCH /users/unsuggest` leaves behind.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn remove_suggestion(
    pool: &PgPool,
    actor: &User,
    subjects: &[User],
) -> AppResult<ModerationLog> {
    users_action(
        pool,
        actor,
        subjects,
        "remove_suggestion",
        "removed suggested",
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::iter;

    use serde_json::Value;

    use super::*;
    use crate::models::moderation_log::test_support::{names, stored};

    /// The user-list helpers store their action, sentence and subject
    /// columns.
    #[tokio::test]
    async fn user_list_helpers_record_their_sentences() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let alice = stored(&pool, "alice").await;
        let bob = stored(&pool, "bob").await;
        let (alice_id, bob_id) = (alice.id, bob.id);
        let subjects = [alice, bob];
        let cases = [
            deactivate(&pool, &actor, &subjects).await.expect("records"),
            activate(&pool, &actor, &subjects).await.expect("records"),
            delete(&pool, &actor, &subjects).await.expect("records"),
            approve(&pool, &actor, &subjects).await.expect("records"),
            reject(&pool, &actor, &subjects).await.expect("records"),
            create_users(&pool, &actor, &subjects)
                .await
                .expect("records"),
            force_password_reset(&pool, &actor, &subjects)
                .await
                .expect("records"),
            updated_users(&pool, &actor, &subjects)
                .await
                .expect("records"),
            confirm_email(&pool, &actor, &subjects)
                .await
                .expect("records"),
            resend_confirmation_email(&pool, &actor, &subjects)
                .await
                .expect("records"),
            disable_mfa(&pool, &actor, &subjects)
                .await
                .expect("records"),
            add_suggestion(&pool, &actor, &subjects)
                .await
                .expect("records"),
            remove_suggestion(&pool, &actor, &subjects)
                .await
                .expect("records"),
        ];
        let expectations = [
            ("deactivate", "subject"),
            ("activate", "subject"),
            ("delete", "subject"),
            ("approve", "subject"),
            ("reject", "subject"),
            ("create", "subjects"),
            ("force_password_reset", "subject"),
            ("updated_users", "subject"),
            ("confirm_email", "subject"),
            ("resend_confirmation_email", "subject"),
            ("disable_mfa", "subject"),
            ("add_suggestion", "subject"),
            ("remove_suggestion", "subject"),
        ];
        for (row, (action, subject_key)) in iter::zip(cases, expectations) {
            assert_eq!(row.action, action, "the action column");
            assert_eq!(row.subject_ids, vec![alice_id, bob_id], "{action} subjects");
            assert_eq!(
                row.subject_nicknames,
                names(&["alice", "bob"]),
                "{action} nicknames"
            );
            assert_eq!(
                row.data
                    .get(subject_key)
                    .and_then(Value::as_array)
                    .map(Vec::len),
                Some(2),
                "{action} carries its user array under {subject_key}"
            );
            assert!(
                row.message.starts_with("@mod ") && row.message.ends_with(" users: @alice, @bob"),
                "{action} reads like Pleroma: {}",
                row.message
            );
        }
    }

    /// The account-action helper stores Pleroma's fall-through sentence —
    /// `@mod performed action {type}` — with the one account in the subject
    /// columns and the same user array under `subject` the user-list
    /// actions write.
    #[tokio::test]
    async fn account_action_entries_name_their_type() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let alice = stored(&pool, "alice").await;
        let alice_id = alice.id;
        for kind in ["none", "sensitive", "silence", "disable", "suspend"] {
            let row = account_action(&pool, &actor, &alice, kind)
                .await
                .expect("records");
            assert_eq!(row.action, kind, "the entry is named by the type");
            assert_eq!(row.subject_ids, vec![alice_id], "{kind} names its subject");
            assert_eq!(
                row.data["subject"].as_array().map_or(0, Vec::len),
                1,
                "{kind} carries the one user map"
            );
            assert_eq!(
                row.message,
                format!("@mod performed action {kind}"),
                "Pleroma's fall-through sentence"
            );
        }
    }
}
