//! Account backups over HTTP: the export archives an account has asked
//! this instance to build (`docs/ARCHITECTURE.md` decision 1).
//!
//! The reference is Pleroma's `BackupController`
//! (`lib/pleroma/web/pleroma_api/controllers/backup_controller.ex`) over
//! `Pleroma.User.Backup` (`lib/pleroma/user/backup.ex`), routed in
//! `lib/pleroma/web/router.ex` under `/api/v1/pleroma` as:
//!
//! ```text
//! get  "/api/v1/pleroma/backups" -> index
//! post "/api/v1/pleroma/backups" -> create
//! ```
//!
//! Those two routes are this module; the fact behind them is the
//! `backups` table ([`crate::models::backup`]) and the entity is the
//! six-key Pleroma `BackupView` shape
//! ([`crate::entities::backup::Backup`]). Backups are strictly
//! per-account: an account's list holds only its own requests.
//!
//! - `GET /api/v1/pleroma/backups` answers the caller's requests as an
//!   array, newest first ([`backup::list_for_user`]).
//! - `POST /api/v1/pleroma/backups` records a new request and hands it to
//!   the queue ([`job::enqueue`]), then answers the same whole array a
//!   `GET` would — Pleroma's `create` re-renders the list. Before
//!   recording it enforces Pleroma's `permitted?` gate
//!   (`lib/pleroma/user/backup.ex`): a newest request younger than
//!   [`backup_limit_days`](crate::config::Config::backup_limit_days) days
//!   is the 400 `Last backup was less than {N} days ago`; a limit of `0`
//!   disables the rule.
//!
//! The request is only a request until the `backups.run` job builds the
//! archive and calls
//! [`mark_processed`](crate::models::backup::mark_processed): until then
//! the entity says so (`processed: false`, an empty `url`, `file_size: 0`).
//! That kind is handled by [`handle_run`](crate::jobs::backups::handle_run),
//! registered on the worker's [`JobRegistry`](crate::jobs::JobRegistry).
//!
//! The entity's `url` is the uploader's to know, so this module asks
//! `state.uploader` for the stored key's public URL and the entity itself
//! stays pure.
//!
//! Both routes require a user token ([`AuthUser`]) and, per the scope
//! pair, `read:backups` to look and `write:backups` to request.
//!
//! [`AuthUser`]: crate::auth::AuthUser
//! [`backup::list_for_user`]: crate::models::backup::list_for_user
//! [`job::enqueue`]: crate::models::job::enqueue

use axum::{Json, Router, extract::State, routing::get};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::json;
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    entities::backup::Backup,
    error::{AppError, AppResult},
    jobs::QUEUE_BACKGROUND,
    models::{
        backup,
        job::{NewJob, enqueue},
    },
    oauth::scopes::{READ_BACKUPS, WRITE_BACKUPS},
    state::AppState,
    uploads::UploaderBackend,
};

/// The job kind a new backup request rides: the `backups.run` handler
/// ([`handle_run`](crate::jobs::backups::handle_run)) builds the
/// account's archive and marks the row processed.
pub const KIND_RUN: &str = "backups.run";

/// The `/api/v1/pleroma/backups` router, nested into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new().route("/", get(list_backups).post(create_backup))
}

/// `GET /api/v1/pleroma/backups` — the caller's backup requests, newest
/// first, as an array.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read:backups` check. Any database failure arrives through `?`
/// as [`AppError::Database`].
async fn list_backups(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Vec<Backup>>> {
    auth.require(READ_BACKUPS)?;
    let rows = backup::list_for_user(&state.db, auth.0.id).await?;
    debug!(
        count = rows.len(),
        user = %auth.0.id,
        "backups listed"
    );
    Ok(Json(render_entities(&state.uploader, &rows)))
}

/// `POST /api/v1/pleroma/backups` — record a new export request for the
/// caller, queue the job that builds it, and answer the whole list as
/// Pleroma's `create` re-render of `index` does.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:backups` check; [`AppError::BadRequest`] with Pleroma's
/// rate-limit wording when [`too_recent`] holds the caller's newest
/// request inside the [`backup_limit_days`] window. Any database failure
/// arrives through `?` as [`AppError::Database`].
///
/// [`backup_limit_days`]: crate::config::Config::backup_limit_days
async fn create_backup(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Vec<Backup>>> {
    auth.require(WRITE_BACKUPS)?;
    let limit_days = state.config.backup_limit_days;
    if let Some(latest) = backup::latest(&state.db, auth.0.id).await?
        && too_recent(latest.inserted_at, Utc::now(), limit_days)
    {
        return Err(too_recent_error(limit_days));
    }
    let row = backup::create(&state.db, &auth.0).await?;
    enqueue_backup_job(&state.db, row.id).await?;
    debug!(
        backup = %row.id,
        user = %auth.0.id,
        "backup requested"
    );
    let rows = backup::list_for_user(&state.db, auth.0.id).await?;
    Ok(Json(render_entities(&state.uploader, &rows)))
}

/// Pleroma's `permitted?` decision (`lib/pleroma/user/backup.ex`) as a
/// pure function: whether a request made at `last` still stands inside
/// the `limit_days` interval when read at `now`. The window is exclusive
/// at its far edge — exactly `limit_days` old is permitted again, younger
/// is not — and a limit of `0` asks nothing of the clock and refuses
/// nothing a stored row can be. A limit beyond what [`TimeDelta`] can
/// hold is a window that never closes, which nothing is ever old enough
/// to escape.
fn too_recent(last: DateTime<Utc>, now: DateTime<Utc>, limit_days: usize) -> bool {
    let days = i64::try_from(limit_days).unwrap_or(i64::MAX);
    let window = TimeDelta::try_days(days).unwrap_or(TimeDelta::MAX);
    // A cutoff before the representable range is a window still open on
    // every row the table can hold.
    now.checked_sub_signed(window)
        .is_none_or(|cutoff| last > cutoff)
}

/// The 400 Pleroma's rate limit answers with: the interval the caller
/// must wait out, counted in days, with no plural special-case (Pleroma's
/// own string).
fn too_recent_error(limit_days: usize) -> AppError {
    AppError::BadRequest(format!("Last backup was less than {limit_days} days ago"))
}

/// Enqueues the [`QUEUE_BACKGROUND`] job that builds the backup `backup_id`
/// was recorded under, carrying `{"backup_id": "<uuid>"}` for the handler.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
async fn enqueue_backup_job(pool: &PgPool, backup_id: Uuid) -> AppResult<()> {
    let queued = NewJob {
        queue: QUEUE_BACKGROUND,
        kind: KIND_RUN,
        args: json!({"backup_id": backup_id.to_string()}),
        run_at: None,
        max_attempts: None,
        unique_key: None,
    };
    let _job = enqueue(pool, queued).await?;
    Ok(())
}

/// Renders `rows` as entities, newest first as read, each carrying its
/// download URL (see [`download_url`]).
fn render_entities(uploader: &UploaderBackend, rows: &[backup::Backup]) -> Vec<Backup> {
    rows.iter()
        .map(|row| Backup::from_row(row, &download_url(uploader, row)))
        .collect()
}

/// The entity's `url` for `row`: the stored archive's public address once
/// [`mark_processed`](backup::mark_processed) has put a file behind the
/// row, and the empty string for a request that has none yet.
fn download_url(uploader: &UploaderBackend, row: &backup::Backup) -> String {
    match (row.processed, row.storage_key.as_deref()) {
        (true, Some(key)) => uploader.public_url(key),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration as ChronoDuration;

    use super::*;

    /// The interval the [`too_recent`] tests read as the limit, in days.
    const LIMIT: usize = 7;

    #[test]
    fn a_request_younger_than_the_limit_is_refused() {
        let now = Utc::now();
        let span = ChronoDuration::days(i64::try_from(LIMIT).expect("seven is a usize count"));
        for age_days in [0, 1, 6] {
            let last = now - ChronoDuration::days(age_days);
            assert!(
                too_recent(last, now, LIMIT),
                "{age_days} days old stands inside the {LIMIT}-day window"
            );
        }
        let almost = now - span + ChronoDuration::seconds(1);
        assert!(too_recent(almost, now, LIMIT), "one second short is short");
    }

    #[test]
    fn a_request_past_the_limit_is_permitted_again() {
        let now = Utc::now();
        let span = ChronoDuration::days(i64::try_from(LIMIT).expect("seven is a usize count"));
        // The window's far edge is exclusive: exactly `limit` old is out of it.
        assert!(
            !too_recent(now - span, now, LIMIT),
            "exactly {LIMIT} days old is permitted"
        );
        let past = now - span - ChronoDuration::seconds(1);
        assert!(!too_recent(past, now, LIMIT), "and a second past it too");
        let long_ago = now - span * 10;
        assert!(!too_recent(long_ago, now, LIMIT), "so is much older");
    }

    #[test]
    fn a_zero_limit_refuses_nothing_a_row_can_be() {
        let now = Utc::now();
        assert!(
            !too_recent(now - ChronoDuration::seconds(1), now, 0),
            "a row from the last second passes"
        );
        assert!(
            !too_recent(now, now, 0),
            "a row stamped this very instant passes: the window is exclusive"
        );
    }

    #[test]
    fn an_unrepresentable_limit_never_permits() {
        let now = Utc::now();
        assert!(
            too_recent(now - ChronoDuration::days(365 * 100), now, usize::MAX),
            "a limit no `TimeDelta` can hold is a window that never closes"
        );
    }

    #[test]
    fn the_refusal_names_the_window_in_days() {
        let message = too_recent_error(7).to_string();
        assert_eq!(message, "Last backup was less than 7 days ago");
        assert_eq!(
            too_recent_error(0).to_string(),
            "Last backup was less than 0 days ago",
            "no plural special-case, whatever the count"
        );
    }
}
