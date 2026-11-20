//! The report entries: the update, the assignment pair and the note pair.

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use super::data::report_data;
use super::sentences::{
    report_assigned_sentence, report_note_delete_sentence, report_note_sentence,
    report_unassigned_sentence, report_update_sentence,
};
use super::{ModAction, ModerationLog, put, record};
use crate::{error::AppResult, models::user::User};

/// Records a `report_update`: `@admin updated report ##{report_id}` plus
/// ` ( on user @target)` when the report names one, plus ` with 'state'
/// state` — Pleroma's report clause.
///
/// The subject map is its `report_to_map`: `{type: "report", id, state}`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn report_update(
    pool: &PgPool,
    actor: &User,
    report_id: Uuid,
    state: &str,
    subject_actor: Option<&User>,
) -> AppResult<ModerationLog> {
    let message = report_update_sentence(
        &actor.nickname,
        report_id,
        state,
        subject_actor.map(|target| target.nickname.as_str()),
    );
    record(
        pool,
        actor,
        ModAction {
            action: "report_update".to_owned(),
            subjects: Vec::new(),
            message,
            data: report_data(report_id, Some(state), subject_actor),
        },
    )
    .await
}

/// Records a `report_assigned`: `@admin assigned report ##{report_id}`
/// plus ` ( on user @target)` when the report names one, plus
/// ` to user {assignee}`.
///
/// That is Pleroma's `report_assigned` clause of `get_log_entry_message`,
/// whose controller writes it from
/// `POST /api/v1/admin/reports/{id}/assign_to_self` with the acting
/// moderator's own nickname as the assignee. The subject map is the one of
/// [`report_update`] without a state, plus the `assigned_account` text
/// Pleroma's `insert_log` carries beside it.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn report_assigned(
    pool: &PgPool,
    actor: &User,
    report_id: Uuid,
    assignee: &str,
    subject_actor: Option<&User>,
) -> AppResult<ModerationLog> {
    let message = report_assigned_sentence(
        &actor.nickname,
        report_id,
        assignee,
        subject_actor.map(|target| target.nickname.as_str()),
    );
    let mut data = report_data(report_id, None, subject_actor);
    put(
        &mut data,
        "assigned_account",
        Value::String(assignee.to_owned()),
    );
    record(
        pool,
        actor,
        ModAction {
            action: "report_assigned".to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records a `report_unassigned`: `@admin unassigned report ##{report_id}`
/// plus ` ( on user @target)` when the report names one, plus
/// ` from a user`.
///
/// That is Pleroma's `report_unassigned` clause, written by
/// `POST /api/v1/admin/reports/{id}/unassign`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn report_unassigned(
    pool: &PgPool,
    actor: &User,
    report_id: Uuid,
    subject_actor: Option<&User>,
) -> AppResult<ModerationLog> {
    let message = report_unassigned_sentence(
        &actor.nickname,
        report_id,
        subject_actor.map(|target| target.nickname.as_str()),
    );
    record(
        pool,
        actor,
        ModAction {
            action: "report_unassigned".to_owned(),
            subjects: Vec::new(),
            message,
            data: report_data(report_id, None, subject_actor),
        },
    )
    .await
}

/// Records a `report_note`: `@admin added note 'text' to report
/// ##{report_id}` plus ` on user @target` when the report names one.
///
/// The note's text rides in the entry's `text` key, as Pleroma's
/// `report_note` clause reads it.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn report_note(
    pool: &PgPool,
    actor: &User,
    report_id: Uuid,
    text: &str,
    subject_actor: Option<&User>,
) -> AppResult<ModerationLog> {
    let message = report_note_sentence(
        &actor.nickname,
        report_id,
        text,
        subject_actor.map(|target| target.nickname.as_str()),
    );
    let mut data = report_data(report_id, None, subject_actor);
    put(&mut data, "text", Value::String(text.to_owned()));
    record(
        pool,
        actor,
        ModAction {
            action: "report_note".to_owned(),
            subjects: Vec::new(),
            message,
            data,
        },
    )
    .await
}

/// Records a `report_note_delete`: `@admin deleted note 'text' from report
/// ##{report_id}` plus ` on user @target` when the report names one —
/// Pleroma's `report_note_delete` clause of `get_log_entry_message`.
///
/// The entry the admin note removal leaves behind. The note is gone when
/// this records, which is exactly why its text is kept: the log line is
/// the surviving trace of the deleted note.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn report_note_delete(
    pool: &PgPool,
    actor: &User,
    report_id: Uuid,
    text: &str,
    subject_actor: Option<&User>,
) -> AppResult<ModerationLog> {
    let message = report_note_delete_sentence(
        &actor.nickname,
        report_id,
        text,
        subject_actor.map(|target| target.nickname.as_str()),
    );
    let mut data = report_data(report_id, None, subject_actor);
    put(&mut data, "text", Value::String(text.to_owned()));
    record(
        pool,
        actor,
        ModAction {
            action: "report_note_delete".to_owned(),
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

    /// `report_update` and `report_note` store their sentences, subject
    /// maps, and optional target user; `report_assigned` adds its
    /// `assigned_account` key.
    #[tokio::test]
    async fn report_helpers_record_their_entries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let target = stored(&pool, "target").await;
        let report = new_id();
        let updated = report_update(&pool, &actor, report, "closed", Some(&target))
            .await
            .expect("report_update records");
        assert_eq!(
            updated.message,
            format!("@mod updated report ##{report} ( on user @target) with 'closed' state"),
            "the report_update entry"
        );
        assert_eq!(
            updated
                .data
                .get("subject")
                .and_then(Value::as_object)
                .and_then(|inner| inner.get("state"))
                .and_then(Value::as_str),
            Some("closed"),
            "the report subject map carries its state"
        );
        assert_eq!(
            updated
                .data
                .get("subject_actor")
                .and_then(Value::as_object)
                .and_then(|who| who.get("nickname"))
                .and_then(Value::as_str),
            Some("target"),
            "and the target user map rides beside it"
        );
        let assigned = report_assigned(&pool, &actor, report, "mod", Some(&target))
            .await
            .expect("report_assigned records");
        assert_eq!(
            assigned.message,
            format!("@mod assigned report ##{report} ( on user @target) to user mod"),
            "the report_assigned entry"
        );
        assert_eq!(
            assigned
                .data
                .get("assigned_account")
                .and_then(Value::as_str),
            Some("mod"),
            "the new owner rides in the data map, Pleroma's extra key"
        );
        assert_eq!(
            assigned.action, "report_assigned",
            "and the action column names the clause"
        );
        let noted = report_note(&pool, &actor, report, "looked", None)
            .await
            .expect("report_note records");
        assert_eq!(
            noted.message,
            format!("@mod added note 'looked' to report ##{report}"),
            "the report_note entry"
        );
        assert!(
            noted.data.get("subject_actor").is_none(),
            "no target means no subject_actor key"
        );
        assert_eq!(
            noted.data.get("text").and_then(Value::as_str),
            Some("looked"),
            "the note rides in the data map"
        );
    }

    /// `report_unassigned` stores its sentence with the named target and
    /// without one, and the action column carries the clause name.
    #[tokio::test]
    async fn report_unassigned_records_its_entry() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = stored(&pool, "mod").await;
        let target = stored(&pool, "target").await;
        let report = new_id();
        let unassigned = report_unassigned(&pool, &actor, report, Some(&target))
            .await
            .expect("report_unassigned records");
        assert_eq!(
            unassigned.action, "report_unassigned",
            "the action names the clause"
        );
        assert_eq!(
            unassigned.message,
            format!("@mod unassigned report ##{report} ( on user @target) from a user"),
            "the target rides in the sentence"
        );
        let bare = report_unassigned(&pool, &actor, report, None)
            .await
            .expect("the unnamed target records too");
        assert_eq!(
            bare.message,
            format!("@mod unassigned report ##{report} from a user"),
            "no target is the bare sentence"
        );
        assert!(
            bare.data.get("subject_actor").is_none(),
            "no target means no subject_actor key"
        );
    }
}
