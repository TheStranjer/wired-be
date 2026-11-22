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

mod assign;
mod listing;
mod notes;
mod render;
mod state;

use axum::{
    Router,
    routing::{delete, get, post},
};

use self::{
    assign::assign_account,
    listing::{list_reports, show_report},
    notes::{add_note, delete_note},
    state::update_reports,
};
use crate::state::AppState;

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
