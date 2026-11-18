//! Admin rules and announcements over HTTP: the instance-content
//! maintenance API under `/api/v1/pleroma/admin` (WBE-204).
//!
//! The references are Pleroma's `AdminAPI.RuleController`
//! (`lib/pleroma/web/admin_api/controllers/rule_controller.ex`) and
//! `AdminAPI.AnnouncementController`
//! (`lib/pleroma/web/admin_api/controllers/announcement_controller.ex`),
//! routed in `lib/pleroma/web/router.ex` under `/api/v1/pleroma/admin`.
//! Pleroma gates the rules scope with `admin:read` / `admin:write` and
//! puts the announcements behind its
//! `announcements_manage_announcements` privileged role, which the default
//! configuration grants admins; the ticket keeps both halves on the admin
//! role and the admin scopes, so every route here runs on [`AdminUser`]
//! followed by its own [`AdminUser::require`] call — the area `rules` for
//! the rule routes, the area `announcements` for the notice routes — and
//! every write leaves a [`moderation_log`] entry behind.
//!
//! [`AdminUser::require`]: crate::auth::admin::AdminUser::require
//! [`moderation_log`]: crate::models::moderation_log
//!
//! The rules:
//!
//! - `GET /rules` (`index`) — every [`instance_rule`] row in display
//!   order, lowest `priority` first and `id` second, as the four-key
//!   [`AdminRule`] entity Pleroma's `AdminAPI.RuleView` renders.
//! - `POST /rules` (`create`) — `text` (required, at most 500 characters),
//!   `hint` and the integer `priority`, answering the created entity. A
//!   body without a `text` is Pleroma's changeset refusal rendered in the
//!   `CastAndValidate` shape the sibling admin modules use: 400
//!   `{"errors":{"text":["can't be blank"]}}` (and the same map for a text
//!   past the bound).
//! - `PATCH /rules/{id}` (`update`) — the present-only write over those
//!   three fields (an absent field keeps its stored value; a present `text`
//!   must still validate), answering the rewritten entity.
//! - `DELETE /rules/{id}` (`delete`) — the empty object.
//!
//! The announcements, on the [`announcement`] model:
//!
//! - `GET /announcements` (`index`) — a `limit` / `offset` window over
//!   every notice, newest first: the administration view sees the ones
//!   whose activity window has not opened and the ones already closed, and
//!   computes no viewer's `read` flag.
//! - `POST /announcements` (`create`) — `content` (required), the RFC 3339
//!   `starts_at` and `ends_at` (with `ends_at` after `starts_at`) and
//!   `all_day`, answering the created entity. What the author wrote is kept
//!   verbatim as the row's `raw_content` and reaches the entity as
//!   `pleroma.raw_content`; what clients render is that text run through
//!   [`text_to_html`], Pleroma's `format_input(content, "text/plain")`
//!   step, so the served `content` is sanitized HTML. The route then
//!   publishes Mastodon's `announcement` stream event for the new entity
//!   ([`announcement_published`]) — the client API's notices otherwise only
//!   appear on a re-read.
//! - `GET /announcements/{id}` (`show`), `PATCH /announcements/{id}`
//!   (`change`, present-only, `null` reading as absent the way every admin
//!   body here reads it) and `DELETE /announcements/{id}` (`delete`, the
//!   empty object).
//!
//! An id that is no `UUIDv7` text, or names no row, is the 404
//! `{"error":"Not found"}` Pleroma's admin `FallbackController` renders,
//! the same refusal the webhook routes give.
//!
//! [`announcement`]: crate::models::announcement
//! [`announcement_published`]: crate::services::streaming::announcement_published
//! [`instance_rule`]: crate::models::instance_rule

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, patch},
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tracing::debug;

use crate::{
    auth::admin::AdminUser,
    entities::{admin::AdminRule, announcement::Announcement},
    error::{AppError, AppResult},
    extract::{ApiBody, flex},
    models::{announcement, instance_rule, moderation_log},
    services::streaming,
    state::AppState,
    util::{format::text_to_html, id::parse_id, time::parse_time},
};

/// The read endpoint class of [`AdminUser::require`].
const CLASS_READ: &str = "read";
/// The write endpoint class of [`AdminUser::require`].
const CLASS_WRITE: &str = "write";
/// The admin scope area the rule routes live in (`admin:read:rules` /
/// `admin:write:rules`).
const AREA_RULES: &str = "rules";
/// The admin scope area the announcement routes live in.
const AREA_ANNOUNCEMENTS: &str = "announcements";
/// Ecto's `validate_required` refusal, the wording Pleroma's changeset
/// answers a blank text or content with.
const CANNOT_BE_BLANK: &str = "can't be blank";
/// Longest rule text the endpoint accepts (Pleroma's
/// `validate_length(:text, max: 500)`).
const MAX_RULE_TEXT: usize = 500;
/// Ecto's `validate_length` message for that bound.
const TOO_LONG: &str = "should be at most 500 character(s)";
/// `CastAndValidate`'s refusal for a value that is not a timestamp.
const INVALID_TIME: &str = "is invalid";
/// The refusal for a window whose end does not fall after its start (the
/// rule the ticket adds over Pleroma, which checks neither bound against
/// the other).
const ENDS_BEFORE_STARTS: &str = "must be after starts_at";
/// Default `limit` when the parameter is absent or unusable: Pleroma's
/// `default_limit` of the admin announcement listing.
const DEFAULT_LIMIT: i64 = 20;

/// The `/rules` and `/announcements` routes of the admin API, merged into
/// [`super`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/rules", get(list_rules).post(create_rule))
        .route("/rules/{id}", patch(update_rule).delete(destroy_rule))
        .route(
            "/announcements",
            get(list_announcements).post(create_announcement),
        )
        .route(
            "/announcements/{id}",
            get(show_announcement)
                .patch(update_announcement)
                .delete(destroy_announcement),
        )
}

/// What a content handler can fail with: the project error as-is, or
/// Pleroma's `CastAndValidate` 400 naming the refused fields.
#[derive(Debug)]
enum ContentError {
    /// An ordinary [`AppError`], rendered by its own `IntoResponse`.
    App(AppError),
    /// The 400 `{"errors":{"<field>":["<message>"]}}` the changeset and
    /// cast refusals of these bodies answer with — several fields at once
    /// when one body refused on several.
    Invalid(Vec<(&'static str, String)>),
}

/// A [`ContentError`] is a response: the wrapped [`AppError`] renders
/// itself, the invalid arm the pinned `{"errors": …}` 400.
impl IntoResponse for ContentError {
    fn into_response(self) -> Response {
        match self {
            Self::App(err) => err.into_response(),
            Self::Invalid(refusals) => {
                let mut errors = Map::new();
                for (field, message) in refusals {
                    let entry = Value::Array(vec![Value::String(message)]);
                    let replaced = errors.insert(field.to_owned(), entry);
                    drop(replaced);
                }
                (StatusCode::BAD_REQUEST, Json(json!({ "errors": errors }))).into_response()
            }
        }
    }
}

/// Every [`AppError`] the extractors, scope gates and models raise
/// travels through unchanged.
impl From<AppError> for ContentError {
    fn from(err: AppError) -> Self {
        Self::App(err)
    }
}

/// The handlers' result with [`ContentError`] as the error half.
type ContentResult<T> = Result<T, ContentError>;

/// The single refusal of one field, for the checks that cannot name two at
/// once.
fn refusal(field: &'static str, message: &str) -> ContentError {
    ContentError::Invalid(vec![(field, message.to_owned())])
}

/// The body of `POST /rules` and `PATCH /rules/{id}`: the three columns a
/// rule holds, every one read as an `Option` so `create` can name its own
/// missing field and `update` can tell absent from present. `priority`
/// deserializes through [`flex::opt_i32`], so a form request's `"3"` counts.
#[derive(Debug, Deserialize)]
struct RuleBody {
    /// The rule text; required on create.
    #[serde(default)]
    text: Option<String>,
    /// The guidance beside it; absent means empty on create.
    #[serde(default)]
    hint: Option<String>,
    /// The sort position; absent means `0` on create, the column default.
    #[serde(default, deserialize_with = "flex::opt_i32")]
    priority: Option<i32>,
}

/// The body of `POST /announcements` and `PATCH /announcements/{id}`: the
/// four parameters Pleroma's admin form sends, each an `Option` for the
/// same reason as [`RuleBody`]'s fields. The two window bounds are read as
/// text because the endpoint refuses an unparsable one in its own shape
/// rather than through the extractor's 422; `all_day` goes through
/// [`flex::opt_bool`].
#[derive(Debug, Deserialize)]
struct AnnouncementBody {
    /// The notice text; required on create.
    #[serde(default)]
    content: Option<String>,
    /// Start of the activity window, RFC 3339 text.
    #[serde(default)]
    starts_at: Option<String>,
    /// End of the activity window, same spelling.
    #[serde(default)]
    ends_at: Option<String>,
    /// Date-only display; absent means `false` on create.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    all_day: Option<bool>,
}

/// `GET /api/v1/pleroma/admin/rules` — every rule in display order, lowest
/// `priority` first and the `UUIDv7` id second (see
/// [`instance_rule::list`]).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::Database`] from the listing read.
async fn list_rules(
    State(state): State<AppState>,
    admin: AdminUser,
) -> ContentResult<Json<Vec<AdminRule>>> {
    admin.require(CLASS_READ, AREA_RULES)?;
    let rules = instance_rule::list(&state.db).await?;
    debug!(count = rules.len(), "admin instance rules listed");
    Ok(Json(AdminRule::from_rows(&rules)))
}

/// `POST /api/v1/pleroma/admin/rules` — Pleroma's `create`: the validated
/// text beside the `hint` and `priority` the body sent (or their defaults),
/// the `rule_create` log entry and the stored entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`ContentError::Invalid`] for the blank or
/// over-long text; [`AppError::Database`] from the insert and the log
/// write.
async fn create_rule(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<RuleBody>,
) -> ContentResult<Json<AdminRule>> {
    admin.require(CLASS_WRITE, AREA_RULES)?;
    let text = require_rule_text(body.text)?;
    let rule = instance_rule::create(
        &state.db,
        &text,
        body.hint.as_deref().unwrap_or_default(),
        body.priority.unwrap_or_default(),
    )
    .await?;
    let logged = moderation_log::rule_create(&state.db, &admin.0, rule.id).await?;
    debug!(id = %logged.id, rule = %rule.id, "admin instance rule created");
    Ok(Json(AdminRule::from_rule(&rule)))
}

/// `PATCH /api/v1/pleroma/admin/rules/{id}` — Pleroma's `update`: the
/// present-only write over the three columns, the whole-row rewrite the
/// model's [`instance_rule::update`] is, the `rule_update` log entry and
/// the rewritten entity.
///
/// # Errors
///
/// As [`create_rule`]'s gate and validation, plus
/// [`AppError::RouteNotFound`] for an id no rule carries and the reads and
/// writes in between.
async fn update_rule(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
    ApiBody(body): ApiBody<RuleBody>,
) -> ContentResult<Json<AdminRule>> {
    admin.require(CLASS_WRITE, AREA_RULES)?;
    let existing = load_rule(&state.db, &raw).await?;
    let text = optional_rule_text(body.text)?.unwrap_or(existing.text);
    let hint = body.hint.unwrap_or(existing.hint);
    let priority = body.priority.unwrap_or(existing.priority);
    let rule = instance_rule::update(&state.db, existing.id, &text, &hint, priority)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    let logged = moderation_log::rule_update(&state.db, &admin.0, rule.id).await?;
    debug!(id = %logged.id, rule = %rule.id, "admin instance rule updated");
    Ok(Json(AdminRule::from_rule(&rule)))
}

/// `DELETE /api/v1/pleroma/admin/rules/{id}` — Pleroma's `delete`: the row
/// gone, the `rule_delete` entry as the surviving trace, and the empty
/// object as the answer.
///
/// # Errors
///
/// As [`list_rules`]'s gate with the write class, plus [`load_rule`]'s 404
/// and [`AppError::Database`] from the delete and the log write.
async fn destroy_rule(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> ContentResult<Json<Value>> {
    admin.require(CLASS_WRITE, AREA_RULES)?;
    let existing = load_rule(&state.db, &raw).await?;
    let removed = instance_rule::delete(&state.db, existing.id).await?;
    let logged = moderation_log::rule_delete(&state.db, &admin.0, existing.id).await?;
    debug!(
        id = %logged.id,
        rule = %existing.id,
        removed,
        "admin instance rule deleted"
    );
    Ok(Json(empty_object()))
}

/// The rule the path names: the row behind a uuid `raw`, 404
/// `{"error":"Not found"}` for text that is no id or no stored rule.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] for either miss; [`AppError::Database`]
/// from the read.
async fn load_rule(pool: &PgPool, raw: &str) -> ContentResult<instance_rule::InstanceRule> {
    let Some(id) = parse_id(raw) else {
        return Err(AppError::RouteNotFound.into());
    };
    let rule = instance_rule::get(pool, id)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    Ok(rule)
}

/// The create-side `text`: absent or blank is the changeset's
/// `can't be blank`, present must also fit [`MAX_RULE_TEXT`].
///
/// # Errors
///
/// [`ContentError::Invalid`] on the `text` field.
fn require_rule_text(text: Option<String>) -> ContentResult<String> {
    let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
        return Err(refusal("text", CANNOT_BE_BLANK));
    };
    check_rule_text(&text)?;
    Ok(text)
}

/// The update-side `text`: absent keeps the stored one, present validates
/// exactly as on create — a blank present value is the same refusal rather
/// than a way to empty a rule out.
///
/// # Errors
///
/// [`ContentError::Invalid`] on the `text` field.
fn optional_rule_text(text: Option<String>) -> ContentResult<Option<String>> {
    match text {
        None => Ok(None),
        Some(text) => {
            let text = require_rule_text(Some(text))?;
            Ok(Some(text))
        }
    }
}

/// The length half of the text gate, by Unicode scalar value (what Ecto's
/// `validate_length` counts for a string).
///
/// # Errors
///
/// [`ContentError::Invalid`] with [`TOO_LONG`] past [`MAX_RULE_TEXT`].
fn check_rule_text(text: &str) -> ContentResult<()> {
    if text.chars().count() > MAX_RULE_TEXT {
        return Err(refusal("text", TOO_LONG));
    }
    Ok(())
}

/// `GET /api/v1/pleroma/admin/announcements` — the `limit` / `offset`
/// window over every notice the instance holds, newest first and window
/// aside (Pleroma's `list_paginated` through the same administration view
/// [`announcement::list_page`] is).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::Database`] from the listing read.
async fn list_announcements(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(params): Query<HashMap<String, String>>,
) -> ContentResult<Json<Vec<Announcement>>> {
    admin.require(CLASS_READ, AREA_ANNOUNCEMENTS)?;
    let limit = window_size(&params);
    let offset = window_offset(&params);
    let notices = announcement::list_page(&state.db, limit, offset).await?;
    debug!(
        count = notices.len(),
        limit, offset, "admin announcements listed"
    );
    Ok(Json(
        notices
            .iter()
            .map(|row| Announcement::from_row(row, false))
            .collect(),
    ))
}

/// `GET /api/v1/pleroma/admin/announcements/{id}` — the entity behind the
/// id, active, upcoming or expired: the administration read sees all three.
///
/// # Errors
///
/// As [`list_announcements`]'s gate, plus the 404 of [`load_notice`] and
/// the read it runs.
async fn show_announcement(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> ContentResult<Json<Announcement>> {
    admin.require(CLASS_READ, AREA_ANNOUNCEMENTS)?;
    let notice = load_notice(&state.db, &raw).await?;
    Ok(Json(Announcement::from_row(&notice, false)))
}

/// `POST /api/v1/pleroma/admin/announcements` — Pleroma's `create`: the
/// sanitized notice stored (its source kept as `raw_content`), the
/// `announcement_create` log entry, the `announcement` stream event for
/// every active local account and the created entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`ContentError::Invalid`] for the blank
/// content, the unparsable bound and the inverted window;
/// [`AppError::Internal`] should the finished entity ever fail to
/// serialize; [`AppError::Database`] from the insert and the log write.
async fn create_announcement(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<AnnouncementBody>,
) -> ContentResult<Json<Announcement>> {
    admin.require(CLASS_WRITE, AREA_ANNOUNCEMENTS)?;
    let raw = require_content(body.content)?;
    let window = window_of(body.starts_at, body.ends_at)?;
    check_window(window)?;
    let notice = announcement::create(
        &state.db,
        &announcement::NewAnnouncement {
            content: text_to_html(&raw),
            raw_content: raw,
            starts_at: window.0,
            ends_at: window.1,
            all_day: body.all_day.unwrap_or_default(),
        },
    )
    .await?;
    let logged = moderation_log::announcement_create(&state.db, &admin.0, notice.id).await?;
    debug!(id = %logged.id, announcement = %notice.id, "admin announcement created");
    let entity = Announcement::from_row(&notice, false);
    publish(&state, &entity).await?;
    Ok(Json(entity))
}

/// `PATCH /api/v1/pleroma/admin/announcements/{id}` — Pleroma's `change`:
/// the present-only write over the four parameters, each validated as on
/// create and the window checked as the merged pair, answering the
/// rewritten entity. An absent `content` re-serves the stored HTML and
/// keeps the stored source, so a write of the window alone changes nothing
/// else.
///
/// # Errors
///
/// As [`create_announcement`]'s gate and validation, plus
/// [`AppError::RouteNotFound`] for an id no notice carries and the reads
/// and writes in between.
async fn update_announcement(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
    ApiBody(body): ApiBody<AnnouncementBody>,
) -> ContentResult<Json<Announcement>> {
    admin.require(CLASS_WRITE, AREA_ANNOUNCEMENTS)?;
    let existing = load_notice(&state.db, &raw).await?;
    let content = optional_content(body.content)?;
    let sent = window_of(body.starts_at, body.ends_at)?;
    let window = (sent.0.or(existing.starts_at), sent.1.or(existing.ends_at));
    check_window(window)?;
    let (content, raw_content) = match content {
        Some(text) => (text_to_html(&text), text),
        None => (existing.content.clone(), existing.raw_content.clone()),
    };
    let id = existing.id;
    let notice = announcement::update(
        &state.db,
        id,
        &announcement::NewAnnouncement {
            content,
            raw_content,
            starts_at: window.0,
            ends_at: window.1,
            all_day: body.all_day.unwrap_or(existing.all_day),
        },
    )
    .await?
    .ok_or(AppError::RouteNotFound)?;
    let logged = moderation_log::announcement_update(&state.db, &admin.0, notice.id).await?;
    debug!(id = %logged.id, announcement = %notice.id, "admin announcement updated");
    Ok(Json(Announcement::from_row(&notice, false)))
}

/// `DELETE /api/v1/pleroma/admin/announcements/{id}` — Pleroma's `delete`:
/// the notice and its dismissal rows gone, the `announcement_delete` entry
/// recording it, and the empty object as the answer.
///
/// # Errors
///
/// As [`list_announcements`]'s gate with the write class, plus
/// [`load_notice`]'s 404 and [`AppError::Database`] from the delete and the
/// log write.
async fn destroy_announcement(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> ContentResult<Json<Value>> {
    admin.require(CLASS_WRITE, AREA_ANNOUNCEMENTS)?;
    let existing = load_notice(&state.db, &raw).await?;
    let removed = announcement::delete(&state.db, existing.id).await?;
    let logged = moderation_log::announcement_delete(&state.db, &admin.0, existing.id).await?;
    debug!(
        id = %logged.id,
        announcement = %existing.id,
        removed,
        "admin announcement deleted"
    );
    Ok(Json(empty_object()))
}

/// The notice the path names: the row behind a uuid `raw`, 404
/// `{"error":"Not found"}` for text that is no id or no stored notice —
/// Pleroma's `get_by_id` miss through the `FallbackController`.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] for either miss; [`AppError::Database`]
/// from the read.
async fn load_notice(pool: &PgPool, raw: &str) -> ContentResult<announcement::Announcement> {
    let Some(id) = parse_id(raw) else {
        return Err(AppError::RouteNotFound.into());
    };
    let notice = announcement::get(pool, id)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    Ok(notice)
}

/// The create-side `content`: absent or blank is Pleroma's
/// `validate_required` refusal in the map shape the ticket pins.
///
/// # Errors
///
/// [`ContentError::Invalid`] on the `content` field.
fn require_content(content: Option<String>) -> ContentResult<String> {
    content
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| refusal("content", CANNOT_BE_BLANK))
}

/// The update-side `content`: absent keeps the stored text, a present one
/// must not be blank.
///
/// # Errors
///
/// [`ContentError::Invalid`] on the `content` field.
fn optional_content(content: Option<String>) -> ContentResult<Option<String>> {
    content.map_or_else(|| Ok(None), |text| require_content(Some(text)).map(Some))
}

/// A one-notice activity window: the two bounds as the handler merged them
/// out of its body and the row it is writing, `None` on either side being
/// an open end.
type Window = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// The two window bounds of one body, parsed out of their text: absent (or
/// `null`, which reads as absent) is no bound to write, present must be RFC
/// 3339 text.
///
/// # Errors
///
/// [`ContentError::Invalid`] naming every bound that failed to parse, so
/// one body reports both of them.
fn window_of(starts_at: Option<String>, ends_at: Option<String>) -> ContentResult<Window> {
    let mut refusals: Vec<(&'static str, String)> = Vec::new();
    let start = bound_of(starts_at, "starts_at", &mut refusals);
    let end = bound_of(ends_at, "ends_at", &mut refusals);
    if !refusals.is_empty() {
        return Err(ContentError::Invalid(refusals));
    }
    Ok((start, end))
}

/// One bound of [`window_of`]: `None` when nothing was sent or it did not
/// parse — the latter refusal appended to `refusals` as it is found, so the
/// two bounds are reported together.
fn bound_of(
    text: Option<String>,
    field: &'static str,
    refusals: &mut Vec<(&'static str, String)>,
) -> Option<DateTime<Utc>> {
    let text = text?;
    parse_time(&text).or_else(|| {
        refusals.push((field, INVALID_TIME.to_owned()));
        None
    })
}

/// The window rule of the ticket: an end that is there must fall strictly
/// after a start that is there. Half-open and open windows are nobody to
/// refuse.
///
/// # Errors
///
/// [`ContentError::Invalid`] on the `ends_at` field with
/// [`ENDS_BEFORE_STARTS`].
fn check_window(window: Window) -> ContentResult<()> {
    if window
        .0
        .is_some_and(|start| window.1.is_some_and(|end| end <= start))
    {
        return Err(refusal("ends_at", ENDS_BEFORE_STARTS));
    }
    Ok(())
}

/// Publishes `entity` as Mastodon's `announcement` event: the entity as
/// JSON text, which is what a socket ships to every active local account
/// (see [`announcement_published`]).
///
/// # Errors
///
/// [`AppError::Internal`] when the entity fails to serialize, which for a
/// plain [`Serialize`] struct of stored strings and timestamps cannot
/// happen but is not something to answer a client with by panicking.
///
/// [`announcement_published`]: crate::services::streaming::announcement_published
async fn publish(state: &AppState, entity: &Announcement) -> AppResult<()> {
    let payload = serde_json::to_value(entity)
        .map_err(|err| AppError::Internal(format!("announcement entity: {err}")))?;
    streaming::announcement_published(state, &payload).await;
    Ok(())
}

/// The `limit` of the announcement window: Pleroma's
/// [`DEFAULT_LIMIT`] for an absent or unusable parameter, capped at the
/// admin listings' ceiling like every other window here.
fn window_size(params: &HashMap<String, String>) -> i64 {
    let text = super::text_of(params, "limit");
    super::page_number_of(text.as_deref(), DEFAULT_LIMIT).min(super::MAX_PAGE_SIZE)
}

/// The `offset` of the announcement window: `0` for an absent, unparsable
/// or negative parameter, as Pleroma's `Map.get(params, :offset, 0)` reads
/// it (its spec asks for a non-negative integer, and the offset has no
/// default of its own to fall back to).
fn window_offset(params: &HashMap<String, String>) -> i64 {
    super::text_of(params, "offset")
        .and_then(|text| text.parse::<i64>().ok())
        .filter(|value| *value >= 0)
        .unwrap_or_default()
}

/// The `{}` of Pleroma's `json(conn, %{})`: what both deletes answer.
fn empty_object() -> Value {
    Value::Object(Map::new())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A rule body parsed from `value`.
    fn rule_body(value: Value) -> RuleBody {
        serde_json::from_value(value).expect("the fixture body deserializes")
    }

    /// An announcement body parsed from `value`.
    fn notice_body(value: Value) -> AnnouncementBody {
        serde_json::from_value(value).expect("the fixture body deserializes")
    }

    /// Whether `err` is the one-field refusal naming `field` with `message`.
    fn is_refusal(err: &ContentError, field: &str, message: &str) -> bool {
        matches!(
            err,
            ContentError::Invalid(refusals)
                if refusals.len() == 1
                    && refusals.first().is_some_and(|(got, text)| *got == field && text == message)
        )
    }

    #[test]
    fn the_bodies_read_the_json_and_form_spellings() {
        let full = rule_body(json!({"text": "Be nice", "hint": "  ", "priority": 3}));
        assert_eq!(full.text.as_deref(), Some("Be nice"));
        assert_eq!(full.hint.as_deref(), Some("  "), "a blank hint is stored");
        assert_eq!(full.priority, Some(3));

        let form = rule_body(json!({"text": "No spam", "priority": "-5"}));
        assert_eq!(
            form.priority,
            Some(-5),
            "a form request's number string reads"
        );

        let empty = rule_body(json!({}));
        assert_eq!(empty.text, None, "no text");
        assert_eq!(empty.hint, None, "no hint");
        assert_eq!(empty.priority, None, "and no priority");

        let notice = notice_body(json!({
            "content": "Down <br> tomorrow",
            "starts_at": "2026-02-01T00:00:00Z",
            "ends_at": null,
            "all_day": "true",
        }));
        assert_eq!(notice.content.as_deref(), Some("Down <br> tomorrow"));
        assert_eq!(
            notice.starts_at.as_deref(),
            Some("2026-02-01T00:00:00Z"),
            "a bound stays text until the handler parses it"
        );
        assert_eq!(notice.ends_at, None, "a null reads as absent");
        assert_eq!(notice.all_day, Some(true), "the form spelling of true");
        assert!(
            notice_body(json!({"content": "x"})).all_day.is_none(),
            "and an absent gate is no decision"
        );
    }

    #[test]
    fn a_create_needs_a_text_that_fits() {
        let absent = require_rule_text(None).expect_err("no text is no rule");
        assert!(
            is_refusal(&absent, "text", CANNOT_BE_BLANK),
            "absent: {absent:?}"
        );
        for blank in ["", "   ", "\n\t"] {
            let err = require_rule_text(Some(blank.to_owned()))
                .expect_err(&format!("{blank:?} is blank"));
            assert!(is_refusal(&err, "text", CANNOT_BE_BLANK), "and so is it");
        }
        let long = "x".repeat(MAX_RULE_TEXT + 1);
        let err = require_rule_text(Some(long)).expect_err("one past the bound refuses");
        assert!(is_refusal(&err, "text", TOO_LONG), "the length wording");

        let exact = "y".repeat(MAX_RULE_TEXT);
        assert_eq!(
            require_rule_text(Some(exact.clone())).expect("the bound itself is inclusive"),
            exact,
            "and the text passes verbatim, spaces included"
        );
        assert_eq!(
            require_rule_text(Some(" Be nice ".to_owned())).expect("a padded text stands"),
            " Be nice ",
            "the value is not trimmed on the way in"
        );
    }

    #[test]
    fn an_update_validates_only_the_text_it_is_given() {
        assert_eq!(
            optional_rule_text(None).expect("absent text keeps the stored one"),
            None
        );
        assert_eq!(
            optional_rule_text(Some("new".to_owned())).expect("a present text is checked"),
            Some("new".to_owned())
        );
        let err = optional_rule_text(Some("  ".to_owned()))
            .expect_err("a present blank text refuses rather than emptying a rule");
        assert!(is_refusal(&err, "text", CANNOT_BE_BLANK));
    }

    #[test]
    fn window_bounds_parse_rfc_3339_and_refuse_the_rest() {
        let (start, end) = window_of(
            Some("2026-02-01T00:00:00Z".to_owned()),
            Some("2026-02-02T12:34:56.500+02:00".to_owned()),
        )
        .expect("both bounds parse");
        assert_eq!(
            start.map(|moment| moment.to_rfc3339()),
            Some("2026-02-01T00:00:00+00:00".to_owned())
        );
        assert_eq!(
            end.map(|moment| moment.to_rfc3339()),
            Some("2026-02-02T10:34:56.500+00:00".to_owned()),
            "an offset folds to UTC"
        );
        assert_eq!(
            window_of(None, None).expect("no bounds is no window"),
            (None, None)
        );

        let err = window_of(Some("01/02/2026".to_owned()), Some("tomorrow".to_owned()))
            .expect_err("neither bound parses");
        let expected = vec![
            ("starts_at", INVALID_TIME.to_owned()),
            ("ends_at", INVALID_TIME.to_owned()),
        ];
        assert!(
            matches!(&err, ContentError::Invalid(refusals) if *refusals == expected),
            "both bounds report together, in body order: {err:?}"
        );
    }

    #[test]
    fn a_window_ends_after_it_starts() {
        let start = parse_time("2026-02-01T00:00:00Z");
        let later = parse_time("2026-02-02T00:00:00Z");
        check_window((start, later)).expect("a forward window stands");
        check_window((None, later)).expect("an open start refuses nothing");
        check_window((start, None)).expect("nor does an open end");
        check_window((None, None)).expect("and a fully open window is nobody's problem");

        let err = check_window((start, start)).expect_err("the same instant is not after itself");
        assert!(
            is_refusal(&err, "ends_at", ENDS_BEFORE_STARTS),
            "the refusal is on the end"
        );
        let backwards = check_window((later, start)).expect_err("a backwards window refuses");
        assert!(is_refusal(&backwards, "ends_at", ENDS_BEFORE_STARTS));
    }

    #[test]
    fn the_window_parameters_default_and_cap_like_the_other_admin_lists() {
        let map = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect()
        };
        assert_eq!(window_size(&map(&[])), DEFAULT_LIMIT, "absent limit");
        assert_eq!(
            window_size(&map(&[("limit", "junk")])),
            DEFAULT_LIMIT,
            "unusable limit"
        );
        assert_eq!(
            window_size(&map(&[("limit", "0")])),
            DEFAULT_LIMIT,
            "and a zero"
        );
        assert_eq!(
            window_size(&map(&[("limit", "5")])),
            5,
            "a usable one passes"
        );
        assert_eq!(
            window_size(&map(&[("limit", "100000")])),
            super::super::MAX_PAGE_SIZE,
            "past the admin ceiling it is capped"
        );

        assert_eq!(window_offset(&map(&[])), 0, "absent offset");
        assert_eq!(
            window_offset(&map(&[("offset", "-4")])),
            0,
            "a negative one"
        );
        assert_eq!(
            window_offset(&map(&[("offset", "x")])),
            0,
            "and an unparsable one"
        );
        assert_eq!(
            window_offset(&map(&[("offset", "0")])),
            0,
            "zero is the first row"
        );
        assert_eq!(window_offset(&map(&[("offset", "40")])), 40, "40 skips 40");
    }

    #[test]
    fn the_error_halves_render_their_own_bodies() {
        let refused = refusal("text", CANNOT_BE_BLANK).into_response();
        assert_eq!(
            refused.status(),
            StatusCode::BAD_REQUEST,
            "a changeset refusal is a 400"
        );
        let wrapped = ContentError::App(AppError::RouteNotFound).into_response();
        assert_eq!(
            wrapped.status(),
            StatusCode::NOT_FOUND,
            "and a wrapped app error keeps its own status"
        );
        let converted: ContentError = AppError::NotFound.into();
        assert!(
            matches!(converted, ContentError::App(AppError::NotFound)),
            "every app error converts for the question mark"
        );
    }

    #[test]
    fn the_deletes_answer_an_empty_object() {
        assert_eq!(empty_object(), json!({}), "the empty JSON object");
    }
}
