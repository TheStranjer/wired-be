//! The admin API over HTTP: the staff-only endpoints under
//! `/api/v1/pleroma/admin` (`docs/ARCHITECTURE.md` decision 1), starting
//! with the moderation-log foundation (WBE-194).
//!
//! The reference is Pleroma's `AdminAPIController`
//! (`lib/pleroma/web/admin_api/controllers/admin_api_controller.ex`)
//! routed in `lib/pleroma/web/router.ex` under `/api/v1/pleroma/admin`.
//! Pleroma gates that scope with the `admin_api` pipeline (a user token
//! whose account is staff) plus `EnsurePrivilegedPlug` on a per-endpoint
//! role; the defaults of `config/config.exs` hand both
//! `:moderation_log_read` and `:statistics_read` to admins only, which is
//! why both routes here take the [`AdminUser`] extractor. The scope plugs
//! of the same pipelines (`admin:read:reports` and friends) are the
//! [`AdminUser::require`] call each handler makes first.
//!
//! - `GET /api/v1/pleroma/admin/moderation_log` (`list_log`) — the
//!   [`crate::models::moderation_log`] entries, newest first, in Pleroma's
//!   page window (`page` from 1, default 50 `page_size`, capped at 200)
//!   through its filters (`user_id`, `search`, `start_date`, `end_date`),
//!   answered as `{items, total}` (see
//!   [`entities::admin::ModerationLogPage`](crate::entities::admin)).
//! - `GET /api/v1/pleroma/admin/stats` (`stats`) — the
//!   `status_visibility` counters over live, non-reblog statuses: local
//!   accounts when no `instance` is asked for, one domain's mirrored
//!   accounts when it is
//!   ([`status_visibility_counts`](crate::models::status::status_visibility_counts)).
//!
//! The account reads the [`users`] submodule serves — `GET /users` and
//! `GET /users/{nickname}` — are listed there, along with the invite
//! actions of [`invites`] and the webhook routes of [`webhooks`]. The
//! media-proxy cache ledger, the frontend installer and the admin's backup
//! trigger (WBE-208) are the [`ops`] submodule.
//!
//! Later admin tickets join this same file with more `.merge(...)` /
//! `.route(...)` lines; the [module docs of `crate::api::v1`](crate::api::v1)
//! hold the no-fallback rule that applies to every router merged there.
//! The runtime config store (`GET`/`POST /config`, its
//! `GET /config/descriptions` payload and the `GET /need_reboot` /
//! `GET /restart` pair, WBE-206 and WBE-207) joins as the
//! [`config`] submodule, the registration invites (`users/invite_token`,
//! `users/invites`, `users/revoke_invite`, `users/email_invite`, WBE-199)
//! as the [`invites`] submodule — whose [`registration_router`]
//! additionally rides on [`crate::app`] at the root — the webhook
//! maintenance API (`/webhooks` and its `{id}` actions, WBE-209) as the
//! [`webhooks`] submodule, the admin's view of the accounts table
//! (`GET /users` and `GET /users/{nickname}`, WBE-195) as the [`users`]
//! submodule, the OAuth-app maintenance and instance-document
//! endpoints (`/oauth_app` and its `{id}` actions, the
//! `/instance_document/{name}` trio, and — riding on [`crate::app`] at
//! the root like the registration link — the public
//! `GET /instance/{name}.html`, WBE-205) as the [`apps_docs`] submodule,
//! and the federation maintenance routes (the `/relay` ledger trio,
//! `DELETE /instances/{instance}` and `POST /reload_emoji`, WBE-203) as
//! the [`federation`] submodule.
//!
//! [`AdminUser`]: crate::auth::admin::AdminUser
//! [`AdminUser::require`]: crate::auth::admin::AdminUser::require
//! [`registration_router`]: invites::registration_router

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use chrono::{DateTime, NaiveDate, Utc};

use crate::{
    auth::admin::AdminUser,
    config::Config,
    entities::admin::{AdminStats, ModerationLogPage},
    error::{AppError, AppResult},
    models::{moderation_log, status},
    state::AppState,
    util::id::parse_id,
};

pub mod apps_docs;
pub mod config;
pub mod federation;
pub mod invites;
pub mod ops;
pub mod users;
pub mod webhooks;

/// Default `page` when the parameter is absent or not a positive number
/// (Pleroma's `page_params` first, over its `fetch_integer_param`).
const DEFAULT_PAGE: i64 = 1;
/// Default `page_size` when absent or unusable. Pleroma's admin lists
/// page by `@users_page_size` (50); the moderation log follows that
/// default, as the ticket pins.
const DEFAULT_PAGE_SIZE: i64 = 50;
/// Cap for `page_size` (the ticket's bound; Pleroma's admin lists have no
/// `restrict`, so the ceiling is stated here rather than enforced there).
const MAX_PAGE_SIZE: i64 = 200;
/// The area segment of the admin scopes the log listing requires
/// (`admin:read:moderation_log`, or the broad `admin:read`).
const AREA_LOG: &str = "moderation_log";
/// The area segment of the admin scopes the stats endpoint requires.
const AREA_STATS: &str = "statistics";
/// The read endpoint class of [`AdminUser::require`].
const CLASS_READ: &str = "read";

/// The `/api/v1/pleroma/admin` router, nested into [`crate::api::v1`].
/// Later admin tickets extend it with more `.merge(...)` lines.
///
/// `config` reaches [`ops::router`] and [`apps_docs::router`], the merged
/// routers whose routes carry a body limit of their own (the frontend
/// install's uploaded zip and the instance-document upload; see
/// [`crate::extract::multipart::upload_body_limit`]).
pub fn router(config: &Config) -> Router<AppState> {
    Router::new()
        .route("/moderation_log", get(list_moderation_log))
        .route("/stats", get(stats))
        .merge(apps_docs::router(config))
        .merge(config::router())
        .merge(federation::router())
        .merge(invites::router())
        .merge(ops::router(config))
        .merge(users::router())
        .merge(webhooks::router())
}

/// `GET /api/v1/pleroma/admin/moderation_log` — the log page Pleroma's
/// `list_log` answers: `{items, total}`, newest first.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::BadRequest`] when `user_id` is
/// no id; [`AppError::Unprocessable`] when `start_date`/`end_date` is no
/// ISO date or timestamp; [`AppError::Database`] from the listing read.
async fn list_moderation_log(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(params): Query<HashMap<String, String>>,
) -> AppResult<Json<ModerationLogPage>> {
    admin.require(CLASS_READ, AREA_LOG)?;
    let params = log_list_params(&params)?;
    let page = moderation_log::list(&state.db, &params).await?;
    Ok(Json(ModerationLogPage::from_page(&page)))
}

/// `GET /api/v1/pleroma/admin/stats` — the `status_visibility` counters,
/// scoped by the optional `instance` parameter (Pleroma's `stats` +
/// `Stats.get_status_visibility_count`).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::Database`] from the count.
async fn stats(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(params): Query<HashMap<String, String>>,
) -> AppResult<Json<AdminStats>> {
    admin.require(CLASS_READ, AREA_STATS)?;
    let instance = params.get("instance").map(String::as_str);
    let counts = status::status_visibility_counts(&state.db, instance).await?;
    Ok(Json(AdminStats::from_counts(&counts)))
}

/// Builds the model's listing params from the raw query map: `page` and
/// `page_size` read leniently (Pleroma's `fetch_integer_param` keeps the
/// default when a value is no integer; a zero or negative one defaults
/// too, and a `page_size` past [`MAX_PAGE_SIZE`] is capped), `user_id`
/// strictly (400 `Invalid id` — the same refusal [`crate::pagination`]
/// gives a malformed cursor), and the two date bounds as ISO timestamps
/// or plain dates (422 naming the offending parameter).
fn log_list_params(params: &HashMap<String, String>) -> AppResult<moderation_log::LogListParams> {
    let user_id = text_of(params, "user_id")
        .map(|raw| parse_id(&raw).ok_or_else(|| AppError::BadRequest("Invalid id".to_owned())))
        .transpose()?;
    let start_date = bound_of(params, "start_date", false)?;
    let end_date = bound_of(params, "end_date", true)?;
    Ok(moderation_log::LogListParams {
        page: page_number(params, "page", DEFAULT_PAGE),
        page_size: page_size(params),
        user_id,
        search: text_of(params, "search"),
        start_date,
        end_date,
    })
}

/// The non-empty query value of `key`, `None` when absent or empty (an
/// empty Pleroma filter parameter matches nothing to filter on:
/// `maybe_filter_by_search` treats `""` as absent).
fn text_of(params: &HashMap<String, String>, key: &str) -> Option<String> {
    params
        .get(key)
        .map(String::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// A defaulted integer page parameter, Pleroma's `fetch_integer_param`:
/// anything unparsable or non-positive keeps the default.
fn page_number(params: &HashMap<String, String>, key: &str, default: i64) -> i64 {
    page_number_of(text_of(params, key).as_deref(), default)
}

/// [`page_number`] for a request whose parameters were read off the raw
/// query string rather than a map: the same rule, applied to one value's
/// text. The admin submodules share it, so a `page` defaults identically
/// everywhere.
fn page_number_of(text: Option<&str>, default: i64) -> i64 {
    text.and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value >= 1)
        .unwrap_or(default)
}

/// `page_size` with its default and its cap.
fn page_size(params: &HashMap<String, String>) -> i64 {
    page_size_of(text_of(params, "page_size").as_deref())
}

/// [`page_size`] for one value's text, shared the way
/// [`page_number_of`] is.
fn page_size_of(text: Option<&str>) -> i64 {
    page_number_of(text, DEFAULT_PAGE_SIZE).min(MAX_PAGE_SIZE)
}

/// One date bound, `end_of_day` deciding whether a plain `YYYY-MM-DD`
/// stands for the start or the end of that day; a value that is neither
/// an ISO timestamp nor an ISO date is a 422 naming it.
fn bound_of(
    params: &HashMap<String, String>,
    key: &str,
    end_of_day: bool,
) -> AppResult<Option<DateTime<Utc>>> {
    let Some(text) = text_of(params, key) else {
        return Ok(None);
    };
    parse_date_bound(&text, end_of_day)
        .map(Some)
        .ok_or_else(|| {
            AppError::Unprocessable(format!(
                "{key} must be an ISO 8601 date or timestamp, not {text}"
            ))
        })
}

/// Reads a date bound: a full ISO 8601 / RFC 3339 timestamp, or a plain
/// `YYYY-MM-DD` day read as its first or last instant (inclusive window,
/// Pleroma's `inserted_at >= start` / `<= end`).
fn parse_date_bound(text: &str, end_of_day: bool) -> Option<DateTime<Utc>> {
    if let Ok(moment) = DateTime::parse_from_rfc3339(text) {
        return Some(moment.with_timezone(&Utc));
    }
    let day = NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
    if end_of_day {
        day.and_hms_micro_opt(23, 59, 59, 999_999)
            .map(|instant| instant.and_utc())
    } else {
        day.and_hms_opt(0, 0, 0).map(|instant| instant.and_utc())
    }
}

#[cfg(test)]
mod tests {
    use axum::response::IntoResponse as _;

    use super::*;

    /// One parsed pair of defaults, for the filter tests.
    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn page_parameters_default_leniently() {
        let parsed = log_list_params(&params(&[])).expect("empty query is parsable");
        assert_eq!(parsed.page, DEFAULT_PAGE, "absent page is the first page");
        assert_eq!(
            parsed.page_size, DEFAULT_PAGE_SIZE,
            "absent page_size is the default"
        );
        assert!(
            parsed.user_id.is_none() && parsed.search.is_none(),
            "no filters"
        );
        let junk = log_list_params(&params(&[
            ("page", "nope"),
            ("page_size", "-3"),
            ("user_id", ""),
            ("search", ""),
        ]))
        .expect("unusable numbers fall to the defaults");
        assert_eq!(junk.page, DEFAULT_PAGE, "a non-number page defaults");
        assert_eq!(
            junk.page_size, DEFAULT_PAGE_SIZE,
            "a negative page_size defaults"
        );
        assert!(
            junk.user_id.is_none() && junk.search.is_none(),
            "empty parameters filter nothing"
        );
    }

    #[test]
    fn the_page_size_is_capped_at_the_bound() {
        let parsed =
            log_list_params(&params(&[("page", "3"), ("page_size", "5000")])).expect("parses");
        assert_eq!(parsed.page, 3, "the page number passes through");
        assert_eq!(parsed.page_size, MAX_PAGE_SIZE, "and the size is capped");
    }

    #[test]
    fn an_unparsable_user_id_is_the_invalid_id_400() {
        let err = log_list_params(&params(&[("user_id", "not-a-uuid")]))
            .expect_err("a broken id cannot filter");
        assert_eq!(
            err.to_string(),
            "Invalid id",
            "the pagination refusal wording"
        );
        let response = err.into_response();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "and it is a 400"
        );
    }

    #[test]
    fn dates_read_as_timestamps_and_days() {
        let parsed = log_list_params(&params(&[
            ("start_date", "2026-01-31T12:34:56Z"),
            ("end_date", "2026-02-01"),
        ]))
        .expect("both bounds parse");
        assert_eq!(
            parsed.start_date.map(|moment| moment.to_rfc3339()),
            Some("2026-01-31T12:34:56+00:00".to_owned()),
            "a full timestamp stands as given"
        );
        assert_eq!(
            parsed.end_date.map(|moment| moment.to_rfc3339()),
            Some("2026-02-01T23:59:59.999999+00:00".to_owned()),
            "a plain end date is the last instant of its day (inclusive)"
        );
        let start_only = parse_date_bound("2026-02-01", false).expect("parses");
        assert_eq!(
            start_only.to_rfc3339(),
            "2026-02-01T00:00:00+00:00",
            "a plain start date is the day's first instant"
        );
        let offset = parse_date_bound("2026-02-01T00:00:00+02:00", false).expect("parses");
        assert_eq!(
            offset.to_rfc3339(),
            "2026-01-31T22:00:00+00:00",
            "offsets fold to UTC"
        );
    }

    #[test]
    fn a_broken_date_names_its_parameter() {
        let err = log_list_params(&params(&[("start_date", "31/01/2026")]))
            .expect_err("a reversed date is no ISO bound");
        assert_eq!(
            err.to_string(),
            "start_date must be an ISO 8601 date or timestamp, not 31/01/2026",
            "the refusal names the parameter and the value"
        );
    }
}
