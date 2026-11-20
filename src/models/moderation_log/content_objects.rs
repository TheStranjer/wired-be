//! The content-maintenance entries: the rule trio and the announcement
//! trio, each naming its row by id.

use serde_json::{Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::{ModAction, ModerationLog, put, record};
use crate::{error::AppResult, models::user::User};

/// Records a `rule_create`: `@admin created rule: {rule_id}`.
///
/// The admin content API's `POST /rules` logs here. A rule is no user, so
/// — like an invite, a webhook or a pack — its id rides in `subject_id` as
/// text and the subject columns stay empty.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn rule_create(pool: &PgPool, actor: &User, rule_id: Uuid) -> AppResult<ModerationLog> {
    rule_action(pool, actor, rule_id, "rule_create", "created").await
}

/// Records a `rule_update`: `@admin updated rule: {rule_id}`.
///
/// The admin content API's `PATCH /rules/{id}` logs here.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn rule_update(pool: &PgPool, actor: &User, rule_id: Uuid) -> AppResult<ModerationLog> {
    rule_action(pool, actor, rule_id, "rule_update", "updated").await
}

/// Records a `rule_delete`: `@admin deleted rule: {rule_id}`.
///
/// The admin content API's `DELETE /rules/{id}` logs here. The row is gone
/// when this records, which is exactly why the id is kept: the line is the
/// surviving trace of the deleted rule.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn rule_delete(pool: &PgPool, actor: &User, rule_id: Uuid) -> AppResult<ModerationLog> {
    rule_action(pool, actor, rule_id, "rule_delete", "deleted").await
}

/// The three rule clauses share their sentence shape and their
/// `subject_id` key; only the action name and the verb differ.
pub(super) async fn rule_action(
    pool: &PgPool,
    actor: &User,
    rule_id: Uuid,
    action: &str,
    verb: &str,
) -> AppResult<ModerationLog> {
    let message = format!("@{} {verb} rule: {rule_id}", actor.nickname);
    let mut data = Map::new();
    put(&mut data, "subject_id", Value::String(rule_id.to_string()));
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

/// Records an `announcement_create`:
/// `@admin created announcement: {announcement_id}`.
///
/// The admin content API's `POST /announcements` logs here, beside the
/// stream event the route publishes.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn announcement_create(
    pool: &PgPool,
    actor: &User,
    announcement_id: Uuid,
) -> AppResult<ModerationLog> {
    announcement_action(
        pool,
        actor,
        announcement_id,
        "announcement_create",
        "created",
    )
    .await
}

/// Records an `announcement_update`:
/// `@admin updated announcement: {announcement_id}`.
///
/// The admin content API's `PATCH /announcements/{id}` logs here.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn announcement_update(
    pool: &PgPool,
    actor: &User,
    announcement_id: Uuid,
) -> AppResult<ModerationLog> {
    announcement_action(
        pool,
        actor,
        announcement_id,
        "announcement_update",
        "updated",
    )
    .await
}

/// Records an `announcement_delete`:
/// `@admin deleted announcement: {announcement_id}`.
///
/// The admin content API's `DELETE /announcements/{id}` logs here, the line
/// being what survives the notice and its dismissal rows.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError); see [`record`].
pub async fn announcement_delete(
    pool: &PgPool,
    actor: &User,
    announcement_id: Uuid,
) -> AppResult<ModerationLog> {
    announcement_action(
        pool,
        actor,
        announcement_id,
        "announcement_delete",
        "deleted",
    )
    .await
}

/// The three announcement clauses share their sentence shape and their
/// `subject_id` key; only the action name and the verb differ.
pub(super) async fn announcement_action(
    pool: &PgPool,
    actor: &User,
    announcement_id: Uuid,
    action: &str,
    verb: &str,
) -> AppResult<ModerationLog> {
    let message = format!("@{} {verb} announcement: {announcement_id}", actor.nickname);
    let mut data = Map::new();
    put(
        &mut data,
        "subject_id",
        Value::String(announcement_id.to_string()),
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
