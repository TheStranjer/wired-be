//! The bulk state change of `PATCH /reports` and the follow-up each moved
//! report owes.

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse as _, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;
use uuid::Uuid;

use super::{AREA_REPORTS, CLASS_WRITE, NOT_FOUND, UNSUPPORTED_STATE};
use crate::{
    auth::admin::StaffUser,
    error::AppResult,
    extract::ApiBody,
    models::{report::ReportState, user::User},
    services::report_moderation::set_report_state,
    state::AppState,
    util::id::parse_id,
};

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
pub(super) struct UpdateBody {
    /// Every element the request changes; an empty list changes nothing.
    reports: Vec<ReportChange>,
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
pub(super) async fn update_reports(
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
/// [`AppError::Database`] from [`set_report_state`]'s write, read or log.
async fn move_all(
    state: &AppState,
    actor: &User,
    changes: &[ReportChange],
) -> AppResult<Vec<Value>> {
    let mut failures: Vec<Value> = Vec::with_capacity(changes.len());
    for change in changes {
        let refused = match parse_change(change) {
            Ok((id, to)) => set_report_state(state, actor, id, to)
                .await?
                .is_none()
                .then_some(NOT_FOUND),
            Err(reason) => Some(reason),
        };
        if let Some(reason) = refused {
            failures.push(refusal(change, reason));
        }
    }
    Ok(failures)
}

/// One element's own shape check: the state check, then the id check. A
/// state off the three or an id that is no id is the refusal text the
/// element is answered with — the other elements of the request are still
/// applied.
fn parse_change(change: &ReportChange) -> Result<(Uuid, ReportState), &'static str> {
    let to = change
        .state
        .parse::<ReportState>()
        .map_err(|_unknown| UNSUPPORTED_STATE)?;
    let id = parse_id(&change.id).ok_or(NOT_FOUND)?;
    Ok((id, to))
}

/// Pleroma's per-item failure entry: the id as the request sent it and
/// the message that says why it moved nothing.
fn refusal(change: &ReportChange, reason: &str) -> Value {
    json!({"id": change.id, "error": reason})
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The two refusals [`parse_change`] can answer: a state off the three
    /// and an id that is no id, in that order — the body's own shape is
    /// settled before anything is looked up — and the pair a good element
    /// parses to.
    #[test]
    fn an_unusable_element_is_refused_before_any_write() {
        for (sent, wanted, reason) in [
            (
                "01934567-89ab-7cde-f012-3456789abcde",
                "bogus",
                UNSUPPORTED_STATE,
            ),
            ("not-an-id", "closed", NOT_FOUND),
            ("not-an-id", "bogus", UNSUPPORTED_STATE),
        ] {
            let change = ReportChange {
                id: sent.to_owned(),
                state: wanted.to_owned(),
            };
            assert_eq!(
                parse_change(&change),
                Err(reason),
                "the refusal for {sent} / {wanted}"
            );
        }
        let id = "01934567-89ab-7cde-f012-3456789abcde";
        let good = ReportChange {
            id: id.to_owned(),
            state: "resolved".to_owned(),
        };
        assert_eq!(
            parse_change(&good),
            Ok((parse_id(id).expect("a valid id"), ReportState::Resolved)),
            "a usable element parses"
        );
    }
}
