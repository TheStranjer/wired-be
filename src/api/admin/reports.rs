//! The admin report endpoint: moving a filed report through moderation,
//! under `/api/v1/pleroma/admin/reports` (WBE-212).
//!
//! The reference is Pleroma's `AdminAPI.ReportController`
//! (`lib/pleroma/web/admin_api/controllers/report_controller.ex`), routed
//! `patch "/reports"` in `lib/pleroma/web/router.ex` and gated by
//! `admin:write:reports` — the same gate as the [`AdminUser::require`] call
//! the handler makes first, on the area `reports`. Pleroma's `index`/`show`
//! pair, its assignment routes and its report notes belong to the tickets
//! that read the ledger; this module is the one write the event-delivery
//! ticket needs: the state change every `report.updated` webhook announces.
//!
//! - `PATCH /reports` (`update`) — Pleroma's bulk state change: a body of
//!   `{"reports": [{"id": …, "state": …}]}`, one report per element, each
//!   moved by [`report::set_state`] (which stamps `action_taken_at` on a
//!   resolution and clears it on a reopening). Every element that moved gets
//!   its `report_update` [`moderation_log`] entry, its
//!   [`webhooks::report_updated`] event and nothing else; every element that
//!   did not — an id that names no report, a state off `open`/`closed`/
//!   `resolved` — is answered with Pleroma's `{"id": …, "error": …}` entry
//!   and a `400`, while the ones that did move stay moved: Pleroma's `update`
//!   runs no transaction over its `Enum.map`, and neither does this. An
//!   answer with no failure is the `204` Pleroma's `json_response(:no_content)`
//!   gives.
//!
//! The ids are this schema's `UUIDv7` text rather than Pleroma's integers
//! (decision 4); a malformed one names no report and is the same
//! `not_found` Pleroma's `CommonAPI.update_report_state` answers with.
//!
//! [`AdminUser::require`]: crate::auth::admin::AdminUser::require
//! [`moderation_log`]: crate::models::moderation_log
//! [`report::set_state`]: crate::models::report::set_state
//! [`webhooks::report_updated`]: crate::services::webhooks::report_updated

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse as _, Response},
    routing::patch,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::debug;

use crate::{
    auth::admin::AdminUser,
    error::AppResult,
    extract::ApiBody,
    models::{
        moderation_log,
        report::{self, Report as ReportRow, ReportState},
        user::{self, User},
    },
    services::webhooks,
    state::AppState,
    util::id::parse_id,
};

/// The write endpoint class of [`AdminUser::require`].
const CLASS_WRITE: &str = "write";
/// The admin scope area this endpoint lives in (`admin:write:reports`,
/// Pleroma's `OAuthScopesPlug` grant for `update`).
const AREA_REPORTS: &str = "reports";
/// The per-item error of a `state` outside the three the column allows —
/// Pleroma's `Utils.update_report_state` falls through to
/// `{:error, "Unsupported state"}`.
const UNSUPPORTED_STATE: &str = "Unsupported state";
/// The per-item error of an id that is no id or names no report: Pleroma's
/// `{:error, :not_found}`, which its JSON renders as this atom's text.
const NOT_FOUND: &str = "not_found";

/// The `/reports` routes of the admin API, merged into [`super`].
pub fn router() -> Router<AppState> {
    Router::new().route("/reports", patch(update_reports))
}

/// One element of the request's `reports` array: the report to move and the
/// state to move it to.
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
/// named report to its new state, log and announce the ones that moved, and
/// answer `204` or the `400` list of what did not move.
///
/// # Errors
///
/// [`crate::error::AppError::Unauthorized`] /
/// [`crate::error::AppError::Forbidden`] from [`AdminUser`] and its
/// `admin:write` gate; [`crate::error::AppError::Database`] from a write that
/// failed for a reason the request cannot be blamed for.
async fn update_reports(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<UpdateBody>,
) -> AppResult<Response> {
    admin.require(CLASS_WRITE, AREA_REPORTS)?;
    let failures = move_all(&state, &admin.0, &body.reports).await?;
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
/// [`crate::error::AppError::Database`] from [`move_one`]'s write or from
/// [`follow_up`].
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
/// [`crate::error::AppError::Database`] when the write itself failed; a state off the three
/// or an id that names no report is [`Moved::Refused`], not an error — the
/// other elements of the request are still applied.
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

/// What a moved report owes: its `report_update` moderation-log entry and its
/// `report.updated` event (the webhook fan-out is a queue write, so a
/// subscribed endpoint costs this request nothing but the insert).
///
/// # Errors
///
/// [`crate::error::AppError::Database`] from either read or write.
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

/// Pleroma's per-item failure entry: the id as the request sent it and the
/// message that says why it moved nothing.
fn refusal(change: &ReportChange, reason: &str) -> Value {
    json!({"id": change.id, "error": reason})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body parses: Pleroma's JSON shape, the two elements the admin
    /// frontend sends at once, and the states the column accepts read as the
    /// enum they name.
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

    /// The refusal entry is Pleroma's `{id, error}` pair, with the id exactly
    /// as the request sent it — a malformed id is answered with the text that
    /// was sent, not with a parsed one.
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

    /// The two refusals [`move_one`] can answer without touching the database:
    /// a state off the three and an id that is no id, in that order — the
    /// body's own shape is settled before anything is looked up. The pool is
    /// lazy: neither arm reaches it.
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
}
