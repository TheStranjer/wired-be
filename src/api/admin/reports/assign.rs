//! The bulk assignment of `POST /reports/assign_account`.

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

use super::{AREA_REPORTS, CLASS_WRITE, NOT_FOUND, render::entities};
use crate::{
    auth::admin::StaffUser,
    error::AppResult,
    extract::ApiBody,
    models::{
        report::Report as ReportRow,
        user::{self, User},
    },
    services::report_moderation::assign_report,
    state::AppState,
    util::id::parse_id,
};

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
pub(super) struct AssignBody {
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
/// `assign_account`: move each named report to its named moderator, record
/// one `report_assigned` / `report_unassigned` moderation-log entry per
/// move (Pleroma's `do_assign_account` clauses write the same two actions
/// its `ModerationLogView` renders), and answer the updated [`AdminReport`]
/// entities, or the `400` list of what resolved to nothing.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:write` gate; [`AppError::Database`] from a write that
/// failed for a reason the request cannot be blamed for.
pub(super) async fn assign_account(
    State(state): State<AppState>,
    staff: StaffUser,
    ApiBody(body): ApiBody<AssignBody>,
) -> AppResult<Response> {
    staff.require(CLASS_WRITE, AREA_REPORTS)?;
    let (moved, failures) = assign_all(&state.db, &staff.0, &body.reports).await?;
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
/// resolved, beside the failure entries it produced. Every resolved
/// element records its moderation-log entry as it moves.
///
/// # Errors
///
/// [`AppError::Database`] from any read or write [`assign_one`] reaches.
async fn assign_all(
    pool: &PgPool,
    actor: &User,
    changes: &[ReportAssignment],
) -> AppResult<(Vec<ReportRow>, Vec<Value>)> {
    let mut moved: Vec<ReportRow> = Vec::with_capacity(changes.len());
    let mut failures: Vec<Value> = Vec::new();
    for change in changes {
        match assign_one(pool, actor, change).await? {
            Assigned::Row(row) => moved.push(row),
            Assigned::Refused(reason) => {
                failures.push(json!({"id": change.id, "error": reason}));
            }
        }
    }
    Ok((moved, failures))
}

/// One report's assignment: the id check, the nickname lookup, the write
/// and the `report_assigned` / `report_unassigned` entry the move owes.
///
/// # Errors
///
/// [`AppError::Database`] when a read or write itself failed; an id that
/// is no id, a report that names nothing or a nickname no account carries
/// is [`Assigned::Refused`] with Pleroma's `not_found`, not an error —
/// the other elements of the request are still applied.
async fn assign_one(pool: &PgPool, actor: &User, change: &ReportAssignment) -> AppResult<Assigned> {
    let Some(id) = parse_id(&change.id) else {
        return Ok(Assigned::Refused(NOT_FOUND));
    };
    let assignee = match change.assigned_account.as_deref() {
        Some(nickname) => {
            let Some(found) = user::get_by_nickname(pool, nickname).await? else {
                return Ok(Assigned::Refused(NOT_FOUND));
            };
            Some(found)
        }
        None => None,
    };
    let Some(row) = assign_report(pool, actor, id, assignee.as_ref()).await? else {
        return Ok(Assigned::Refused(NOT_FOUND));
    };
    Ok(Assigned::Row(row))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
