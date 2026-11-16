//! The admin reports surface: reading, moving, assigning and annotating
//! filed reports under `/api/v1/pleroma/admin/reports` (WBE-200, with the
//! WBE-212 state change).
//!
//! The same two reads ride the legacy `/api/v0/pleroma/reports` mount.
//! The reference is Pleroma's `AdminAPI.ReportController`
//! (`lib/pleroma/web/admin_api/controllers/report_controller.ex`) over its
//! `ReportView` (`lib/pleroma/web/admin_api/views/report_view.ex`), routed
//! in `lib/pleroma/web/router.ex` under the `admin_api` pipeline — whose
//! gate plug is `UserIsStaffPlug`, and whose default privileges hand
//! `:reports_manage_reports` to moderators as well as admins. That is why
//! every route here takes the [`StaffUser`] extractor: a moderator with an
//! `admin:read:reports` / `admin:write:reports` token is Pleroma's
//! expected caller, and the [`StaffUser::require`] call each handler makes
//! first is the same pipeline's `OAuthScopesPlug` half.
//!
//! - `GET /reports` (`index`) — the [`report::admin_list`] page: the
//!   `state`, `rule_id` and `assigned_account` filters, the `page` /
//!   `page_size` window (with `limit` as its synonym and the ticket's
//!   default of 20), answered as `{total, reports}` of the
//!   [`AdminReport`] entity: target under `account`, reporter under
//!   `actor`, the reporter's comment as `content`, the cited statuses
//!   rendered with the reading staff member as viewer **and no visibility
//!   filter** (Pleroma's admin view runs none either — the moderation
//!   queue is where private posts are read), the moderators' notes, and
//!   the assignee's admin entity under `assigned_account`, omitted while
//!   the report is unassigned.
//! - `GET /reports/{id}` (`show`) — the one entity the id names, or the
//!   `404` `{"error":"Not found"}` Pleroma's fallback answers (a
//!   malformed id names no report and is the same miss).
//! - `PATCH /reports` (`update`, WBE-212) — Pleroma's bulk state change:
//!   a body of `{"reports": [{"id": …, "state": …}]}`, one report per
//!   element, each moved by [`report::set_state`] (which stamps
//!   `action_taken_at` on a resolution and clears it on a reopening).
//!   Every element that moved gets its `report_update`
//!   [`moderation_log`] entry, its [`webhooks::report_updated`] event and
//!   nothing else; every element that did not — an id that names no
//!   report, a state off `open`/`closed`/`resolved` — is answered with
//!   Pleroma's `{"id": …, "error": …}` entry and a `400`, while the ones
//!   that did move stay moved: Pleroma's `update` runs no transaction
//!   over its `Enum.map`, and neither does this. An answer with no
//!   failure is the `204` Pleroma's `json_response(:no_content)` gives.
//! - `POST /reports/assign_account` (`assign_account`) — a body of
//!   `{"reports": [{"id": …, "assigned_account": nickname}]}`; each
//!   element names its report and the moderator who should own it, the
//!   `null` (or absent) nickname handing the report back to the
//!   unassigned queue, the same two Pleroma clauses. When every element
//!   resolved, the answer is the `200` array of the updated [`AdminReport`]
//!   entities; when one did not, the answer is Pleroma's `400`
//!   `{"id": …, "error": "not_found"}` list — an element refuses as
//!   `not_found` for a malformed or unknown report id and for a nickname
//!   no account carries — and the elements that did resolve stay
//!   assigned.
//! - `POST /reports/{id}/notes` (`notes_create`) — a `content` body
//!   (required, at most [`MAX_NOTE_LENGTH`] characters) writes a
//!   `report_notes` row authored by the caller and logs the
//!   `report_note` entry, answering Pleroma's `204`. A blank or
//!   over-long content is the `422` the body checks state; a report that
//!   names nothing is the same `404` as `show`.
//! - `DELETE /reports/{report_id}/notes/{id}` (`notes_delete`) — removes
//!   the note *of that report* (one miss across reports, so a note of
//!   another report cannot be deleted through this pair) and logs the
//!   `report_note_delete` entry with the removed text; `204`, or the
//!   `404` for a pair that names no note.
//!
//! `GET /api/v0/pleroma/reports` and `/api/v0/pleroma/reports/{id}` are
//! the legacy mount of the same `index`/`show` pair — [`legacy_router`],
//! nested at the root by [`crate::api`]. Pleroma serves those paths from
//! its `PleromaAPI.ReportController`, the historical admin report ledger
//! (its client counterpart is `POST /api/v1/reports`, [`crate::api::reports`],
//! a different surface entirely); the ticket pins this pair to the same
//! [`StaffUser`]-gated admin entities, so both mounts run the very same
//! handlers.
//!
//! The ids are this schema's `UUIDv7` text rather than Pleroma's integers
//! (decision 4).
//!
//! [`StaffUser`]: crate::auth::admin::StaffUser
//! [`StaffUser::require`]: crate::auth::admin::StaffUser::require
//! [`moderation_log`]: crate::models::moderation_log
//! [`report::set_state`]: crate::models::report::set_state
//! [`report::admin_list`]: crate::models::report::admin_list
//! [`webhooks::report_updated`]: crate::services::webhooks::report_updated
//! [`AdminReport`]: crate::entities::admin_report::AdminReport

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse as _, Response},
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    auth::admin::StaffUser,
    entities::admin_report::{AdminReport, AdminReportNote, AdminReportPage, AdminReportParts},
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        moderation_log,
        report::{self, Report as ReportRow, ReportState},
        status,
        user::{self, User},
    },
    services::{
        render_status::{ViewerMode, render_statuses},
        webhooks,
    },
    state::AppState,
    util::id::parse_id,
};

/// The read endpoint class of [`StaffUser::require`].
const CLASS_READ: &str = "read";
/// The write endpoint class of [`StaffUser::require`].
const CLASS_WRITE: &str = "write";
/// The admin scope area these endpoints live in (`admin:read:reports` and
/// `admin:write:reports`, Pleroma's `OAuthScopesPlug` grants for the
/// controller's actions).
const AREA_REPORTS: &str = "reports";
/// The per-item error of a `state` outside the three the column allows —
/// Pleroma's `Utils.update_report_state` falls through to
/// `{:error, "Unsupported state"}`.
const UNSUPPORTED_STATE: &str = "Unsupported state";
/// The per-item error of an id that is no id or names no report (and of a
/// nickname that names no account): Pleroma's `{:error, :not_found}`,
/// which its JSON renders as this atom's text.
const NOT_FOUND: &str = "not_found";
/// The refusal for a path or filter id that is no id.
const INVALID_ID: &str = "Invalid id";
/// The ticket's default page window: `limit` (and, through it,
/// `page_size`) when the request states neither.
const DEFAULT_WINDOW: i64 = 20;
/// The note body's cap the ticket pins, exported so the tests state the
/// same bound.
pub const MAX_NOTE_LENGTH: usize = 5000;
/// The 422 for a blank note, Mastodon's changeset wording.
const NOTE_BLANK: &str = "Validation failed: Content can not be blank";

/// The `/reports` routes of the admin API, merged into [`super`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/reports", get(list_reports).patch(update_reports))
        .route("/reports/assign_account", post(assign_account))
        .route("/reports/{id}", get(show_report))
        .route("/reports/{id}/notes", post(add_note))
        .route("/reports/{report_id}/notes/{id}", delete(delete_note))
}

/// The legacy `/api/v0/pleroma` mount of the two admin reads, nested by
/// [`crate::api`] at the root: the same handlers, the same gate, the same
/// entities.
pub fn legacy_router() -> Router<AppState> {
    Router::new()
        .route("/reports", get(list_reports))
        .route("/reports/{id}", get(show_report))
}

// ---------------------------------------------------------------------------
// index / show
// ---------------------------------------------------------------------------

/// `GET /api/v1/pleroma/admin/reports` (and the legacy mount) — Pleroma's
/// `index`: the filtered page of [`AdminReport`] entities beside the total
/// the filters select.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:read` gate; [`AppError::BadRequest`] for a `state` off
/// the three or a `rule_id` / `assigned_account` that is no id;
/// [`AppError::Database`] from the listing reads.
async fn list_reports(
    State(state): State<AppState>,
    staff: StaffUser,
    Query(params): Query<HashMap<String, String>>,
) -> AppResult<Json<AdminReportPage>> {
    staff.require(CLASS_READ, AREA_REPORTS)?;
    let params = list_params(&params)?;
    let page = report::admin_list(&state.db, &params).await?;
    debug!(
        listed = page.reports.len(),
        total = page.total,
        "admin reports listed"
    );
    let reports = entities(&state, &staff.0, &page.reports).await?;
    Ok(Json(AdminReportPage {
        total: page.total,
        reports,
    }))
}

/// `GET /api/v1/pleroma/admin/reports/{id}` (and the legacy mount) —
/// Pleroma's `show`: the one entity the id names.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:read` gate; [`AppError::RouteNotFound`] for an id that
/// is no id or names no report; [`AppError::Database`] from the reads.
async fn show_report(
    State(state): State<AppState>,
    staff: StaffUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminReport>> {
    staff.require(CLASS_READ, AREA_REPORTS)?;
    let row = report_by_id(&state.db, &raw).await?;
    entity(&state, &staff.0, &row).await.map(Json)
}

/// Builds the model's listing params from the raw query map: `page` read
/// leniently (the shared admin default of 1), the window through
/// [`window_size`], the three filters strictly — an unusable one refuses
/// rather than silently listing everything.
fn list_params(params: &HashMap<String, String>) -> AppResult<report::AdminListParams> {
    Ok(report::AdminListParams {
        state: state_filter(params)?,
        rule_id: id_filter(params, "rule_id")?,
        assigned_account: id_filter(params, "assigned_account")?,
        page: super::page_number(params, "page", super::DEFAULT_PAGE),
        page_size: window_size(params),
    })
}

/// The `state` filter: absent or empty is every state, anything the enum
/// does not name is the 400 [`UNSUPPORTED_STATE`].
fn state_filter(params: &HashMap<String, String>) -> AppResult<Option<ReportState>> {
    super::text_of(params, "state")
        .map(|text| {
            text.parse::<ReportState>()
                .map_err(|_unknown| AppError::BadRequest(UNSUPPORTED_STATE.to_owned()))
        })
        .transpose()
}

/// One id filter: absent or empty names nothing to filter on, a value
/// that is no id is the 400 [`INVALID_ID`] the moderation log's `user_id`
/// filter gives too.
fn id_filter(params: &HashMap<String, String>, key: &str) -> AppResult<Option<Uuid>> {
    super::text_of(params, key)
        .map(|raw| parse_id(&raw).ok_or_else(|| AppError::BadRequest(INVALID_ID.to_owned())))
        .transpose()
}

/// The listing window: `page_size` wins, `limit` is its synonym, and
/// neither (or an unusable value of either) is the ticket's default of
/// [`DEFAULT_WINDOW`]; the admin cap of [`super::MAX_PAGE_SIZE`] bounds
/// an explicit value.
fn window_size(params: &HashMap<String, String>) -> i64 {
    let positive = |key: &str| {
        super::text_of(params, key)
            .as_deref()
            .and_then(|text| text.parse::<i64>().ok())
            .filter(|value| *value >= 1)
    };
    positive("page_size")
        .or_else(|| positive("limit"))
        .unwrap_or(DEFAULT_WINDOW)
        .min(super::MAX_PAGE_SIZE)
}

// ---------------------------------------------------------------------------
// update (the WBE-212 state change)
// ---------------------------------------------------------------------------

/// One element of the request's `reports` array: the report to move and
/// the state to move it to.
#[derive(Debug, Deserialize)]
struct ReportChange {
    /// The report to change; this schema's id text, not Pleroma's integer.
    id: String,
    /// The new state: `open`, `closed` or `resolved`.
    state: String,
}

/// The body of `PATCH /api/v1/pleroma/admin/reports`: Pleroma's
/// `update_request`, the reports to move.
#[derive(Debug, Deserialize)]
struct UpdateBody {
    /// Every element the request changes; an empty list changes nothing.
    reports: Vec<ReportChange>,
}

/// What one element of the request came to.
#[derive(Debug)]
enum Moved {
    /// The rewritten row, ready to be logged and announced.
    Row(ReportRow),
    /// Why the element moved nothing, in the entry the 400 answers with.
    Refused(&'static str),
}

/// `PATCH /api/v1/pleroma/admin/reports` — Pleroma's `update`: move each
/// named report to its new state, log and announce the ones that moved,
/// and answer `204` or the `400` list of what did not move.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:write` gate; [`AppError::Database`] from a write that
/// failed for a reason the request cannot be blamed for.
async fn update_reports(
    State(state): State<AppState>,
    staff: StaffUser,
    ApiBody(body): ApiBody<UpdateBody>,
) -> AppResult<Response> {
    staff.require(CLASS_WRITE, AREA_REPORTS)?;
    let failures = move_all(&state, &staff.0, &body.reports).await?;
    if failures.is_empty() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    debug!(
        refused = failures.len(),
        "admin report state change refused some reports"
    );
    Ok((StatusCode::BAD_REQUEST, Json(failures)).into_response())
}

/// The request's elements, in order: each one moved and followed up, or
/// collected into the failure list the caller answers with.
///
/// # Errors
///
/// [`AppError::Database`] from [`move_one`]'s write or from [`follow_up`].
async fn move_all(
    state: &AppState,
    actor: &User,
    changes: &[ReportChange],
) -> AppResult<Vec<Value>> {
    let mut failures: Vec<Value> = Vec::with_capacity(changes.len());
    for change in changes {
        match move_one(&state.db, change).await? {
            Moved::Row(row) => follow_up(state, actor, &row).await?,
            Moved::Refused(reason) => failures.push(refusal(change, reason)),
        }
    }
    Ok(failures)
}

/// One report's move: the state check, the id check and the write.
///
/// # Errors
///
/// [`AppError::Database`] when the write itself failed; a state off the
/// three or an id that names no report is [`Moved::Refused`], not an
/// error — the other elements of the request are still applied.
async fn move_one(pool: &PgPool, change: &ReportChange) -> AppResult<Moved> {
    let Ok(state_name) = change.state.parse::<ReportState>() else {
        return Ok(Moved::Refused(UNSUPPORTED_STATE));
    };
    let Some(id) = parse_id(&change.id) else {
        return Ok(Moved::Refused(NOT_FOUND));
    };
    let row = report::set_state(pool, id, state_name).await?;
    Ok(row.map_or(Moved::Refused(NOT_FOUND), Moved::Row))
}

/// What a moved report owes: its `report_update` moderation-log entry and
/// its `report.updated` event (the webhook fan-out is a queue write, so a
/// subscribed endpoint costs this request nothing but the insert).
///
/// # Errors
///
/// [`AppError::Database`] from either read or write.
async fn follow_up(state: &AppState, actor: &User, row: &ReportRow) -> AppResult<()> {
    let target = user::get_by_id(&state.db, row.target_id).await?;
    let logged = moderation_log::report_update(
        &state.db,
        actor,
        row.id,
        row.state.as_str(),
        target.as_ref(),
    )
    .await?;
    debug!(
        id = %logged.id,
        report = %row.id,
        state = row.state.as_str(),
        "admin report state changed"
    );
    webhooks::report_updated(state, row).await;
    Ok(())
}

/// Pleroma's per-item failure entry: the id as the request sent it and
/// the message that says why it moved nothing.
fn refusal(change: &ReportChange, reason: &str) -> Value {
    json!({"id": change.id, "error": reason})
}

// ---------------------------------------------------------------------------
// assign_account
// ---------------------------------------------------------------------------

/// One element of the assignment request: the report and the nickname of
/// the moderator who should own it; an absent or `null` nickname hands
/// the report back to the unassigned queue.
#[derive(Debug, Deserialize)]
struct ReportAssignment {
    /// The report to assign; this schema's id text.
    id: String,
    /// The assignee's nickname, or none.
    assigned_account: Option<String>,
}

/// The body of `POST /api/v1/pleroma/admin/reports/assign_account`:
/// Pleroma's `assign_account_request`, the reports to (re)assign.
#[derive(Debug, Deserialize)]
struct AssignBody {
    /// Every element the request assigns; an empty list assigns nothing.
    reports: Vec<ReportAssignment>,
}

/// What one element of the assignment request came to.
#[derive(Debug)]
enum Assigned {
    /// The rewritten row, ready to render.
    Row(ReportRow),
    /// Why the element assigned nothing, in the entry the 400 answers
    /// with.
    Refused(&'static str),
}

/// `POST /api/v1/pleroma/admin/reports/assign_account` — Pleroma's
/// `assign_account`: move each named report to its named moderator and
/// answer the updated [`AdminReport`] entities, or the `400` list of what
/// resolved to nothing.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:write` gate; [`AppError::Database`] from a write that
/// failed for a reason the request cannot be blamed for.
async fn assign_account(
    State(state): State<AppState>,
    staff: StaffUser,
    ApiBody(body): ApiBody<AssignBody>,
) -> AppResult<Response> {
    staff.require(CLASS_WRITE, AREA_REPORTS)?;
    let (moved, failures) = assign_all(&state.db, &body.reports).await?;
    if !failures.is_empty() {
        debug!(
            refused = failures.len(),
            "admin report assignment resolved nothing for some reports"
        );
        return Ok((StatusCode::BAD_REQUEST, Json(failures)).into_response());
    }
    debug!(assigned = moved.len(), "admin reports assigned",);
    Ok(Json(entities(&state, &staff.0, &moved).await?).into_response())
}

/// The request's elements, in order: the assigned rows the request
/// resolved, beside the failure entries it produced.
///
/// # Errors
///
/// [`AppError::Database`] from any read or write [`assign_one`] reaches.
async fn assign_all(
    pool: &PgPool,
    changes: &[ReportAssignment],
) -> AppResult<(Vec<ReportRow>, Vec<Value>)> {
    let mut moved: Vec<ReportRow> = Vec::with_capacity(changes.len());
    let mut failures: Vec<Value> = Vec::new();
    for change in changes {
        match assign_one(pool, change).await? {
            Assigned::Row(row) => moved.push(row),
            Assigned::Refused(reason) => {
                failures.push(json!({"id": change.id, "error": reason}));
            }
        }
    }
    Ok((moved, failures))
}

/// One report's assignment: the id check, the nickname lookup and the
/// write.
///
/// # Errors
///
/// [`AppError::Database`] when a read or write itself failed; an id that
/// is no id, a report that names nothing or a nickname no account carries
/// is [`Assigned::Refused`] with Pleroma's `not_found`, not an error —
/// the other elements of the request are still applied.
async fn assign_one(pool: &PgPool, change: &ReportAssignment) -> AppResult<Assigned> {
    let Some(id) = parse_id(&change.id) else {
        return Ok(Assigned::Refused(NOT_FOUND));
    };
    let assignee = match change.assigned_account.as_deref() {
        Some(nickname) => {
            let Some(found) = user::get_by_nickname(pool, nickname).await? else {
                return Ok(Assigned::Refused(NOT_FOUND));
            };
            Some(found.id)
        }
        None => None,
    };
    let row = report::assign(pool, id, assignee).await?;
    Ok(row.map_or(Assigned::Refused(NOT_FOUND), Assigned::Row))
}

// ---------------------------------------------------------------------------
// notes
// ---------------------------------------------------------------------------

/// The body of `POST /api/v1/pleroma/admin/reports/{id}/notes`: Pleroma's
/// `notes_create_request`, the moderator's text.
#[derive(Debug, Deserialize)]
struct NoteBody {
    /// The note to write; a body without it is the extractor's 422.
    content: String,
}

/// `POST /api/v1/pleroma/admin/reports/{id}/notes` — Pleroma's
/// `notes_create`: write the caller's note on the report, log the
/// `report_note` entry, answer `204`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:write` gate; [`AppError::RouteNotFound`] for an id that
/// is no id or names no report; [`AppError::Unprocessable`] for a blank
/// or over-long `content`; [`AppError::Database`] from the writes.
async fn add_note(
    State(state): State<AppState>,
    staff: StaffUser,
    Path(raw): Path<String>,
    ApiBody(body): ApiBody<NoteBody>,
) -> AppResult<StatusCode> {
    staff.require(CLASS_WRITE, AREA_REPORTS)?;
    let row = report_by_id(&state.db, &raw).await?;
    validate_note(&body.content)?;
    let note = report::create_note(&state.db, row.id, staff.0.id, &body.content).await?;
    let target = user::get_by_id(&state.db, row.target_id).await?;
    let logged =
        moderation_log::report_note(&state.db, &staff.0, row.id, &body.content, target.as_ref())
            .await?;
    debug!(
        log = %logged.id,
        report = %row.id,
        note = %note.id,
        "admin report note added"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// The note body's checks the ticket pins: an empty text is blank, and
/// more than [`MAX_NOTE_LENGTH`] characters is over the cap — both the
/// `422` Mastodon's changeset wording states.
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described.
fn validate_note(content: &str) -> AppResult<()> {
    if content.is_empty() {
        return Err(AppError::Unprocessable(NOTE_BLANK.to_owned()));
    }
    if content.chars().count() > MAX_NOTE_LENGTH {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: Content is too long (maximum is {MAX_NOTE_LENGTH} characters)"
        )));
    }
    Ok(())
}

/// `DELETE /api/v1/pleroma/admin/reports/{report_id}/notes/{id}` —
/// Pleroma's `notes_delete`: remove the report's note, log the
/// `report_note_delete` entry with its text, answer `204`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:write` gate; [`AppError::RouteNotFound`] for a
/// `report_id` that is no id or names no report, and for a note `id`
/// that is no id or names no note *of that report*;
/// [`AppError::Database`] from the writes.
async fn delete_note(
    State(state): State<AppState>,
    staff: StaffUser,
    Path((report_raw, note_raw)): Path<(String, String)>,
) -> AppResult<StatusCode> {
    staff.require(CLASS_WRITE, AREA_REPORTS)?;
    let row = report_by_id(&state.db, &report_raw).await?;
    let Some(note_id) = parse_id(&note_raw) else {
        return Err(AppError::RouteNotFound);
    };
    let Some(note) = report::delete_note(&state.db, row.id, note_id).await? else {
        return Err(AppError::RouteNotFound);
    };
    let target = user::get_by_id(&state.db, row.target_id).await?;
    let logged = moderation_log::report_note_delete(
        &state.db,
        &staff.0,
        row.id,
        &note.content,
        target.as_ref(),
    )
    .await?;
    debug!(
        log = %logged.id,
        report = %row.id,
        note = %note.id,
        "admin report note deleted"
    );
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// entity rendering
// ---------------------------------------------------------------------------

/// The `{id}` segment of an admin report route as the stored row: a
/// malformed id and an unknown one are one 404, Pleroma's answer for a
/// report that is not there.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] as described; any database failure
/// arrives through `?` as [`AppError::Database`].
async fn report_by_id(pool: &PgPool, text: &str) -> AppResult<ReportRow> {
    let Some(id) = parse_id(text) else {
        return Err(AppError::RouteNotFound);
    };
    report::get(pool, id).await?.ok_or(AppError::RouteNotFound)
}

/// The one entity for `row`, through the batched [`entities`] read.
///
/// # Errors
///
/// As [`entities`].
async fn entity(state: &AppState, viewer: &User, row: &ReportRow) -> AppResult<AdminReport> {
    entities(state, viewer, std::slice::from_ref(row))
        .await?
        .pop()
        .ok_or_else(|| AppError::Internal("one report rendered no entity".to_owned()))
}

/// The [`AdminReport`] entities for `rows`, in order: the named accounts
/// in one read, the notes in one read, the citations in one read, and
/// every report's statuses through the shared batch renderer — with the
/// reading staff member as viewer and no visibility filter, which is what
/// lets the moderation queue show a private post the viewer follows
/// nobody into.
///
/// # Errors
///
/// [`AppError::Database`] from any read; [`AppError::Internal`] for a
/// report whose reporter or target row is gone — the foreign keys
/// cascade a report away with either account, so this names a schema
/// state that cannot occur.
async fn entities(
    state: &AppState,
    viewer: &User,
    rows: &[ReportRow],
) -> AppResult<Vec<AdminReport>> {
    let report_ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let users = named_users(state, rows).await?;
    let notes = note_lists(state, &report_ids).await?;
    let cited = report::statuses_for_reports(&state.db, &report_ids).await?;
    let mut by_report: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    let mut status_ids: Vec<Uuid> = Vec::new();
    for (report_id, status_id) in cited {
        by_report.entry(report_id).or_default().push(status_id);
        status_ids.push(status_id);
    }
    let status_rows = status::get_many(&state.db, &status_ids).await?;
    let by_id: HashMap<Uuid, &status::Status> =
        status_rows.iter().map(|row| (row.id, row)).collect();
    let mut entities = Vec::with_capacity(rows.len());
    for row in rows {
        let ids = by_report.get(&row.id).cloned().unwrap_or_default();
        let owned: Vec<status::Status> = ids
            .iter()
            .filter_map(|id| by_id.get(id).copied())
            .cloned()
            .collect();
        let statuses = render_statuses(state, ViewerMode::user(viewer), &owned).await?;
        entities.push(AdminReport::from_parts(
            AdminReportParts {
                row,
                target: named(&users, row.target_id)?,
                reporter: named(&users, row.reporter_id)?,
                assigned: row.assigned_to.and_then(|id| users.get(&id)),
                statuses,
                notes: notes.get(&row.id).cloned().unwrap_or_default(),
            },
            &state.config,
        ));
    }
    Ok(entities)
}

/// Every account the reports name — reporter, target and assignee,
/// deduplicated — keyed by id in one read.
///
/// # Errors
///
/// [`AppError::Database`] from the read.
async fn named_users(state: &AppState, rows: &[ReportRow]) -> AppResult<HashMap<Uuid, User>> {
    let mut ids: Vec<Uuid> = Vec::with_capacity(rows.len() * 3);
    for row in rows {
        ids.push(row.reporter_id);
        ids.push(row.target_id);
        ids.extend(row.assigned_to);
    }
    let users = user::get_many(&state.db, &ids).await?;
    Ok(users.into_iter().map(|user| (user.id, user)).collect())
}

/// Every note on these reports, grouped by report and rendered, in one
/// read.
///
/// # Errors
///
/// [`AppError::Database`] from the read.
async fn note_lists(
    state: &AppState,
    report_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<AdminReportNote>>> {
    let rows = report::notes_for_reports(&state.db, report_ids).await?;
    let mut notes: HashMap<Uuid, Vec<AdminReportNote>> = HashMap::new();
    for row in rows {
        let entity = AdminReportNote::from_row(&row);
        notes.entry(row.report_id).or_default().push(entity);
    }
    Ok(notes)
}

/// The account an entity slot names; its absence is the impossible state
/// the reports' foreign keys rule out, not a client failure.
///
/// # Errors
///
/// [`AppError::Internal`] as described.
fn named(users: &HashMap<Uuid, User>, id: Uuid) -> AppResult<&User> {
    users.get(&id).ok_or_else(|| {
        AppError::Internal(format!(
            "a report names the account {id}, which no users row holds"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parsed pair of defaults, for the filter tests.
    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// The body parses: Pleroma's JSON shape, the two elements the admin
    /// frontend sends at once, and the states the column accepts read as
    /// the enum they name.
    #[test]
    fn the_body_parses_pleromas_shape() {
        let full: UpdateBody = serde_json::from_value(json!({
            "reports": [
                {"id": "01934567-89ab-7cde-f012-3456789abcde", "state": "closed"},
                {"id": "01934567-89ab-7cde-f012-3456789abcd2", "state": "resolved"},
            ]
        }))
        .expect("the ticket's shape deserializes");
        assert_eq!(full.reports.len(), 2, "both elements arrive");
        assert_eq!(
            full.reports[0].state, "closed",
            "the state text is kept for the enum to parse"
        );
        let none: UpdateBody =
            serde_json::from_value(json!({"reports": []})).expect("an empty list is a valid body");
        assert!(none.reports.is_empty(), "and changes nothing");
        let missing: Result<UpdateBody, _> = serde_json::from_value(json!({}));
        assert!(missing.is_err(), "`reports` is the body's one required key");
        for state in ReportState::ALL {
            let parsed: ReportState = state
                .as_str()
                .parse()
                .unwrap_or_else(|_| panic!("`{state}` is one of the three"));
            assert_eq!(parsed, state, "the wire name round-trips");
        }
    }

    /// The refusal entry is Pleroma's `{id, error}` pair, with the id
    /// exactly as the request sent it — a malformed id is answered with
    /// the text that was sent, not with a parsed one.
    #[test]
    fn a_refusal_names_the_id_as_it_was_sent() {
        let change = ReportChange {
            id: "123".to_owned(),
            state: "bogus".to_owned(),
        };
        assert_eq!(
            refusal(&change, UNSUPPORTED_STATE),
            json!({"id": "123", "error": "Unsupported state"}),
            "the state refusal is Pleroma's wording"
        );
        assert_eq!(
            refusal(&change, NOT_FOUND),
            json!({"id": "123", "error": "not_found"}),
            "and the miss is its `:not_found` atom as text"
        );
    }

    /// The two refusals [`move_one`] can answer without touching the
    /// database: a state off the three and an id that is no id, in that
    /// order — the body's own shape is settled before anything is looked
    /// up. The pool is lazy: neither arm reaches it.
    #[tokio::test]
    async fn an_unusable_element_refuses_without_a_write() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no server");
        for (sent, wanted, reason) in [
            (
                "01934567-89ab-7cde-f012-3456789abcde",
                "bogus",
                UNSUPPORTED_STATE,
            ),
            ("not-an-id", "closed", NOT_FOUND),
        ] {
            let change = ReportChange {
                id: sent.to_owned(),
                state: wanted.to_owned(),
            };
            let outcome = move_one(&pool, &change).await.expect("no write runs");
            match outcome {
                Moved::Refused(got) => assert_eq!(got, reason, "the refusal for {sent}"),
                Moved::Row(row) => panic!("nothing was written for {sent}, got {}", row.id),
            }
        }
    }

    /// The assignment body parses Pleroma's shape, the `null` nickname
    /// and its absence alike, and [`assign_one`] refuses a malformed
    /// report id without reaching the pool.
    #[test]
    fn the_assignment_body_takes_the_nickname_or_none() {
        let body: AssignBody = serde_json::from_value(json!({
            "reports": [
                {"id": "01934567-89ab-7cde-f012-3456789abcde", "assigned_account": "moddie"},
                {"id": "01934567-89ab-7cde-f012-3456789abcd2", "assigned_account": null},
                {"id": "01934567-89ab-7cde-f012-3456789abcd3"},
            ]
        }))
        .expect("the ticket's shape deserializes");
        assert_eq!(body.reports.len(), 3, "all three elements arrive");
        assert_eq!(
            body.reports[0].assigned_account.as_deref(),
            Some("moddie"),
            "the nickname is kept for the lookup"
        );
        assert_eq!(
            body.reports[1].assigned_account, None,
            "an explicit null is the unassign"
        );
        assert_eq!(
            body.reports[2].assigned_account, None,
            "and so is an absent key"
        );
    }

    /// The listing parameters: the ticket's defaults, `page_size` over
    /// `limit` over the default, the admin cap, the lenient numbers and
    /// the strict filters.
    #[test]
    fn the_listing_defaults_and_filters_read_the_query() {
        let plain = list_params(&params(&[])).expect("an empty query parses");
        assert_eq!(plain.page, 1, "the first page");
        assert_eq!(plain.page_size, DEFAULT_WINDOW, "and the ticket's 20");
        assert!(
            plain.state.is_none() && plain.rule_id.is_none() && plain.assigned_account.is_none(),
            "no filters"
        );

        let limited = list_params(&params(&[("limit", "5")])).expect("limit parses");
        assert_eq!(limited.page_size, 5, "`limit` sets the window");
        let sized =
            list_params(&params(&[("limit", "5"), ("page_size", "9")])).expect("both parse");
        assert_eq!(
            sized.page_size, 9,
            "`page_size` wins over `limit`, Pleroma's `get_reports` order"
        );
        let capped = list_params(&params(&[("page_size", "5000")])).expect("a big page parses");
        assert_eq!(capped.page_size, 200, "and the admin cap bounds it");
        let junk = list_params(&params(&[
            ("page", "nope"),
            ("page_size", "-3"),
            ("limit", "x"),
        ]))
        .expect("unusable numbers fall through");
        assert_eq!(junk.page, 1, "a non-number page defaults");
        assert_eq!(
            junk.page_size, DEFAULT_WINDOW,
            "an unusable window defaults too"
        );

        let open = list_params(&params(&[("state", "open")])).expect("open parses");
        assert_eq!(open.state, Some(ReportState::Open), "the state filter");
        let bogus =
            list_params(&params(&[("state", "fixed")])).expect_err("an unknown state is refused");
        assert_eq!(bogus.to_string(), UNSUPPORTED_STATE, "the pinned wording");

        let rule = "01934567-89ab-7cde-f012-3456789abcde";
        let filtered =
            list_params(&params(&[("rule_id", rule), ("assigned_account", rule)])).expect("ids");
        assert_eq!(filtered.rule_id, parse_id(rule), "the rule filter");
        assert_eq!(
            filtered.assigned_account,
            parse_id(rule),
            "and the assignee filter"
        );
        for key in ["rule_id", "assigned_account"] {
            let err = list_params(&params(&[(key, "not-a-uuid")]))
                .expect_err("a broken id cannot filter");
            assert_eq!(err.to_string(), INVALID_ID, "the refusal for {key}");
        }
    }

    /// The note body's two refusals and its boundary: blank is blank, the
    /// cap counts characters, and it is inclusive.
    #[test]
    fn the_note_body_validates_against_the_cap() {
        let blank = validate_note("").expect_err("a blank note is refused");
        assert_eq!(blank.to_string(), NOTE_BLANK, "the blank wording");
        let edge = "x".repeat(MAX_NOTE_LENGTH);
        validate_note(&edge).expect("the cap itself is allowed");
        let over =
            validate_note(&format!("{edge}!")).expect_err("one character past the cap is not");
        assert_eq!(
            over.to_string(),
            "Validation failed: Content is too long (maximum is 5000 characters)",
            "the wording names the cap"
        );
        let multibyte = "é".repeat(MAX_NOTE_LENGTH);
        validate_note(&multibyte).expect("characters, not bytes");
    }

    /// The note body demands its `content`.
    #[test]
    fn the_note_body_requires_content() {
        let missing: Result<NoteBody, _> = serde_json::from_value(json!({}));
        assert!(missing.is_err(), "`content` is required");
        let body: NoteBody =
            serde_json::from_value(json!({"content": "looked"})).expect("the shape parses");
        assert_eq!(body.content, "looked", "and arrives verbatim");
    }

    /// A malformed path segment is the 404 before any read would run;
    /// the unknown-but-well-formed id reaches the database, and the HTTP
    /// tests pin that half.
    #[tokio::test]
    async fn junk_path_segments_are_the_one_404_before_any_read() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no server");
        for raw in ["not-a-uuid", "", "01890b9a"] {
            let err = report_by_id(&pool, raw)
                .await
                .expect_err("no id text can resolve");
            assert!(
                matches!(err, AppError::RouteNotFound),
                "the malformed-id refusal is the 404 Not found: {err}"
            );
        }
    }
}
