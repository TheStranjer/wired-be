//! The [`AdminReport`] entity rendering shared by the report routes.

use std::collections::HashMap;

use uuid::Uuid;

use crate::{
    api::report_render::{cited_statuses, report_account, report_accounts},
    entities::admin_report::{AdminReport, AdminReportNote, AdminReportParts},
    error::{AppError, AppResult},
    models::{
        report::{self, Report as ReportRow},
        user::User,
    },
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
    let users = report_accounts(state, rows).await?;
    let notes = note_lists(state, &report_ids).await?;
    let mut cited = cited_statuses(state, viewer, rows).await?;
    let mut entities = Vec::with_capacity(rows.len());
    for row in rows {
        entities.push(AdminReport::from_parts(
            AdminReportParts {
                row,
                target: report_account(&users, row.target_id)?,
                reporter: report_account(&users, row.reporter_id)?,
                assigned: row.assigned_to.and_then(|id| users.get(&id)),
                statuses: cited.remove(&row.id).unwrap_or_default(),
                notes: notes.get(&row.id).cloned().unwrap_or_default(),
            },
            &state.config,
        ));
    }
    Ok(entities)
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
