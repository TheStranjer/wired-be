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
//! [`report::create`]: crate::models::report::create
//! [`report::set_state`]: crate::models::report::set_state
//! [`report::assign`]: crate::models::report::assign
//! [`status_visibility`]: crate::models::status_visibility

use axum::{Json, Router, extract::State, routing::post};
use serde::Deserialize;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    api::helpers::account_or_404,
    auth::AuthUser,
    entities::report::Report,
    error::{AppError, AppResult},
    extract::ApiBody,
    mailer,
    models::{
        instance_rule, report,
        report::{Category, Report as ReportRow},
        status, status_visibility,
        user::{self, User},
    },
    oauth::scopes::WRITE_REPORTS,
    services::{federation_out, webhooks},
    state::AppState,
    util::id::parse_id,
};

/// 422 body for a cited status that is not an id, not live, not the target's,
/// or not visible to the reporter (the ticket's wording, where Pleroma's
/// `visibility_error` answers a 404 "Record not found").
const INVALID_STATUS_IDS: &str = "Invalid status_ids";
/// 422 body for a cited rule that names no rule of this instance.
const INVALID_RULE_IDS: &str = "Invalid rule_ids";
/// 422 body for a `category` outside the four the column allows (Mastodon's
/// changeset wording, the precedent [`crate::api::filters`] keeps).
const CATEGORY_INVALID: &str = "Validation failed: Category is not included in the list";

/// The `/api/v1` reports router, merged into [`crate::api::v1`].
///
/// `state` is what lets the one route mount the `statuses_actions` rate
/// limiter (see [`crate::rate_limit`]).
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new().route(
        "/reports",
        post(create_report).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
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
async fn create_report(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<ReportParams>,
) -> AppResult<Json<Report>> {
    auth.require(WRITE_REPORTS)?;
    let comment = validated_comment(state.config.max_report_comment, params.comment.as_deref())?;
    let category = validated_category(params.category.as_deref())?;
    let target = account_or_404(&state, &params.account_id).await?;
    let status_ids = cited_status_ids(&state, &auth.0, &target, params.status_ids).await?;
    let rule_ids = cited_rule_ids(&state, params.rule_ids).await?;
    let row = report::create(
        &state.db,
        &state.config.base_url,
        &auth.0,
        &target,
        &report::NewReport {
            comment,
            category,
            forwarded: params.forward.unwrap_or_default(),
            rule_ids,
            status_ids,
        },
    )
    .await?;
    debug!(
        report = %row.id,
        user = %auth.0.id,
        target = %target.id,
        "report filed"
    );
    after_report_created(&state, &row).await;
    filed_response(&state, &auth.0, &target, &row).await
}

/// The entity response for a filed report: its cited statuses read back from
/// `report_statuses` (which is where the write's deduplication and the
/// statuses' own order show up) and the reported account rendered as the
/// reporter sees it.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
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

/// The request parameters of `POST /api/v1/reports`.
///
/// Everything but [`account_id`](Self::account_id) is `Option` at the serde
/// layer so the endpoint can apply the documented default or answer the
/// ticket's wording; the id arrays are [`IdValues`], which takes the JSON
/// array, the form's `status_ids[]=…` keys and a lone value alike.
#[derive(Debug, Deserialize)]
struct ReportParams {
    /// The account to report; required, and a missing one is the extractor's
    /// 422.
    account_id: String,
    /// The statuses to cite for context.
    #[serde(default)]
    status_ids: Option<IdValues>,
    /// The reporter's own words; empty when it says none.
    #[serde(default)]
    comment: Option<String>,
    /// Whether to forward the report to the target's instance; `false` when
    /// absent (Pleroma's `Map.get(data, :forward, false)`).
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    forward: Option<bool>,
    /// Why it is reporting; `other` when absent.
    #[serde(default)]
    category: Option<String>,
    /// The instance rules it is citing.
    #[serde(default)]
    rule_ids: Option<IdValues>,
}

/// One of the id-array parameters as clients send it: a list (a JSON array or
/// repeated `key[]=…` form keys) or a lone value, which Pleroma's
/// `List.wrap` accepts too.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IdValues {
    /// A lone id.
    Single(String),
    /// A list of ids.
    Multiple(Vec<String>),
}

impl IdValues {
    /// The values as a list; a lone string becomes the one-element list.
    fn into_list(self) -> Vec<String> {
        match self {
            Self::Single(text) => vec![text],
            Self::Multiple(values) => values,
        }
    }
}

/// The `comment`, checked: absent is empty, and more than
/// `max_report_comment` characters is 422 with Mastodon's limit wording. A
/// kept comment is stored verbatim.
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
fn validated_comment(max_report_comment: usize, comment: Option<&str>) -> AppResult<String> {
    let text = comment.unwrap_or_default();
    if text.chars().count() > max_report_comment {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: Comment is too long (maximum is {max_report_comment} characters)"
        )));
    }
    Ok(text.to_owned())
}

/// The `category`, checked: absent is [`Category::Other`] (the column's
/// default), anything the enum does not name is 422 [`CATEGORY_INVALID`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
fn validated_category(category: Option<&str>) -> AppResult<Category> {
    category.map_or_else(
        || Ok(Category::Other),
        |text| {
            text.parse()
                .map_err(|_unknown| AppError::Unprocessable(CATEGORY_INVALID.to_owned()))
        },
    )
}

/// The values of one id-array parameter as ids, in the order they were sent
/// and without repeats: an entry that is not id text is 422 `refusal`, which
/// is how the endpoint answers a malformed id the same way it answers one
/// that names nothing.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `refusal` as its message, for an entry
/// that is not id text.
fn id_list(values: Option<IdValues>, refusal: &str) -> AppResult<Vec<Uuid>> {
    let mut ids: Vec<Uuid> = Vec::new();
    for value in values.map_or_else(Vec::new, IdValues::into_list) {
        let Some(id) = parse_id(&value) else {
            return Err(AppError::Unprocessable(refusal.to_owned()));
        };
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// The statuses a report may cite, checked: every one of them a live status
/// [`status`] gives `reporter` leave to see and whose author is `target`.
///
/// The authorship check is Pleroma's `get_report_statuses` (the cited posts
/// must belong to the reported account) and the visibility check its
/// `check_statuses_visibility`; the two are one answer here, because telling
/// a reporter that somebody's private post exists is not something either
/// endpoint does. An empty or absent array is the empty list and nothing to
/// check.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_STATUS_IDS`] for an entry that
/// is not id text or fails either check; any database failure arrives through
/// `?` as [`AppError::Database`].
async fn cited_status_ids(
    state: &AppState,
    reporter: &User,
    target: &User,
    values: Option<IdValues>,
) -> AppResult<Vec<Uuid>> {
    let requested = id_list(values, INVALID_STATUS_IDS)?;
    if requested.is_empty() {
        return Ok(requested);
    }
    let authored: Vec<status::Status> = status::get_many(&state.db, &requested)
        .await?
        .into_iter()
        .filter(|row| row.user_id == target.id)
        .collect();
    let visible = status_visibility::filter_visible(&state.db, authored, Some(reporter)).await?;
    if visible.len() != requested.len() {
        return Err(AppError::Unprocessable(INVALID_STATUS_IDS.to_owned()));
    }
    Ok(requested)
}

/// The rules a report may cite, checked: every one of them an id this
/// instance's [`instance_rule::list`] holds. An empty or absent array is the
/// empty list and nothing to check.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_RULE_IDS`] for an entry that is
/// not id text or names no rule; any database failure arrives through `?` as
/// [`AppError::Database`].
async fn cited_rule_ids(state: &AppState, values: Option<IdValues>) -> AppResult<Vec<Uuid>> {
    let requested = id_list(values, INVALID_RULE_IDS)?;
    if requested.is_empty() {
        return Ok(requested);
    }
    let rules = instance_rule::list(&state.db).await?;
    let known: Vec<Uuid> = rules.iter().map(|rule| rule.id).collect();
    if requested.iter().all(|id| known.contains(id)) {
        Ok(requested)
    } else {
        Err(AppError::Unprocessable(INVALID_RULE_IDS.to_owned()))
    }
}

/// Post-write hook of `POST /api/v1/reports`: three things happen once a
/// report is stored, in the order Pleroma does them in
/// `ActivityPub.flag/1` — forward, announce, write to the staff.
///
/// - [`federation_out::after_report_created`] forwards the report's `Flag` to
///   a REMOTE target's instance when the reporter asked for it (its delivery
///   queues through the job table, decision 11), and stays silent for a local
///   target or an unforwarded filing.
/// - [`webhooks::report_created`] queues the signed `report.created` event
///   for every enabled webhook subscribed to it (decision 11 again: the
///   delivery is a job, not part of this request).
/// - [`email_staff`] posts the report to the staff mailboxes.
///
/// The inbound federation `Flag` ([`crate::federation::handlers::misc`])
/// runs the same hook on the report it files, so whatever stands here
/// fires for locally filed and forwarded reports alike; the federating
/// half answers silence for the inbound one, whose target is by
/// construction a local account.
pub(crate) async fn after_report_created(state: &AppState, report: &ReportRow) {
    federation_out::after_report_created(state, report).await;
    webhooks::report_created(state, report).await;
    email_staff(state, report).await;
}

/// The two accounts a filed report names, as
/// [`report_to_admin`](mailer::templates::report_to_admin) addresses them.
#[derive(Debug)]
struct Parties {
    /// The nickname of the account that filed the report.
    reporter: String,
    /// The nickname of the account the report is about.
    target: String,
}

/// The staff letters a filed report owes: Pleroma's `AdminEmail.report`
/// ([`report_to_admin`](mailer::templates::report_to_admin) here) to every
/// local admin or moderator that left an address — the recipients of
/// Pleroma's `all_users_with_privilege(:reports_manage_reports)` filtered to
/// the mailable ones, with the reporter left out of its own report's letters
/// as Pleroma leaves it out.
///
/// While `SMTP_HOST` is unset nothing is queued at all
/// ([`mailer::deliver_later`] drops the letter), so the reads are skipped
/// rather than run for a letter that will never be written. A failure at any
/// step is logged and costs at most one recipient's letter: the report itself
/// is filed, and a missing notification has never been a reason to undo one.
async fn email_staff(state: &AppState, row: &ReportRow) {
    if state.config.smtp_host.is_none() {
        debug!(report = %row.id, "mail is off: no staff letter is queued for this report");
        return;
    }
    let staff = match user::admin::staff_with_email(&state.db).await {
        Ok(staff) => staff,
        Err(err) => {
            warn!(%err, report = %row.id, "the staff listing failed, no report letter is queued");
            return;
        }
    };
    if staff.is_empty() {
        debug!(report = %row.id, "the instance has no mailable staff account");
        return;
    }
    let Some(parties) = report_parties(state, row).await else {
        debug!(report = %row.id, "a report account is gone, no staff letter can name them");
        return;
    };
    for staffer in staff {
        if staffer.id == row.reporter_id {
            continue;
        }
        let Some(address) = staffer.email.as_deref() else {
            continue;
        };
        let letter = mailer::templates::report_to_admin(
            &state.config,
            address,
            &parties.reporter,
            &parties.target,
            &row.comment,
        );
        if let Err(err) = mailer::deliver_later(state, letter).await {
            warn!(%err, report = %row.id, "a staff report letter could not be queued");
        }
    }
}

/// The nicknames [`email_staff`] writes into a letter, `None` when either
/// account is already gone — a deletion cascade that beat the hook, and a
/// letter about accounts that no longer exist is not written.
async fn report_parties(state: &AppState, row: &ReportRow) -> Option<Parties> {
    let reporter = nickname_of(state, row.reporter_id).await?;
    let target = nickname_of(state, row.target_id).await?;
    Some(Parties { reporter, target })
}

/// The nickname of the account behind `id`, `None` when no account carries it
/// or the read failed.
async fn nickname_of(state: &AppState, id: Uuid) -> Option<String> {
    match user::get_by_id(&state.db, id).await {
        Ok(Some(account)) => Some(account.nickname),
        Ok(None) => None,
        Err(err) => {
            warn!(%err, account = %id, "the report letter's account could not be read");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::{Config, DEFAULT_MAX_REPORT_COMMENT, TEST_BASE_URL},
        http_client::HttpClient,
        mailer::MemoryTransport,
        models::job::Job,
        rate_limit::RateLimiter,
        streaming::hub::Hub,
        uploads::UploaderBackend,
    };

    /// Parses `value` as the endpoint's parameters and validates the body-only
    /// ones, as the handler would before it looks anything up.
    fn body(value: Value) -> AppResult<(String, Category)> {
        let params: ReportParams = crate::extract::into_params(value)?;
        Ok((
            validated_comment(DEFAULT_MAX_REPORT_COMMENT, params.comment.as_deref())?,
            validated_category(params.category.as_deref())?,
        ))
    }

    /// The minimal valid parameter object.
    fn minimal() -> Value {
        json!({"account_id": "01934567-89ab-7cde-f012-3456789abcde"})
    }

    /// The minimal body with parameter `key` set to `value`.
    fn body_with(key: &str, value: Value) -> Value {
        let mut body = minimal();
        let _replaced: Option<Value> = body
            .as_object_mut()
            .and_then(|map| map.insert(key.to_owned(), value));
        body
    }

    /// The 422 message `value` is rejected with.
    fn rejection(value: Value) -> String {
        body(value)
            .expect_err("the fixture body is invalid")
            .to_string()
    }

    /// The ids `value` yields under the `status_ids` spelling, as the text the
    /// parameter layer reads them, validated with no database behind it.
    fn ids(value: Value, refusal: &str) -> AppResult<Vec<String>> {
        let params: ReportParams = crate::extract::into_params(value)?;
        Ok(id_list(params.status_ids, refusal)?
            .into_iter()
            .map(|id| id.to_string())
            .collect())
    }

    #[test]
    fn a_minimal_body_applies_every_default() {
        let (comment, category) = body(minimal()).expect("the minimal body validates");
        assert_eq!(comment, "", "no comment is the empty comment");
        assert_eq!(category, Category::Other, "and no category is `other`");
    }

    #[test]
    fn the_comment_cap_is_the_configured_number_in_characters() {
        let at_the_cap = body_with("comment", json!("x".repeat(DEFAULT_MAX_REPORT_COMMENT)));
        let (comment, _) = body(at_the_cap).expect("a comment of exactly the cap fits");
        assert_eq!(
            comment.chars().count(),
            DEFAULT_MAX_REPORT_COMMENT,
            "the cap itself is still valid"
        );

        let message = rejection(body_with("comment", json!("x".repeat(1_001))));
        assert_eq!(
            message, "Validation failed: Comment is too long (maximum is 1000 characters)",
            "one over the cap is Mastodon's wording"
        );

        // The cap counts characters, not bytes: a multi-byte comment of the
        // same length is as valid as an ascii one.
        let unicode = body_with("comment", json!("é".repeat(DEFAULT_MAX_REPORT_COMMENT)));
        assert!(body(unicode).is_ok(), "a codepoint is one character");
    }

    #[test]
    fn every_known_category_parses() {
        for category in Category::ALL {
            let (_, parsed) = body(body_with("category", json!(category.as_str())))
                .unwrap_or_else(|err| panic!("`{category}` is a category: {err}"));
            assert_eq!(parsed, category, "the name round-trips");
        }
    }

    #[test]
    fn an_unknown_category_is_refused() {
        for value in [
            json!(""),
            json!("Spam"),
            json!("harassment"),
            json!("oth er"),
        ] {
            assert_eq!(
                rejection(body_with("category", value.clone())),
                CATEGORY_INVALID,
                "`{value}` is not one of the four"
            );
        }
    }

    #[test]
    fn the_forward_flag_takes_the_form_spellings() {
        let params: ReportParams =
            crate::extract::into_params(body_with("forward", json!("true"))).expect("a form bool");
        assert_eq!(params.forward, Some(true), "the string `true`");

        let absent: ReportParams = crate::extract::into_params(minimal()).expect("no forward key");
        assert_eq!(absent.forward, None, "absent is None at the serde layer");
        assert!(
            !absent.forward.unwrap_or_default(),
            "and the handler defaults it to false"
        );
    }

    #[test]
    fn an_array_or_a_lone_value_is_the_same_id_list() {
        let one = "01934567-89ab-7cde-f012-3456789abcde";
        let two = "01934567-89ab-7cde-f012-3456789abcd6";

        let listed =
            ids(body_with("status_ids", json!([one, two])), "bad").expect("a json array is a list");
        assert_eq!(listed, [one, two].map(str::to_owned), "in the order sent");

        let lone = ids(body_with("status_ids", json!(one)), "bad").expect("a lone value");
        assert_eq!(lone, [one].map(str::to_owned), "is the one-element list");

        let repeated = ids(body_with("status_ids", json!([one, two, one])), "bad")
            .expect("a repeat is no error");
        assert_eq!(
            repeated,
            [one, two].map(str::to_owned),
            "and answers once, in order"
        );

        let empty = ids(body_with("status_ids", json!([])), "bad").expect("an empty list");
        assert!(empty.is_empty(), "is the empty list");

        let absent = ids(minimal(), "bad").expect("no key at all");
        assert!(absent.is_empty(), "and an absent parameter is the same");
    }

    #[test]
    fn an_id_that_is_not_id_text_is_the_parameter_s_own_refusal() {
        assert_eq!(
            rejection_min("status_ids", json!(["not-an-id"])),
            "bad",
            "a malformed id is refused with the message the caller names"
        );
        assert_eq!(
            ids(
                body_with(
                    "status_ids",
                    json!(["01934567-89ab-7cde-f012-3456789abcde", "x"])
                ),
                INVALID_STATUS_IDS
            )
            .expect_err("one bad entry sinks the array")
            .to_string(),
            INVALID_STATUS_IDS,
            "and the endpoint's wording is the one the ticket pins"
        );
    }

    /// The refusal text a `status_ids` body of `value` is answered with.
    fn rejection_min(key: &str, value: Value) -> String {
        ids(body_with(key, value), "bad")
            .expect_err("the fixture ids are invalid")
            .to_string()
    }

    /// Test state over `pool` on `config`, with the recording transport: the
    /// shape both helpers below build.
    fn state_with(pool: &PgPool, config: Config) -> AppState {
        let config = Arc::new(config);
        AppState {
            db: pool.clone(),
            hub: Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(MemoryTransport::new()),
        }
    }

    /// The mail-enabled state: [`email_staff`] queues a letter only when a
    /// relay is configured, and the suite's default config names none.
    ///
    /// [`email_staff`]: super::email_staff
    fn mail_state(pool: &PgPool) -> AppState {
        let mut config = Config::for_tests();
        config.smtp_host = Some("smtp.example.org".to_owned());
        state_with(pool, config)
    }

    /// Every queued `mailer.send` job, oldest first.
    async fn letters(pool: &PgPool) -> Vec<Job> {
        sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 ORDER BY id")
            .bind(mailer::KIND_SEND)
            .fetch_all(pool)
            .await
            .expect("the job read runs")
    }

    /// A local account named `nickname` with `address` as its email and the
    /// admin role when `admin` says so.
    async fn person(pool: &PgPool, nickname: &str, address: &str, admin: bool) -> User {
        user::insert(
            pool,
            &user::NewUser {
                email: Some(address.to_owned()),
                is_admin: user::Flag::new(admin),
                ..user::NewUser::local(nickname, &user::local_ap_id(TEST_BASE_URL, nickname))
            },
        )
        .await
        .expect("the account fixture inserts")
    }

    /// `reporter`'s filed report about `target`, with a comment the letter
    /// quotes.
    async fn filed_row(pool: &PgPool, reporter: &User, target: &User) -> ReportRow {
        report::create(
            pool,
            TEST_BASE_URL,
            reporter,
            target,
            &report::NewReport {
                comment: "a letter, please".to_owned(),
                category: Category::Other,
                forwarded: false,
                rule_ids: vec![],
                status_ids: vec![],
            },
        )
        .await
        .expect("the report fixture files")
    }

    /// The recipients the queued letters name, oldest first.
    async fn recipients(pool: &PgPool) -> Vec<String> {
        letters(pool)
            .await
            .iter()
            .map(|job| {
                job.args["to"]
                    .as_str()
                    .expect("a queued letter names its recipient")
                    .to_owned()
            })
            .collect()
    }

    #[tokio::test]
    async fn a_filed_report_letters_every_mailable_staff_account() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = mail_state(&pool);
        let boss = person(&pool, "letter-boss", "boss@example.org", true).await;
        let _deputy = person(&pool, "letter-deputy", "deputy@example.org", true).await;
        let reporter = person(&pool, "letter-reporter", "ring@example.org", false).await;
        let target = person(&pool, "letter-target", "target@example.org", false).await;
        let row = filed_row(&pool, &reporter, &target).await;

        email_staff(&state, &row).await;

        assert_eq!(
            recipients(&pool).await,
            vec![
                "boss@example.org".to_owned(),
                "deputy@example.org".to_owned()
            ],
            "one letter per mailable staff account, in creation order"
        );
        let queued = letters(&pool).await;
        let first = queued.first().expect("the admin's letter");
        assert_eq!(
            first.queue,
            crate::jobs::QUEUE_MAILER,
            "on the mailer queue"
        );
        assert_eq!(first.args["subject"], "Wired Report", "the subject");
        assert!(
            first.args["text_body"]
                .as_str()
                .is_some_and(|body| body.contains("@letter-reporter")
                    && body.contains("@letter-target")
                    && body.contains("a letter, please")),
            "the body names both accounts and quotes the comment: {}",
            first.args["text_body"]
        );
        assert!(boss.is_admin.get(), "the fixture's own admin");
    }

    #[tokio::test]
    async fn the_reporter_is_left_out_of_its_own_reports_letters() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = mail_state(&pool);
        let boss = person(&pool, "self-boss", "boss2@example.org", true).await;
        let target = person(&pool, "self-target", "target2@example.org", false).await;
        let row = filed_row(&pool, &boss, &target).await;

        email_staff(&state, &row).await;

        assert!(
            recipients(&pool).await.is_empty(),
            "the instance's only staffer filed the report, so nobody is written to"
        );
    }

    #[tokio::test]
    async fn a_staff_account_without_an_address_gets_no_letter() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = mail_state(&pool);
        let _silent = user::insert(
            &pool,
            &user::NewUser {
                is_moderator: user::Flag::new(true),
                ..user::NewUser::local(
                    "silent-mod",
                    &user::local_ap_id(TEST_BASE_URL, "silent-mod"),
                )
            },
        )
        .await
        .expect("the addressless moderator inserts");
        let reporter = person(&pool, "silent-reporter", "ring2@example.org", false).await;
        let target = person(&pool, "silent-target", "target3@example.org", false).await;
        let row = filed_row(&pool, &reporter, &target).await;

        email_staff(&state, &row).await;

        assert!(
            recipients(&pool).await.is_empty(),
            "a staffer with no mailbox is not written to"
        );
    }

    #[tokio::test]
    async fn a_report_that_names_no_accounts_letters_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = mail_state(&pool);
        let boss = person(&pool, "orphan-boss", "boss3@example.org", true).await;
        let reporter = person(&pool, "orphan-reporter", "ring3@example.org", false).await;
        let target = person(&pool, "orphan-target", "target4@example.org", false).await;
        let row = filed_row(&pool, &reporter, &target).await;

        email_staff(
            &state,
            &ReportRow {
                target_id: Uuid::now_v7(),
                ..row.clone()
            },
        )
        .await;

        assert!(
            recipients(&pool).await.is_empty(),
            "a report whose target the cascade already took names nobody to write about"
        );
        email_staff(&state, &row).await;
        assert_eq!(
            recipients(&pool).await,
            vec!["boss3@example.org".to_owned()],
            "and the same hook on a whole row writes the letter"
        );
        assert!(boss.is_admin.get(), "the fixture's own admin");
    }

    #[tokio::test]
    async fn no_relay_means_no_letter_and_no_reads() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = state_with(&pool, Config::for_tests());
        let boss = person(&pool, "quiet-boss", "boss4@example.org", true).await;
        let reporter = person(&pool, "quiet-reporter", "ring4@example.org", false).await;
        let target = person(&pool, "quiet-target", "target5@example.org", false).await;
        let row = filed_row(&pool, &reporter, &target).await;

        email_staff(&state, &row).await;

        assert!(
            letters(&pool).await.is_empty(),
            "`SMTP_HOST` unset drops the letter before anything is read"
        );
        assert_eq!(
            boss.email.as_deref(),
            Some("boss4@example.org"),
            "the staffer was there all along"
        );
    }

    #[tokio::test]
    async fn a_failing_staff_read_is_logged_and_not_raised() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/wired_no_such_database")
            .expect("a lazy pool needs no server");
        let state = mail_state(&pool);
        let now = chrono::Utc::now();
        let row = ReportRow {
            id: Uuid::now_v7(),
            reporter_id: Uuid::now_v7(),
            target_id: Uuid::now_v7(),
            comment: String::new(),
            category: Category::Other,
            forwarded: false,
            state: report::ReportState::Open,
            assigned_to: None,
            action_taken_at: None,
            rule_ids: vec![],
            ap_id: None,
            inserted_at: now,
            updated_at: now,
        };

        email_staff(&state, &row).await;

        assert!(
            report::get(&pool, row.id).await.is_err(),
            "the pool this test ran on really cannot answer a read"
        );
    }
}
