//! The [`AdminReport`] entity rendering shared by the report routes.

use std::collections::HashMap;

use uuid::Uuid;

use crate::{
    entities::admin_report::{AdminReport, AdminReportNote, AdminReportParts},
    error::{AppError, AppResult},
    models::{
        report::{self, Report as ReportRow},
        status,
        user::{self, User},
    },
    services::render_status::{ViewerMode, render_statuses},
    state::AppState,
};

/// The one entity for `row`, through the batched [`entities`] read.
///
/// # Errors
///
/// As [`entities`].
pub(super) async fn entity(
    state: &AppState,
    viewer: &User,
    row: &ReportRow,
) -> AppResult<AdminReport> {
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
pub(super) async fn entities(
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
