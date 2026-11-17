//! The Mastodon admin API's report endpoints over HTTP: the queue reads
//! and the four moderation writes under `/api/v1/admin/reports` (WBE-214).
//!
//! The reference is `Pleroma.Web.MastodonAPI.Admin.ReportController`
//! (`lib/pleroma/web/mastodon_api/admin/controllers/report_controller.ex`)
//! over its `ReportView`
//! (`lib/pleroma/web/mastodon_api/admin/views/report_view.ex`), routed in
//! `lib/pleroma/web/router.ex` under `/api/v1/admin` beside the account
//! routes of [`crate::api::mastodon_admin`] — which this file joins as the
//! sibling [`v1_router`] merged onto the same `/admin` nest.
//!
//! The controller runs the `admin_api` pipeline, whose gate plug is
//! `UserIsStaffPlug`, and its `OAuthScopesPlug` pair: `admin:read:reports`
//! on the two reads, `admin:write:reports` on the four writes. Every route
//! here therefore takes the [`StaffUser`] extractor — a moderator with the
//! area scope is Pleroma's expected caller — and opens with the scope half
//! of the same pipeline: [`StaffUser::require`] with `read` or `write` on
//! the `reports` area, so `admin:read` and `admin:read:reports` read,
//! `admin:write` and `admin:write:reports` write, and nothing else does.
//! Unlike the account surface's five writes, which the WBE-213 ticket
//! pinned to [`AdminUser`], these four stay Pleroma's staff writes.
//!
//! The six routes:
//!
//! - `GET /reports` (`index`) — the [`report::mastodon_list`] page: the
//!   ticket's `resolved` filter reads the state pair (true keeps `resolved`
//!   *and* `closed`, the two states its `action_taken` answers `true` for;
//!   false and absence keep the open queue), and `account_id` and
//!   `target_account_id` weigh the reporter and the reported. The page is
//!   [`PageParams`] keyset over the report's own id (decision 4) under
//!   Mastodon's admin page sizes — a hundred rows by default, at most two
//!   hundred — answered as the [`MastodonAdminReport`] entity array under
//!   the `Link` header its cursors belong to, keyed on the page's first
//!   and last id with the request's own filters repeated (Pleroma's
//!   `build_pagination_fields` rule the shared [`link_query`] implements).
//! - `GET /reports/{id}` (`show`) — the one entity the id names. `{id}` is
//!   a row id and nothing else: text that is no id and an id that names no
//!   report are the same 404 `{"error":"Record not found"}`, Pleroma's
//!   `:not_found` fallback as the ticket spells it.
//! - `POST /reports/{id}/resolve` (`resolve`) — [`report::set_state`] to
//!   `resolved`, which stamps [`action_taken_at`](ReportRow::action_taken_at)
//!   and names the acting staff member in
//!   [`action_taken_by`](ReportRow::action_taken_by) (each once: a repeat
//!   keeps the first stamp and the first actor), beside its
//!   `report_update` [`moderation_log`] entry and the `report.updated`
//!   [`webhooks`] event every other state change of this table emits. The
//!   fresh row's entity is the answer.
//! - `POST /reports/{id}/reopen` (`reopen`) — the same write to `open`,
//!   which clears both action columns; the entity answers with
//!   `action_taken` false and both action slots null.
//! - `POST /reports/{id}/assign_to_self` (`assign_to_self`) —
//!   [`report::assign`] naming the reading staff member as the owner, and
//!   Pleroma's `report_assigned` entry, whose sentence names the same
//!   moderator the write stored.
//! - `POST /reports/{id}/unassign` (`unassign`) — the pair's `None` arm,
//!   handing the report back to the unassigned queue beside its
//!   `report_unassigned` entry.
//!
//! Every write answers the rewritten row's entity, so the console reads
//! the effect of its action off the same shape it listed. Assignment is
//! not a state change: neither assign route moves the [`state`], and
//! neither state route moves the assignment.
//!
//! The entity is the ticket's fourteen keys — Pleroma's `ReportView`
//! `"show.json"` read against this schema's real columns, where its
//! hardcoded `category` and `forwarded` and its always-`nil`
//! `action_taken_by_account` have rows to read (see
//! [`entities::mastodon_admin_report`](crate::entities::mastodon_admin_report)).
//! The ids are this schema's `UUIDv7` text rather than Pleroma's integers
//! (decision 4).
//!
//! [`StaffUser`]: crate::auth::admin::StaffUser
//! [`AdminUser`]: crate::auth::admin::AdminUser
//! [`StaffUser::require`]: crate::auth::admin::StaffUser::require
//! [`MastodonAdminReport`]: crate::entities::mastodon_admin_report::MastodonAdminReport
//! [`report::mastodon_list`]: crate::models::report::mastodon_list
//! [`report::set_state`]: crate::models::report::set_state
//! [`report::assign`]: crate::models::report::assign
//! [`moderation_log`]: crate::models::moderation_log
//! [`webhooks`]: crate::services::webhooks
//! [`state`]: ReportRow::state
//! [`ReportRow`]: crate::models::report::Report

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, Uri},
    routing::{get, post},
};
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::{QueryPairs, link_query},
    auth::admin::StaffUser,
    entities::{
        mastodon_admin_report::{
            MastodonAdminReport, MastodonAdminReportParts, MastodonAdminReportRule,
        },
        status::Status,
    },
    error::{AppError, AppResult},
    models::{
        instance_rule, moderation_log,
        report::{self, Report as ReportRow, ReportState},
        status, user,
        user::User,
    },
    pagination::{PageParams, link_header, set_link_header},
    services::{
        render_status::{ViewerMode, render_statuses},
        webhooks,
    },
    state::AppState,
    util::id::parse_id,
};

/// The read endpoint class of [`StaffUser::require`].
const CLASS_READ: &str = "read";
/// The write endpoint class of [`StaffUser::require`]: the four moderation
/// routes are gated by it, the two reads by [`CLASS_READ`] (Pleroma's
/// `OAuthScopesPlug` pair on this controller).
const CLASS_WRITE: &str = "write";
/// The admin scope area these endpoints live in (`admin:read:reports` and
/// `admin:write:reports`, Pleroma's `OAuthScopesPlug` grants for the six
/// actions).
const AREA_REPORTS: &str = "reports";
/// Default page size when `limit` is absent or unusable: Mastodon's admin
/// listing default, where Pleroma's global [`PageParams`] default is 20 —
/// the same pair the account listings of
/// [`crate::api::mastodon_admin`] state.
const DEFAULT_LIMIT: i64 = 100;
/// Page size cap: the bound the ticket pins for this listing.
const MAX_LIMIT: i64 = 200;
/// The 400 body of a filter id that is no id — the refusal every other
/// id-filtered listing here gives, from the moderation log's `user_id`
/// filter to the Pleroma admin report listing's `rule_id`.
const INVALID_ID: &str = "Invalid id";

/// The `/reports` routes of the Mastodon admin API v1, merged onto the
/// `/admin` nest of [`crate::api::v1`] beside
/// [`crate::api::mastodon_admin`]'s account routes.
///
/// The four action suffixes are static segments beside the parameter
/// `/reports/{id}` the read occupies: axum prefers the longer
/// static match, so `POST /reports/{id}/resolve` reaches the resolve
/// handler while `GET /reports/{id}` still reaches the show one — the same
/// pair of shapes the account surface hangs off each other.
pub fn v1_router() -> Router<AppState> {
    Router::new()
        .route("/reports", get(list_reports))
        .route("/reports/{id}", get(show_report))
        .route("/reports/{id}/resolve", post(resolve_report))
        .route("/reports/{id}/reopen", post(reopen_report))
        .route("/reports/{id}/assign_to_self", post(assign_to_self))
        .route("/reports/{id}/unassign", post(unassign_report))
}

/// `GET /api/v1/admin/reports` — Pleroma's `index`: the page of reports
/// the request's filters select, newest filing first, under the `Link`
/// header its cursors belong to.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:read` gate; [`AppError::BadRequest`] for a malformed
/// pagination cursor or a filter that is no id; [`AppError::Database`]
/// from the listing read; and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn list_reports(
    State(state): State<AppState>,
    staff: StaffUser,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<MastodonAdminReport>>)> {
    staff.require(CLASS_READ, AREA_REPORTS)?;
    let params = list_params(&QueryPairs::parse(uri.query()))?;
    let page = PageParams::from_query_with_default(uri.query(), DEFAULT_LIMIT, MAX_LIMIT)?;
    let rows = report::mastodon_list(&state.db, &params, page).await?;
    debug!(listed = rows.len(), "admin mastodon reports listed");
    let entities = entities(&state, &staff.0, &rows).await?;
    let link = page_link(&state, &uri, &rows);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    Ok((headers, Json(entities)))
}

/// `GET /api/v1/admin/reports/{id}` — Pleroma's `show`: the one entity the
/// id names.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:read` gate; [`AppError::NotFound`] from [`report_by_id`];
/// [`AppError::Database`] from the reads.
async fn show_report(
    State(state): State<AppState>,
    staff: StaffUser,
    Path(raw): Path<String>,
) -> AppResult<Json<MastodonAdminReport>> {
    staff.require(CLASS_READ, AREA_REPORTS)?;
    let row = report_by_id(&state.db, &raw).await?;
    entity(&state, &staff.0, &row).await.map(Json)
}

/// `POST /api/v1/admin/reports/{id}/resolve` — Pleroma's `resolve`: move
/// the report to `resolved`, stamping the action and naming the acting
/// staff member, and answer the fresh entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:write` gate; [`AppError::NotFound`] from [`report_by_id`]
/// or from the write matching no row; [`AppError::Database`] from the
/// writes and the log.
async fn resolve_report(
    State(state): State<AppState>,
    staff: StaffUser,
    Path(raw): Path<String>,
) -> AppResult<Json<MastodonAdminReport>> {
    staff.require(CLASS_WRITE, AREA_REPORTS)?;
    let filed = report_by_id(&state.db, &raw).await?;
    let row = move_state(&state, &staff.0, &filed, ReportState::Resolved).await?;
    entity(&state, &staff.0, &row).await.map(Json)
}

/// `POST /api/v1/admin/reports/{id}/reopen` — Pleroma's `reopen`: move the
/// report back to `open`, clearing both action columns with the state that
/// no longer stands on an action, and answer the fresh entity.
///
/// # Errors
///
/// Exactly those of [`resolve_report`].
async fn reopen_report(
    State(state): State<AppState>,
    staff: StaffUser,
    Path(raw): Path<String>,
) -> AppResult<Json<MastodonAdminReport>> {
    staff.require(CLASS_WRITE, AREA_REPORTS)?;
    let filed = report_by_id(&state.db, &raw).await?;
    let row = move_state(&state, &staff.0, &filed, ReportState::Open).await?;
    entity(&state, &staff.0, &row).await.map(Json)
}

/// The shared write of the two state routes: [`report::set_state`] to
/// `to`, the acting staff member named only on the resolution (Pleroma's
/// `update_report_state` takes no actor; the ticket's `resolve` does),
/// beside the `report_update` [`moderation_log`] entry and the
/// `report.updated` [`webhooks`] event every state change of this table
/// leaves — the same follow-ups the console's own bulk `PATCH /reports`
/// runs.
///
/// # Errors
///
/// [`AppError::NotFound`] when the row went between the two reads;
/// [`AppError::Database`] from the write, the target read and the log.
///
/// [`webhooks`]: crate::services::webhooks
async fn move_state(
    state: &AppState,
    staff: &User,
    filed: &ReportRow,
    to: ReportState,
) -> AppResult<ReportRow> {
    let acted_by = (to == ReportState::Resolved).then_some(staff.id);
    let moved = report::set_state(&state.db, filed.id, to, acted_by)
        .await?
        .ok_or(AppError::NotFound)?;
    let target = user::get_by_id(&state.db, moved.target_id).await?;
    let logged = moderation_log::report_update(
        &state.db,
        staff,
        moved.id,
        moved.state.as_str(),
        target.as_ref(),
    )
    .await?;
    debug!(
        log = %logged.id,
        report = %moved.id,
        state = moved.state.as_str(),
        "admin mastodon report state changed"
    );
    webhooks::report_updated(state, &moved).await;
    Ok(moved)
}

/// `POST /api/v1/admin/reports/{id}/assign_to_self` — Pleroma's
/// `assign_to_self`: [`report::assign`] naming the reading staff member as
/// the report's owner, and answer the fresh entity.
///
/// # Errors
///
/// Exactly those of [`resolve_report`].
async fn assign_to_self(
    State(state): State<AppState>,
    staff: StaffUser,
    Path(raw): Path<String>,
) -> AppResult<Json<MastodonAdminReport>> {
    staff.require(CLASS_WRITE, AREA_REPORTS)?;
    let filed = report_by_id(&state.db, &raw).await?;
    let row = move_assignment(&state, &staff.0, &filed, Some(staff.0.id)).await?;
    entity(&state, &staff.0, &row).await.map(Json)
}

/// `POST /api/v1/admin/reports/{id}/unassign` — Pleroma's `unassign`: the
/// assignment write's `None` arm, handing the report back to the
/// unassigned queue, and answer the fresh entity.
///
/// # Errors
///
/// Exactly those of [`resolve_report`].
async fn unassign_report(
    State(state): State<AppState>,
    staff: StaffUser,
    Path(raw): Path<String>,
) -> AppResult<Json<MastodonAdminReport>> {
    staff.require(CLASS_WRITE, AREA_REPORTS)?;
    let filed = report_by_id(&state.db, &raw).await?;
    let row = move_assignment(&state, &staff.0, &filed, None).await?;
    entity(&state, &staff.0, &row).await.map(Json)
}

/// The shared write of the two assignment routes: [`report::assign`], then
/// the entry Pleroma's controller logs beside each — `report_assigned`
/// naming the new owner by nickname, `report_unassigned` the release.
/// Assignment is not a state change: the entity answers in whatever state
/// the report already stood, and nothing else about the row moves.
///
/// # Errors
///
/// [`AppError::NotFound`] when the row went between the two reads;
/// [`AppError::Database`] from the write, the target read and the log.
async fn move_assignment(
    state: &AppState,
    staff: &User,
    filed: &ReportRow,
    assignee: Option<Uuid>,
) -> AppResult<ReportRow> {
    let moved = report::assign(&state.db, filed.id, assignee)
        .await?
        .ok_or(AppError::NotFound)?;
    let target = user::get_by_id(&state.db, moved.target_id).await?;
    let logged = if assignee.is_some() {
        moderation_log::report_assigned(
            &state.db,
            staff,
            moved.id,
            &staff.nickname,
            target.as_ref(),
        )
        .await?
    } else {
        moderation_log::report_unassigned(&state.db, staff, moved.id, target.as_ref()).await?
    };
    debug!(
        log = %logged.id,
        report = %moved.id,
        assigned = moved.assigned_to.is_some(),
        "admin mastodon report assignment changed"
    );
    Ok(moved)
}

/// The `{id}` segment of an admin report route as the stored row: text
/// that is no id and an id that names no report are the one 404 the show
/// route and all four writes answer with.
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives
/// through `?` as [`AppError::Database`].
async fn report_by_id(pool: &PgPool, text: &str) -> AppResult<ReportRow> {
    let Some(id) = parse_id(text) else {
        return Err(AppError::NotFound);
    };
    report::get(pool, id).await?.ok_or(AppError::NotFound)
}

/// Builds the model's listing params from the request's query pairs: the
/// `resolved` state filter read leniently (an unusable value is no value,
/// and no value is the open queue), the two account filters strictly — a
/// value that is no id is the 400 [`INVALID_ID`] every other id filter
/// here gives, an id that names no account is simply a filter that
/// matches no row (Pleroma's `restrict_actor` fall-through).
///
/// # Errors
///
/// [`AppError::BadRequest`] as described.
fn list_params(pairs: &QueryPairs) -> AppResult<report::MastodonListParams> {
    Ok(report::MastodonListParams {
        resolved: resolved_filter(pairs.value("resolved").as_deref()),
        reporter_id: id_filter(pairs, "account_id")?,
        target_id: id_filter(pairs, "target_account_id")?,
    })
}

/// The `resolved` filter: `true` asks for the acted-on pair and `false`
/// for the open queue, each also spelled the way form-encoded clients
/// send booleans (`1` / `0`); anything else — absence, emptiness, junk —
/// is no statement, and the listing keeps its open default (Pleroma's
/// `restrict_state` reads only the literal `true` as an action request
/// and falls everything else through to `open`).
fn resolved_filter(text: Option<&str>) -> Option<bool> {
    match text {
        Some("true" | "1") => Some(true),
        Some("false" | "0") => Some(false),
        _ => None,
    }
}

/// One id filter: absent or empty names nothing to filter on, a value
/// that is no id is the 400 [`INVALID_ID`].
fn id_filter(pairs: &QueryPairs, key: &str) -> AppResult<Option<Uuid>> {
    pairs
        .value(key)
        .map(|raw| parse_id(&raw).ok_or_else(|| AppError::BadRequest(INVALID_ID.to_owned())))
        .transpose()
}

/// The one entity for `row`, through the batched [`entities`] read.
///
/// # Errors
///
/// As [`entities`].
async fn entity(
    state: &AppState,
    viewer: &User,
    row: &ReportRow,
) -> AppResult<MastodonAdminReport> {
    entities(state, viewer, std::slice::from_ref(row))
        .await?
        .pop()
        .ok_or_else(|| AppError::Internal("one report rendered no entity".to_owned()))
}

/// The [`MastodonAdminReport`] entities for `rows`, in order: the named
/// accounts in one read, the cited rules in one read, the citations in one
/// read, and every report's statuses through the shared batch renderer —
/// with the reading staff member as viewer and the same no-filter admin
/// rendering [`crate::api::admin::reports`] gives its entities.
///
/// # Errors
///
/// [`AppError::Database`] from any read; [`AppError::Internal`] for a
/// report whose reporter or target row is gone — the foreign keys cascade
/// a report away with either account, so this names a schema state that
/// cannot occur.
async fn entities(
    state: &AppState,
    viewer: &User,
    rows: &[ReportRow],
) -> AppResult<Vec<MastodonAdminReport>> {
    let users = named_users(state, rows).await?;
    let rules = cited_rules(state, rows).await?;
    let mut statuses = cited_statuses(state, viewer, rows).await?;
    let mut entities = Vec::with_capacity(rows.len());
    for row in rows {
        entities.push(MastodonAdminReport::from_parts(
            MastodonAdminReportParts {
                row,
                reporter: named(&users, row.reporter_id)?,
                target: named(&users, row.target_id)?,
                assigned: row.assigned_to.and_then(|id| users.get(&id)),
                acted_by: row.action_taken_by.and_then(|id| users.get(&id)),
                statuses: statuses.remove(&row.id).unwrap_or_default(),
                rules: row
                    .rule_ids
                    .iter()
                    .filter_map(|id| rules.get(id))
                    .cloned()
                    .collect(),
            },
            &state.config,
        ));
    }
    Ok(entities)
}

/// Every account the reports name — reporter, target, assignee and acting
/// moderator, deduplicated — keyed by id in one read.
///
/// # Errors
///
/// [`AppError::Database`] from the read.
async fn named_users(state: &AppState, rows: &[ReportRow]) -> AppResult<HashMap<Uuid, User>> {
    let mut ids: Vec<Uuid> = Vec::with_capacity(rows.len() * 4);
    for row in rows {
        ids.push(row.reporter_id);
        ids.push(row.target_id);
        ids.extend(row.assigned_to);
        ids.extend(row.action_taken_by);
    }
    let users = user::get_many(&state.db, &ids).await?;
    Ok(users.into_iter().map(|user| (user.id, user)).collect())
}

/// Every rule cited by these reports, keyed by id in one read (an empty
/// citation set reads nothing).
///
/// # Errors
///
/// [`AppError::Database`] from the read.
async fn cited_rules(
    state: &AppState,
    rows: &[ReportRow],
) -> AppResult<HashMap<Uuid, MastodonAdminReportRule>> {
    let mut ids: Vec<Uuid> = rows.iter().flat_map(|row| row.rule_ids.clone()).collect();
    ids.sort_unstable();
    ids.dedup();
    let rules = instance_rule::get_many(&state.db, &ids).await?;
    Ok(rules
        .iter()
        .map(|rule| (rule.id, MastodonAdminReportRule::from_rule(rule)))
        .collect())
}

/// The rendered `statuses` arrays, keyed per report: the citations in one
/// batched read, the rows behind them in one [`status::get_many`], and
/// each report's own page through [`render_statuses`] — the shape the
/// Pleroma admin surface's renderer established.
///
/// # Errors
///
/// [`AppError::Database`] from any read.
async fn cited_statuses(
    state: &AppState,
    viewer: &User,
    rows: &[ReportRow],
) -> AppResult<HashMap<Uuid, Vec<Status>>> {
    let report_ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let cited = report::statuses_for_reports(&state.db, &report_ids).await?;
    let mut ids_per_report: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    let mut status_ids: Vec<Uuid> = Vec::new();
    for (report_id, status_id) in cited {
        ids_per_report.entry(report_id).or_default().push(status_id);
        status_ids.push(status_id);
    }
    let status_rows = status::get_many(&state.db, &status_ids).await?;
    let by_id: HashMap<Uuid, &status::Status> =
        status_rows.iter().map(|row| (row.id, row)).collect();
    let mut rendered: HashMap<Uuid, Vec<Status>> = HashMap::new();
    for (report_id, ids) in ids_per_report {
        let owned: Vec<status::Status> = ids
            .iter()
            .filter_map(|id| by_id.get(id).copied())
            .cloned()
            .collect();
        let statuses = render_statuses(state, ViewerMode::user(viewer), &owned).await?;
        let _kept: Option<Vec<Status>> = rendered.insert(report_id, statuses);
    }
    Ok(rendered)
}

/// The `Link` header value for one fetched page: [`link_header`] over the
/// first (newest) and last (oldest) row's id — the report ids the ticket
/// keys this listing's cursors on — with the request's own filters
/// repeated, and `None` for an empty page.
fn page_link(state: &AppState, uri: &Uri, rows: &[ReportRow]) -> Option<String> {
    let newest = rows.first()?.id.to_string();
    let oldest = rows.last()?.id.to_string();
    link_header(
        &state.config.base_url,
        uri.path(),
        &link_query(uri.query()),
        &newest,
        &oldest,
    )
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

    /// The `resolved` filter in its three readings: the `true` spelling
    /// asks for the acted-on pair, `false` states the open queue outright,
    /// and every other text — absence, emptiness, junk — is no statement
    /// at all, which the listing also reads as open.
    #[test]
    fn the_resolved_filter_reads_booleans_and_ignores_junk() {
        for stated in ["true", "1"] {
            assert_eq!(
                resolved_filter(Some(stated)),
                Some(true),
                "`{stated}` is the acted-on pair"
            );
        }
        for stated in ["false", "0"] {
            assert_eq!(
                resolved_filter(Some(stated)),
                Some(false),
                "`{stated}` states the open queue"
            );
        }
        for unstated in [None, Some(""), Some("yes"), Some("TRUE"), Some("closed")] {
            assert_eq!(
                resolved_filter(unstated),
                None,
                "`{unstated:?}` is no statement"
            );
        }
    }

    /// The filters as the query pairs hand them over: the three ticket
    /// parameters read, an empty value filters nothing, and an id-shaped
    /// pair of accounts splits reporter from target by key, not by value.
    #[test]
    fn the_listing_filters_read_their_own_parameters() {
        let id = "01934567-89ab-7cde-f012-3456789abcde";
        let query = format!("resolved=true&account_id={id}&target_account_id={id}");
        let stated =
            list_params(&QueryPairs::parse(Some(&query))).expect("the ticket's query parses");
        assert_eq!(stated.resolved, Some(true), "the state pair");
        assert_eq!(stated.reporter_id, parse_id(id), "the reporter filter");
        assert_eq!(stated.target_id, parse_id(id), "and the target filter");
        let empty = list_params(&QueryPairs::parse(Some(
            "resolved=&account_id=&target_account_id=",
        )))
        .expect("empty parameters are no filters");
        assert_eq!(empty.resolved, None, "an empty resolved is no statement");
        assert!(
            empty.reporter_id.is_none() && empty.target_id.is_none(),
            "and empty ids filter nothing"
        );
    }

    /// A filter value that is no id is the 400 [`INVALID_ID`] — the same
    /// refusal the moderation log's `user_id` filter and the Pleroma
    /// admin listing give, on either of the two account keys.
    #[test]
    fn an_unusable_filter_id_refuses_the_request() {
        for key in ["account_id", "target_account_id"] {
            let err = list_params(&QueryPairs::parse(Some(&format!("{key}=not-a-uuid"))))
                .expect_err("a broken id cannot filter");
            assert_eq!(err.to_string(), INVALID_ID, "the refusal for {key}");
        }
    }

    /// The page window is Mastodon's admin pair rather than Pleroma's
    /// global one: a hundred rows when the request states no size, two
    /// hundred at most when it asks for more, and an unusable `limit`
    /// back to the default — the same readings the account listings pin.
    #[test]
    fn the_page_window_defaults_to_a_hundred_and_caps_at_two() {
        let absent = PageParams::from_query_with_default(None, DEFAULT_LIMIT, MAX_LIMIT)
            .expect("an absent query is a first page");
        assert_eq!(absent.limit, DEFAULT_LIMIT, "the default page");
        for (query, want) in [("limit=7", 7), ("limit=150", 150), ("limit=999", MAX_LIMIT)] {
            let params = PageParams::from_query_with_default(Some(query), DEFAULT_LIMIT, MAX_LIMIT)
                .expect("a limit is parsable");
            assert_eq!(params.limit, want, "`{query}` sizes the page");
        }
        let junk =
            PageParams::from_query_with_default(Some("limit=nonsense"), DEFAULT_LIMIT, MAX_LIMIT)
                .expect("an unusable limit is no limit");
        assert_eq!(junk.limit, DEFAULT_LIMIT, "and falls back to the default");
    }

    /// The cursor half of the request still refuses a malformed id: an
    /// ignored *filter* may be lenient, an unreadable *cursor* is a bad
    /// request.
    #[test]
    fn a_malformed_cursor_is_still_a_bad_request() {
        let err =
            PageParams::from_query_with_default(Some("max_id=nope"), DEFAULT_LIMIT, MAX_LIMIT)
                .expect_err("a broken cursor cannot page");
        assert_eq!(err.to_string(), INVALID_ID, "the shared wording");
    }
}
