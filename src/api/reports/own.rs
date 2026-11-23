//! The reporter's own reports over HTTP: `GET /api/v0/pleroma/reports` and
//! `GET /api/v0/pleroma/reports/{id}`.
//!
//! Pleroma serves these paths from `PleromaAPI.ReportController`
//! (`lib/pleroma/web/pleroma_api/controllers/report_controller.ex`) behind
//! its `:authenticated_api` pipeline and a `read:reports` scope: `index`
//! forces `actor_id` to the caller, so a user lists exactly what they filed,
//! and `show` answers 404 unless the report's actor is the caller. The
//! entity is `PleromaAPI.ReportView`'s: `id`, `account` (the reported
//! account), `actor` (the reporter), `content`, `created_at`, `statuses`
//! and `state` — nothing of the moderators' side (no notes, no assignee, no
//! action fields), and `{reports, total}` for the list.
//!
//! The cited statuses are filtered through
//! [`status_visibility::filter_visible`] for the reporter and rendered with
//! [`ViewerMode::user`], so a post that has since become invisible to them
//! drops out of the entity instead of leaking through their own report. The
//! staff view of the same ledger is [`crate::api::admin::reports`].
//!
//! The ids are this schema's `UUIDv7` text (decision 4).

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    entities::{account::Account, status::Status},
    error::{AppError, AppResult},
    models::{
        report::{self, DEFAULT_OWN_PAGE_SIZE, MAX_OWN_PAGE_SIZE, Report as ReportRow},
        status, status_visibility,
        user::{self, User},
    },
    oauth::scopes::READ_REPORTS,
    services::render_status::{ViewerMode, render_statuses},
    state::AppState,
    util::{id::parse_id, time::mastodon_time},
};

/// One report as its own reporter reads it back: Pleroma's
/// `PleromaAPI.ReportView` `"show.json"`.
#[derive(Debug, Serialize)]
pub(super) struct OwnReport {
    /// The row's `UUIDv7` as lowercase hyphenated text.
    pub(super) id: String,
    /// The reported account, as the reporter sees it.
    pub(super) account: Account,
    /// The reporting account: the reader themselves.
    pub(super) actor: Account,
    /// The reporter's own words, verbatim.
    pub(super) content: String,
    /// When the report was filed, in the Mastodon time format.
    pub(super) created_at: String,
    /// The cited statuses still visible to the reporter, oldest first.
    pub(super) statuses: Vec<Status>,
    /// Where the report stands: `open`, `closed` or `resolved`.
    pub(super) state: String,
}

/// The `{reports, total}` answer of the list.
#[derive(Debug, Serialize)]
pub(super) struct OwnReportPage {
    /// The page, newest filing first.
    pub(super) reports: Vec<OwnReport>,
    /// How many reports the caller filed, pages included.
    pub(super) total: i64,
}

/// The `/api/v0/pleroma` reports router, nested by [`crate::api`].
pub fn own_router() -> Router<AppState> {
    Router::new()
        .route("/reports", get(list_own))
        .route("/reports/{id}", get(show_own))
}

/// `GET /api/v0/pleroma/reports` — the caller's own filed reports, newest
/// first, as `{reports, total}`. `page` counts from 1 and `page_size` (or
/// its synonym `limit`) defaults to 20 and is capped at 200; an unusable
/// value reads as the default.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read:reports` check; [`AppError::Database`] from the reads.
async fn list_own(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<HashMap<String, String>>,
) -> AppResult<Json<OwnReportPage>> {
    auth.require(READ_REPORTS)?;
    let size = positive(&params, "page_size")
        .or_else(|| positive(&params, "limit"))
        .unwrap_or(DEFAULT_OWN_PAGE_SIZE)
        .min(MAX_OWN_PAGE_SIZE);
    let page = positive(&params, "page").unwrap_or(1);
    let listed = report::list_for_reporter(&state.db, auth.0.id, page, size).await?;
    let reports = entities(&state, &auth.0, &listed.reports).await?;
    Ok(Json(OwnReportPage {
        reports,
        total: listed.total,
    }))
}

/// `GET /api/v0/pleroma/reports/{id}` — one report the caller filed.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read:reports` check; [`AppError::RouteNotFound`] (`{"error":
/// "Not found"}`) for an id that is no id, names no report, or names a
/// report somebody else filed; [`AppError::Database`] from the reads.
async fn show_own(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(raw): Path<String>,
) -> AppResult<Json<OwnReport>> {
    auth.require(READ_REPORTS)?;
    let id = parse_id(&raw).ok_or(AppError::RouteNotFound)?;
    let row = report::get(&state.db, id)
        .await?
        .filter(|row| row.reporter_id == auth.0.id)
        .ok_or(AppError::RouteNotFound)?;
    entities(&state, &auth.0, std::slice::from_ref(&row))
        .await?
        .pop()
        .map(Json)
        .ok_or_else(|| AppError::Internal("one report rendered no entity".to_owned()))
}

/// The query value of `key` as a number of at least 1, `None` when absent
/// or unusable.
fn positive(params: &HashMap<String, String>, key: &str) -> Option<i64> {
    params
        .get(key)
        .and_then(|text| text.trim().parse::<i64>().ok())
        .filter(|value| *value >= 1)
}

/// The [`OwnReport`] entities for `rows` (all filed by `reporter`), in
/// order: the reported accounts in one read, the citations in one read, and
/// each report's statuses filtered for and rendered as `reporter`.
///
/// # Errors
///
/// [`AppError::Database`] from any read; [`AppError::Internal`] for a
/// report whose target row is gone, which the foreign key's cascade rules
/// out.
async fn entities(
    state: &AppState,
    reporter: &User,
    rows: &[ReportRow],
) -> AppResult<Vec<OwnReport>> {
    let target_ids: Vec<Uuid> = rows.iter().map(|row| row.target_id).collect();
    let targets: HashMap<Uuid, User> = user::get_many(&state.db, &target_ids)
        .await?
        .into_iter()
        .map(|target| (target.id, target))
        .collect();
    let report_ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let mut cited: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    let mut status_ids: Vec<Uuid> = Vec::new();
    for (report_id, status_id) in report::statuses_for_reports(&state.db, &report_ids).await? {
        cited.entry(report_id).or_default().push(status_id);
        status_ids.push(status_id);
    }
    let status_rows = status::get_many(&state.db, &status_ids).await?;
    let by_id: HashMap<Uuid, &status::Status> =
        status_rows.iter().map(|row| (row.id, row)).collect();
    let mut entities = Vec::with_capacity(rows.len());
    for row in rows {
        let owned: Vec<status::Status> = cited
            .get(&row.id)
            .into_iter()
            .flatten()
            .filter_map(|id| by_id.get(id).copied())
            .cloned()
            .collect();
        let visible = status_visibility::filter_visible(&state.db, owned, Some(reporter)).await?;
        let statuses = render_statuses(state, ViewerMode::user(reporter), &visible).await?;
        let target = targets.get(&row.target_id).ok_or_else(|| {
            AppError::Internal(format!(
                "a report names the account {}, which no users row holds",
                row.target_id
            ))
        })?;
        entities.push(OwnReport {
            id: row.id.to_string(),
            account: Account::from_user_for(Some(reporter), target, &state.config, None),
            actor: Account::from_user_for(Some(reporter), reporter, &state.config, None),
            content: row.comment.clone(),
            created_at: mastodon_time(&row.inserted_at),
            statuses,
            state: row.state.as_str().to_owned(),
        });
    }
    Ok(entities)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The query numbers: at least 1, anything else is absent.
    #[test]
    fn positive_reads_only_numbers_of_at_least_one() {
        let params: HashMap<String, String> = [("a", "5"), ("b", "0"), ("c", "x"), ("d", " 7 ")]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect();
        assert_eq!(positive(&params, "a"), Some(5), "a plain number");
        assert_eq!(positive(&params, "b"), None, "zero is no window");
        assert_eq!(positive(&params, "c"), None, "junk is absent");
        assert_eq!(positive(&params, "d"), Some(7), "whitespace is trimmed");
        assert_eq!(positive(&params, "e"), None, "a missing key is absent");
    }
}
