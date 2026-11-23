//! Reports over HTTP: filing a report against an account.
//!
//! Pleroma's `ReportController.create`
//! (`lib/pleroma/web/mastodon_api/controllers/report_controller.ex`, routed
//! `post "/reports"` in `lib/pleroma/web/router.ex`) over
//! `CommonAPI.report` (`lib/pleroma/web/common_api.ex`) is the reference; the
//! storage is [`crate::models::report`] and the fact is this server's
//! `reports` table.
//!
//! The route requires a user token ([`AuthUser`]) granting `write:reports`
//! (Pleroma's own grant for the action, with the `follow` root listed on its
//! `OpenApiSpex` operation but not enforced by its plug; this ticket's grant
//! is the narrow one) and runs on the `statuses_actions` throttle bucket,
//! the bucket Pleroma's `config :pleroma, :rate_limit` puts report filing in
//! (ten requests per ten seconds; see [`crate::rate_limit`]).
//!
//! The parameters, through [`ApiBody`] (decision 10, so `status_ids[]=…`
//! array keys work in form bodies): `account_id` (required — the account to
//! report), `status_ids` (optional, the posts to cite for context), `comment`
//! (the reporter's own words), `forward` (whether to ask the target's
//! instance to handle it; `false` when absent), `category` (`spam`, `legal`,
//! `violation` or `other`, the default) and `rule_ids` (the instance rules
//! the reporter is citing).
//!
//! What the endpoint refuses, all before anything is written:
//!
//! - a `comment` over [`max_report_comment`](crate::config::Config::max_report_comment)
//!   characters (default 1000) is 422 with Mastodon's changeset wording;
//! - a `category` outside the four is 422 with the same wording;
//! - an `account_id` that is malformed, unknown or deactivated is the shared
//!   404 ([`account_or_404`]), resolved after those two body checks the way
//!   Pleroma's `CastAndValidate` plug rejects a bad body before its action
//!   ever looks anything up;
//! - a request about the reporter's own account is 422 `"Can not report
//!   yourself"`, which [`report::create`] states;
//! - a `status_ids` entry that is not an id, names no live status, is not the
//!   target's post, or is not visible to the reporter (the same
//!   [`status_visibility`] the read endpoints apply) is 422
//!   `"Invalid status_ids"` — Pleroma's `check_statuses_visibility` and
//!   `get_report_statuses` reduced to the one answer the ticket pins;
//! - a `rule_ids` entry that is not an id or names no rule of
//!   [`instance_rules`](crate::models::instance_rule) is 422
//!   `"Invalid rule_ids"` (Pleroma drops unknown rules silently, Mastodon
//!   refuses; the ticket pins the refusal).
//!
//! A report that passes is written by [`report::create`] and answered with
//! the Mastodon `Report` entity ([`crate::entities::report`]) whose
//! `status_ids` are read back off `report_statuses` and whose
//! `target_account` is the reported [`User`] as the reporter sees it. Filing
//! is not deduplicated: the same account may report the same account again,
//! and each filing is its own row with its own id.
//!
//! `POST /api/v1/accounts/{id}/flag` is the same filing keyed by the path:
//! the target is the account the route names and the body carries everything
//! else the `ReportParams` of `POST /api/v1/reports` does minus
//! `account_id`. Its one deliberate
//! difference is the default: `forward` is `true` unless the body says
//! otherwise, because the point of flagging an account by id is that the
//! target's own instance hears about it
//! ([`crate::services::federation_out::after_report_created`]
//! then speaks the `Flag` as this instance's own actor, delivered to the
//! target's inbox alone); `forward: false` keeps the flag with our own
//! moderators exactly like an unforwarded `POST /api/v1/reports`.
//!
//! A fresh report is `open`, has nothing done about it, and names no
//! moderator; moving it through moderation is the admin tickets' business,
//! which is what [`report::set_state`] and [`report::assign`] are there for.
//!
//! Filing is also the moment the later tickets act on: the route calls this
//! module's `after_report_created` hook as its landing point, and that hook
//! runs the three things a fresh report owes — the forwarding `Flag`
//! ([`crate::services::federation_out::after_report_created`], for a REMOTE
//! target the reporter asked to involve), the signed `report.created` webhook
//! event ([`crate::services::webhooks::report_created`], queued as a
//! [`webhooks.deliver`](crate::services::webhooks::KIND_DELIVER) job) and the
//! [`report_to_admin`](crate::mailer::templates::report_to_admin) letter to
//! every mailable staff account.
//!
//! The reporter's read side — `GET /api/v0/pleroma/reports` and its `{id}`
//! twin, the user's own filed reports under `read:reports` — is the
//! `own` submodule, mounted at `/api/v0/pleroma` by [`crate::api`] as
//! [`own_router`].
//!
//! [`report::create`]: crate::models::report::create
//! [`report::set_state`]: crate::models::report::set_state
//! [`report::assign`]: crate::models::report::assign
//! [`status_visibility`]: crate::models::status_visibility

mod follow_up;
mod own;
mod params;

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};
use tracing::debug;

use self::params::{
    FlagParams, IdValues, ReportParams, cited_rule_ids, cited_status_ids, validated_category,
    validated_comment,
};
use crate::{
    api::helpers::account_or_404,
    auth::AuthUser,
    entities::report::Report,
    error::AppResult,
    extract::ApiBody,
    models::{report, report::Report as ReportRow, user::User},
    oauth::scopes::WRITE_REPORTS,
    state::AppState,
};

pub use self::{follow_up::after_report_created, own::own_router};

/// The `/api/v1` reports router, merged into [`crate::api::v1`].
///
/// `state` is what lets the one route mount the `statuses_actions` rate
/// limiter (see [`crate::rate_limit`]).
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/reports",
            post(create_report).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route(
            "/accounts/{id}/flag",
            post(flag_account).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
}

/// `POST /api/v1/reports` — file the report the body describes and answer the
/// [`Report`] entity.
///
/// The order is Pleroma's: the body's own shape is checked first (the
/// `comment` cap and the `category` set need no database), then the reported
/// account is resolved, then the two id arrays are checked against what
/// exists, and only then is anything written. A request that passes files its
/// report and answers the entity read back off what was stored — including
/// the cited statuses, whose order and deduplication the write decides.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:reports` check; [`AppError::Unprocessable`] for a `comment`
/// over the configured cap, an unknown `category`, a self-report, or a
/// `status_ids` / `rule_ids` entry that fails its check; [`AppError::NotFound`]
/// for a malformed, unknown or deactivated `account_id`;
/// [`AppError::RateLimited`] from the bucket. Any database failure arrives
/// through `?` as [`AppError::Database`].
///
/// [`AppError::Unauthorized`]: crate::error::AppError::Unauthorized
/// [`AppError::Forbidden`]: crate::error::AppError::Forbidden
/// [`AppError::Unprocessable`]: crate::error::AppError::Unprocessable
/// [`AppError::NotFound`]: crate::error::AppError::NotFound
/// [`AppError::RateLimited`]: crate::error::AppError::RateLimited
/// [`AppError::Database`]: crate::error::AppError::Database
async fn create_report(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<ReportParams>,
) -> AppResult<Json<Report>> {
    auth.require(WRITE_REPORTS)?;
    file(
        &state,
        &auth.0,
        &params.account_id,
        Filing {
            comment: params.comment.as_deref(),
            category: params.category.as_deref(),
            forward: params.forward.unwrap_or_default(),
            status_ids: params.status_ids,
            rule_ids: params.rule_ids,
        },
    )
    .await
}

/// `POST /api/v1/accounts/{id}/flag` — file a report against the account the
/// path names, on the body of [`crate::api::reports::ReportParams`] minus
/// `account_id`.
///
/// One deliberate difference from the collection-shaped route: `forward`
/// defaults to `true` here, because the point of flagging an account by id
/// is that the target's instance is told (see the [module docs]). Everything
/// else — the scope, the checks and their order, the write, the post-write
/// hook and the entity answer — is [`file`], shared with
/// [`create_report`] line for line.
///
/// # Errors
///
/// Exactly [`create_report`]'s, with the path id standing in for
/// `account_id`: a malformed or unknown one is the shared 404.
async fn flag_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<FlagParams>,
) -> AppResult<Json<Report>> {
    auth.require(WRITE_REPORTS)?;
    file(
        &state,
        &auth.0,
        &id,
        Filing {
            comment: params.comment.as_deref(),
            category: params.category.as_deref(),
            forward: params.forward.unwrap_or(true),
            status_ids: params.status_ids,
            rule_ids: params.rule_ids,
        },
    )
    .await
}

/// The validated body of one filing, in the raw shape both report routes
/// hand to [`file`]: what the client sent, before any of it is checked.
#[derive(Debug)]
struct Filing<'a> {
    /// The reporter's own words, `None` for the empty comment.
    comment: Option<&'a str>,
    /// The claimed reason, `None` for the `other` default.
    category: Option<&'a str>,
    /// Whether the target's instance is asked to handle it; each route
    /// applies its own default before handing this in.
    forward: bool,
    /// The statuses cited for context, as sent.
    status_ids: Option<IdValues>,
    /// The instance rules cited, as sent.
    rule_ids: Option<IdValues>,
}

/// The whole life of one filing, shared by both report routes: body checks
/// first, then the target, then the two id arrays, then the write, the
/// post-write hook and the entity answer. Split from the handlers so the
/// path-keyed flag cannot drift from the collection-keyed report.
///
/// # Errors
///
/// Every refusal [`create_report`] documents, in the same order.
async fn file(
    state: &AppState,
    reporter: &User,
    target_id: &str,
    filing: Filing<'_>,
) -> AppResult<Json<Report>> {
    let comment = validated_comment(state.config.max_report_comment, filing.comment)?;
    let category = validated_category(filing.category)?;
    let target = account_or_404(state, target_id).await?;
    let status_ids = cited_status_ids(state, reporter, &target, filing.status_ids).await?;
    let rule_ids = cited_rule_ids(state, filing.rule_ids).await?;
    let row = report::create(
        &state.db,
        &state.config.base_url,
        reporter,
        &target,
        &report::NewReport {
            comment,
            category,
            forwarded: filing.forward,
            rule_ids,
            status_ids,
        },
    )
    .await?;
    debug!(
        report = %row.id,
        user = %reporter.id,
        target = %target.id,
        "report filed"
    );
    after_report_created(state, &row).await;
    filed_response(state, reporter, &target, &row).await
}

/// The entity response for a filed report: its cited statuses read back from
/// `report_statuses` (which is where the write's deduplication and the
/// statuses' own order show up) and the reported account rendered as the
/// reporter sees it.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError::Database
async fn filed_response(
    state: &AppState,
    reporter: &User,
    target: &User,
    row: &ReportRow,
) -> AppResult<Json<Report>> {
    let cited = report::statuses_for(&state.db, row.id).await?;
    Ok(Json(Report::from_row(
        row,
        target,
        &cited,
        &state.config,
        Some(reporter),
    )))
}
