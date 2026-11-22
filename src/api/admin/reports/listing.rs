//! The report reads: the filtered listing, the single report and the
//! lookup of a report by its path segment.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
};
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use super::{
    AREA_REPORTS, CLASS_READ, DEFAULT_WINDOW, INVALID_ID, UNSUPPORTED_STATE,
    render::{entities, entity},
};
use crate::{
    auth::admin::StaffUser,
    entities::admin_report::{AdminReport, AdminReportPage},
    error::{AppError, AppResult},
    models::report::{self, Report as ReportRow, ReportState},
    state::AppState,
    util::id::parse_id,
};

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
pub(super) async fn list_reports(
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
pub(super) async fn show_report(
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
        page: super::super::page_number(params, "page", super::super::DEFAULT_PAGE),
        page_size: window_size(params),
    })
}

/// The `state` filter: absent or empty is every state, anything the enum
/// does not name is the 400 [`UNSUPPORTED_STATE`].
fn state_filter(params: &HashMap<String, String>) -> AppResult<Option<ReportState>> {
    super::super::text_of(params, "state")
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
    super::super::text_of(params, key)
        .map(|raw| parse_id(&raw).ok_or_else(|| AppError::BadRequest(INVALID_ID.to_owned())))
        .transpose()
}

/// The listing window: `page_size` wins, `limit` is its synonym, and
/// neither (or an unusable value of either) is the ticket's default of
/// [`DEFAULT_WINDOW`]; the admin cap of [`super::super::MAX_PAGE_SIZE`] bounds
/// an explicit value.
fn window_size(params: &HashMap<String, String>) -> i64 {
    let positive = |key: &str| {
        super::super::text_of(params, key)
            .as_deref()
            .and_then(|text| text.parse::<i64>().ok())
            .filter(|value| *value >= 1)
    };
    positive("page_size")
        .or_else(|| positive("limit"))
        .unwrap_or(DEFAULT_WINDOW)
        .min(super::super::MAX_PAGE_SIZE)
}

/// The `{id}` segment of an admin report route as the stored row: a
/// malformed id and an unknown one are one 404, Pleroma's answer for a
/// report that is not there.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] as described; any database failure
/// arrives through `?` as [`AppError::Database`].
pub(super) async fn report_by_id(pool: &PgPool, text: &str) -> AppResult<ReportRow> {
    let Some(id) = parse_id(text) else {
        return Err(AppError::RouteNotFound);
    };
    report::get(pool, id).await?.ok_or(AppError::RouteNotFound)
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
