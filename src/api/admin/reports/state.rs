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
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use super::{AREA_REPORTS, CLASS_WRITE, NOT_FOUND, UNSUPPORTED_STATE};
use crate::{
    auth::admin::StaffUser,
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
/// [`AppError::Database`] from [`move_one`]'s write or from [`follow_up`].
async fn move_all(
    state: &AppState,
    actor: &User,
    changes: &[ReportChange],
) -> AppResult<Vec<Value>> {
    let mut failures: Vec<Value> = Vec::with_capacity(changes.len());
    for change in changes {
        match move_one(&state.db, Some(actor.id), change).await? {
            Moved::Row(row) => follow_up(state, actor, &row).await?,
            Moved::Refused(reason) => failures.push(refusal(change, reason)),
        }
    }
    Ok(failures)
}

/// One report's move: the state check, the id check and the write. A move
/// to `resolved` names `acted_by` as the moderator who took the action
/// ([`report::set_state`] keeps the first name it was given); the other
/// two states decide the action columns on their own.
///
/// # Errors
///
/// [`AppError::Database`] when the write itself failed; a state off the
/// three or an id that names no report is [`Moved::Refused`], not an
/// error — the other elements of the request are still applied.
async fn move_one(
    pool: &PgPool,
    acted_by: Option<Uuid>,
    change: &ReportChange,
) -> AppResult<Moved> {
    let Ok(state_name) = change.state.parse::<ReportState>() else {
        return Ok(Moved::Refused(UNSUPPORTED_STATE));
    };
    let Some(id) = parse_id(&change.id) else {
        return Ok(Moved::Refused(NOT_FOUND));
    };
    let row = report::set_state(pool, id, state_name, acted_by).await?;
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
            let outcome = move_one(&pool, None, &change).await.expect("no write runs");
            match outcome {
                Moved::Refused(got) => assert_eq!(got, reason, "the refusal for {sent}"),
                Moved::Row(row) => panic!("nothing was written for {sent}, got {}", row.id),
            }
        }
    }
}
