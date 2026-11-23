//! The admin webhook endpoints: the event-delivery maintenance API under
//! `/api/v1/pleroma/admin/webhooks` (WBE-209).
//!
//! The reference is Pleroma's `AdminAPI.WebhookController`
//! (`lib/pleroma/web/admin_api/controllers/webhook_controller.ex`), routed
//! in `lib/pleroma/web/router.ex`, over the `Pleroma.Webhook` store
//! (`lib/pleroma/webhook.ex`). Pleroma gates the reads with `admin:read`
//! and the writes with `admin:write`; the same split lives here as the
//! [`AdminUser::require`] call each handler makes first, on the area
//! `webhooks`. The entity is the six keys the ticket pins — `id`, `url`,
//! `events`, `secret`, `enabled`, `internal` — rendered by
//! [`crate::entities::admin::AdminWebhook`]; unlike Pleroma's view it
//! carries no timestamps.
//!
//! - `GET /webhooks` (`index`) — a `page` / `page_size` window (the admin
//!   listings' defaults: page 1, 50 rows, capped at 200) over every
//!   webhook ordered by id. Pleroma answers the unordered `Repo.all` as a
//!   bare array; the ticket pins the array *and* the order.
//! - `GET /webhooks/{id}` (`show`) — the entity; an id that is no uuid or
//!   no row is 404 `{"error":"Not found"}`, as everywhere here.
//! - `POST /webhooks` (`create`) — `url` (a required `http(s)` URL) and
//!   `events` (every element one of
//!   [`crate::models::webhook::KNOWN_EVENTS`]), with `enabled` defaulting
//!   to true; the signing secret is minted by the model and answered in
//!   the entity. A body without `url` is the 400
//!   `{"errors":{"url":["Missing parameter: url"]}}` and a bad URL or an
//!   unknown event the 400 `{"errors":{"url":["invalid url"]}}` /
//!   `{"errors":{"events":["invalid event"]}}` — Pleroma's
//!   `CastAndValidate` renders its refusals as `{"errors": …}` maps, and
//!   the ticket pins the events wording. `internal` is never a parameter
//!   (Pleroma's `webhook_params` drops it likewise): a created webhook is
//!   always external. Records one `webhook_create` [`moderation_log`]
//!   entry.
//! - `PATCH /webhooks/{id}` (`update`) — the present-only write over `url`
//!   / `events` / `enabled` (same validation), answering the updated
//!   entity and logging `webhook_update`.
//! - `DELETE /webhooks/{id}` (`delete`) — 204 once the row is gone, with
//!   the `webhook_delete` [`moderation_log`] entry as its surviving trace.
//!   An internal webhook cannot be deleted: Pleroma 403s every write on
//!   one, and the ticket widens the delete into the 422
//!   `{"error":"Cannot delete internal webhook"}`; the other writes keep
//!   Pleroma's 403 `{"error":"Forbidden"}`.
//! - `POST /webhooks/{id}/enable` and `/disable` (`enable`, `disable`) —
//!   the entity with `enabled` set to the route's value.
//! - `POST /webhooks/{id}/rotate_secret` (`rotate_secret`) — the entity
//!   with a freshly minted secret, logged as `webhook_rotate_secret`.
//!
//! [`AdminUser::require`]: crate::auth::admin::AdminUser::require
//! [`moderation_log`]: crate::models::moderation_log

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tracing::debug;

use crate::{
    auth::admin::AdminUser,
    entities::admin::AdminWebhook,
    error::AppError,
    extract::{ApiBody, flex},
    models::{moderation_log, webhook},
    state::AppState,
    util::{id::parse_id, url::is_http_url},
};

/// The read endpoint class of [`AdminUser::require`].
const CLASS_READ: &str = "read";
/// The write endpoint class of [`AdminUser::require`].
const CLASS_WRITE: &str = "write";
/// The admin scope area these endpoints live in
/// (`admin:read:webhooks` / `admin:write:webhooks`).
const AREA_WEBHOOKS: &str = "webhooks";
/// 400 `errors.url` of a create body that carries no `url`.
const MISSING_URL: &str = "Missing parameter: url";
/// 400 `errors.url` of a `url` that is no parsable `http(s)` URL.
const INVALID_URL: &str = "invalid url";
/// 400 `errors.events` of a body naming an event off
/// [`webhook::KNOWN_EVENTS`] (the wording the ticket pins).
const INVALID_EVENT: &str = "invalid event";
/// 403 body of every write onto an internal webhook (Pleroma's
/// `AdminAPI.FallbackController` `:forbidden` wording).
const FORBIDDEN: &str = "Forbidden";
/// 422 body of `DELETE /webhooks/{id}` for an internal webhook (the
/// wording the ticket pins).
const CANNOT_DELETE_INTERNAL: &str = "Cannot delete internal webhook";

/// The `/webhooks` routes of the admin API, merged into [`super`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/webhooks", get(list).post(create))
        .route("/webhooks/{id}", get(show).patch(update).delete(destroy))
        .route("/webhooks/{id}/enable", post(enable))
        .route("/webhooks/{id}/disable", post(disable))
        .route("/webhooks/{id}/rotate_secret", post(rotate_secret))
}

/// What a handler can fail with: the project error as-is, or Pleroma's
/// `CastAndValidate` 400 naming one field.
#[derive(Debug)]
enum WebhookError {
    /// An ordinary [`AppError`], rendered by its own `IntoResponse`.
    App(AppError),
    /// The 400 `{"errors":{"<field>":["<message>"]}}` the ticket pins for
    /// validation refusals (Pleroma's api-spec error map, narrowed to the
    /// single-field shape these bodies can trip).
    Invalid {
        /// The offending body parameter (`url` or `events`).
        field: &'static str,
        /// The refusal message the field answers with.
        message: &'static str,
    },
}

/// The handlers' result with [`WebhookError`] as the error half.
type WebhookResult<T> = Result<T, WebhookError>;

/// A [`WebhookError`] is a response: the wrapped [`AppError`] renders
/// itself, the invalid arm the pinned `{"errors": …}` 400.
impl IntoResponse for WebhookError {
    fn into_response(self) -> Response {
        match self {
            Self::App(err) => err.into_response(),
            Self::Invalid { field, message } => {
                let mut errors = Map::new();
                let replaced: Option<Value> = errors.insert(field.to_owned(), json!([message]));
                drop(replaced);
                (StatusCode::BAD_REQUEST, Json(json!({ "errors": errors }))).into_response()
            }
        }
    }
}

/// Every [`AppError`] the extractors, scope gates and models raise
/// travels through unchanged.
impl From<AppError> for WebhookError {
    fn from(err: AppError) -> Self {
        Self::App(err)
    }
}

/// The body of `POST /webhooks` and `PATCH /webhooks/{id}`: the three
/// mutable fields of the entity. All read as `Option`s — `create` demands
/// its `url`, `update` is present-only — and `enabled` deserializes
/// through [`flex::opt_bool`] so a form-encoded `"false"` counts.
#[derive(Debug, Deserialize)]
struct WebhookBody {
    /// The delivery target; required by `create`, kept-as-stored by an
    /// `update` that omits it.
    #[serde(default)]
    url: Option<String>,
    /// The subscribed events; every element must be known.
    #[serde(default)]
    events: Option<Vec<String>>,
    /// The delivery gate; absent means enabled on create (the column
    /// default Pleroma's schema carries) and untouched on update.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    enabled: Option<bool>,
}

/// `GET /api/v1/pleroma/admin/webhooks` — every webhook ordered by id,
/// windowed by the admin listing's `page` / `page_size` (defaults 1 / 50,
/// capped like [`super`]'s log listing), as a JSON array.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::Database`] from the listing
/// read.
async fn list(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(params): Query<HashMap<String, String>>,
) -> WebhookResult<Json<Vec<AdminWebhook>>> {
    admin.require(CLASS_READ, AREA_WEBHOOKS)?;
    let page = super::page_number(&params, "page", super::DEFAULT_PAGE);
    let size = super::page_size(&params);
    let offset = (page - 1).saturating_mul(size);
    let rows = webhook::list(&state.db, size, offset).await?;
    debug!(count = rows.len(), page, "admin webhooks listed");
    Ok(Json(AdminWebhook::from_rows(&rows)))
}

/// `GET /api/v1/pleroma/admin/webhooks/{id}` — the entity behind the id,
/// internal webhooks included.
///
/// # Errors
///
/// As [`list`]'s gate, plus the 404 of [`load`] and the read it runs.
async fn show(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> WebhookResult<Json<AdminWebhook>> {
    admin.require(CLASS_READ, AREA_WEBHOOKS)?;
    let hook = load(&state, &raw).await?;
    Ok(Json(AdminWebhook::from_webhook(&hook)))
}

/// `POST /api/v1/pleroma/admin/webhooks` — Pleroma's `create`: validate
/// the body, insert with a minted id and secret, record the
/// `webhook_create` log entry and answer the stored entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`WebhookError::Invalid`] for the missing
/// or unroutable `url` and the unknown event; [`AppError::Database`] from
/// the insert and the log write.
async fn create(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<WebhookBody>,
) -> WebhookResult<Json<AdminWebhook>> {
    admin.require(CLASS_WRITE, AREA_WEBHOOKS)?;
    let url = require_url(body.url.as_deref())?;
    check_events(body.events.as_deref())?;
    let events = body.events.unwrap_or_default();
    let enabled = body.enabled.unwrap_or(true);
    let hook = webhook::create(&state.db, url, &events, enabled).await?;
    let logged = moderation_log::webhook_create(&state.db, &admin.0, hook.id).await?;
    debug!(
        id = %logged.id,
        webhook = %hook.id,
        enabled = hook.enabled,
        "admin webhook created"
    );
    Ok(Json(AdminWebhook::from_webhook(&hook)))
}

/// `PATCH /api/v1/pleroma/admin/webhooks/{id}` — Pleroma's `update`:
/// present-only over `url` / `events` / `enabled` (each validated as on
/// create), answering the updated entity and recording `webhook_update`.
/// An internal webhook is the 403 Pleroma's `internal: true` arm gives.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`],
/// its `admin:write` gate, and [`reject_internal`]; [`WebhookError::Invalid`]
/// from [`check_url`] / [`check_events`]; [`AppError::Database`] from the
/// read, the update and the log write.
async fn update(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
    ApiBody(body): ApiBody<WebhookBody>,
) -> WebhookResult<Json<AdminWebhook>> {
    admin.require(CLASS_WRITE, AREA_WEBHOOKS)?;
    let existing = load(&state, &raw).await?;
    reject_internal(&existing)?;
    check_url(body.url.as_deref())?;
    check_events(body.events.as_deref())?;
    let changes = webhook::WebhookChanges {
        url: body.url,
        events: body.events,
        enabled: body.enabled,
    };
    let hook = webhook::update(&state.db, existing.id, &changes)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    let logged = moderation_log::webhook_update(&state.db, &admin.0, hook.id).await?;
    debug!(id = %logged.id, webhook = %hook.id, "admin webhook updated");
    Ok(Json(AdminWebhook::from_webhook(&hook)))
}

/// `DELETE /api/v1/pleroma/admin/webhooks/{id}` — Pleroma's `delete`: 204
/// once the row is gone, its `webhook_delete` log entry the surviving
/// trace. An internal webhook instead answers the ticket's 422.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] for an internal
/// webhook; [`AppError::Database`] from the read, the delete and the log
/// write.
async fn destroy(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> WebhookResult<StatusCode> {
    admin.require(CLASS_WRITE, AREA_WEBHOOKS)?;
    let existing = load(&state, &raw).await?;
    if existing.internal {
        let refusal = AppError::Unprocessable(CANNOT_DELETE_INTERNAL.to_owned());
        return Err(refusal.into());
    }
    let removed = webhook::delete(&state.db, existing.id).await?;
    let logged = moderation_log::webhook_delete(&state.db, &admin.0, existing.id).await?;
    debug!(
        id = %logged.id,
        webhook = %existing.id,
        removed,
        "admin webhook deleted"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/pleroma/admin/webhooks/{id}/enable` — the entity with
/// `enabled` set to true.
///
/// # Errors
///
/// As [`show`]'s gate with the write class, plus [`reject_internal`] and
/// the write [`set_gate`] runs.
async fn enable(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> WebhookResult<Json<AdminWebhook>> {
    admin.require(CLASS_WRITE, AREA_WEBHOOKS)?;
    set_gate(&state, &raw, true).await
}

/// `POST /api/v1/pleroma/admin/webhooks/{id}/disable` — the entity with
/// `enabled` set to false.
///
/// # Errors
///
/// As [`enable`]'s.
async fn disable(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> WebhookResult<Json<AdminWebhook>> {
    admin.require(CLASS_WRITE, AREA_WEBHOOKS)?;
    set_gate(&state, &raw, false).await
}

/// `POST /api/v1/pleroma/admin/webhooks/{id}/rotate_secret` — Pleroma's
/// `rotate_secret`: a freshly minted secret over the stored one, logged as
/// `webhook_rotate_secret`. The entity answers with the new secret; the
/// log line names only the row.
///
/// # Errors
///
/// As [`enable`]'s, with the rotation in place of the gate write.
async fn rotate_secret(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> WebhookResult<Json<AdminWebhook>> {
    admin.require(CLASS_WRITE, AREA_WEBHOOKS)?;
    let existing = load(&state, &raw).await?;
    reject_internal(&existing)?;
    let hook = webhook::rotate_secret(&state.db, existing.id)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    let logged = moderation_log::webhook_rotate_secret(&state.db, &admin.0, hook.id).await?;
    debug!(id = %logged.id, webhook = %hook.id, "admin webhook secret rotated");
    Ok(Json(AdminWebhook::from_webhook(&hook)))
}

/// The webhook the path names: the row behind a uuid `raw`, 404
/// `{"error":"Not found"}` for text that is no id or no stored row —
/// Pleroma's `Webhook.get` miss through the `FallbackController`.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] for either miss; [`AppError::Database`]
/// from the read.
async fn load(state: &AppState, raw: &str) -> WebhookResult<webhook::Webhook> {
    let Some(id) = parse_id(raw) else {
        return Err(AppError::RouteNotFound.into());
    };
    let hook = webhook::get(&state.db, id)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    Ok(hook)
}

/// Pleroma's `internal: true` refusal: no admin write touches a webhook
/// the instance installed itself (the delete answers its own 422 in
/// [`destroy`]).
///
/// # Errors
///
/// [`AppError::Forbidden`] with Pleroma's `FallbackController` wording for
/// an internal webhook.
fn reject_internal(hook: &webhook::Webhook) -> WebhookResult<()> {
    if hook.internal {
        return Err(AppError::Forbidden(FORBIDDEN.to_owned()).into());
    }
    Ok(())
}

/// The gate write the enable and disable routes share: fetch, refuse an
/// internal webhook, set `enabled`, answer the entity.
///
/// # Errors
///
/// As [`load`] and [`reject_internal`], plus [`AppError::Database`] from
/// the gate write.
async fn set_gate(state: &AppState, raw: &str, enabled: bool) -> WebhookResult<Json<AdminWebhook>> {
    let existing = load(state, raw).await?;
    reject_internal(&existing)?;
    let hook = webhook::set_enabled(&state.db, existing.id, enabled)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    debug!(
        webhook = %hook.id,
        enabled = hook.enabled,
        "admin webhook gate set"
    );
    Ok(Json(AdminWebhook::from_webhook(&hook)))
}

/// The create-side `url`: absent (or blank) is the missing-parameter 400,
/// present-but-unroutable the invalid-url 400.
///
/// # Errors
///
/// [`WebhookError::Invalid`] with [`MISSING_URL`] or [`INVALID_URL`].
fn require_url(url: Option<&str>) -> WebhookResult<&str> {
    let Some(text) = url.filter(|text| !text.trim().is_empty()) else {
        return Err(WebhookError::Invalid {
            field: "url",
            message: MISSING_URL,
        });
    };
    if is_http_url(text) {
        Ok(text)
    } else {
        Err(WebhookError::Invalid {
            field: "url",
            message: INVALID_URL,
        })
    }
}

/// The update-side `url`: absent keeps the stored one, present must
/// validate as on create.
///
/// # Errors
///
/// [`WebhookError::Invalid`] with [`INVALID_URL`] for a present bad url.
fn check_url(url: Option<&str>) -> WebhookResult<()> {
    match url {
        Some(text) if !is_http_url(text) => Err(WebhookError::Invalid {
            field: "url",
            message: INVALID_URL,
        }),
        Some(_) | None => Ok(()),
    }
}

/// The `events` of either body: absent is none to check, present must be
/// all [`webhook::KNOWN_EVENTS`].
///
/// # Errors
///
/// [`WebhookError::Invalid`] with [`INVALID_EVENT`] for any unknown
/// element.
fn check_events(events: Option<&[String]>) -> WebhookResult<()> {
    let bad = events.is_some_and(|list| list.iter().any(|event| !webhook::is_known_event(event)));
    if bad {
        return Err(WebhookError::Invalid {
            field: "events",
            message: INVALID_EVENT,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Whether `err` is the invalid-field 400 naming `field` and `message`.
    fn is_invalid(err: &WebhookError, field: &str, message: &str) -> bool {
        matches!(
            err,
            WebhookError::Invalid { field: got, message: got_message }
                if *got == field && *got_message == message
        )
    }

    /// The body parses: the ticket's JSON shape, the form request's string
    /// booleans, and an empty body as all-`None` for the two handlers to
    /// read their own defaults out of.
    #[test]
    fn the_body_parses_the_ticket_shape() {
        let full: WebhookBody = serde_json::from_value(json!({
            "url": "https://example.org/hook",
            "events": ["report.created", "account.created"],
            "enabled": false,
        }))
        .expect("the ticket's shape deserializes");
        assert_eq!(full.url.as_deref(), Some("https://example.org/hook"));
        assert_eq!(
            full.events.as_deref(),
            Some(&["report.created".to_owned(), "account.created".to_owned()][..]),
            "the event list round-trips"
        );
        assert_eq!(full.enabled, Some(false), "a real boolean");

        let form: WebhookBody = serde_json::from_value(json!({"enabled": "false"}))
            .expect("the form spelling reads through flex");
        assert_eq!(form.enabled, Some(false), "the stringly-typed gate");
        let empty: WebhookBody =
            serde_json::from_value(json!({})).expect("an empty body deserializes");
        assert_eq!(empty.url, None, "no url");
        assert_eq!(empty.events, None, "no events");
        assert_eq!(empty.enabled, None, "and no gate: the handlers default");
    }

    /// The create-side url gate: absent and blank are the missing-
    /// parameter refusal, unroutable text the invalid-url one, and both
    /// web schemes pass verbatim.
    #[test]
    fn the_create_url_gate_names_its_refusal() {
        let absent = require_url(None).expect_err("no url is no create");
        assert!(
            is_invalid(&absent, "url", MISSING_URL),
            "absent: {absent:?}"
        );
        let blank = require_url(Some("   ")).expect_err("blank is absent");
        assert!(
            is_invalid(&blank, "url", MISSING_URL),
            "blank reads as missing"
        );
        let empty = require_url(Some("")).expect_err("empty is absent");
        assert!(
            is_invalid(&empty, "url", MISSING_URL),
            "and so is the empty string"
        );
        for bad in [
            "ftp://example.org/hook",
            "not a url",
            "mailto:hook@example.org",
        ] {
            let err = require_url(Some(bad)).expect_err(&format!("{bad} is no http(s) url"));
            assert!(
                is_invalid(&err, "url", INVALID_URL),
                "present but bad: {err:?}"
            );
        }
        assert_eq!(
            require_url(Some("https://example.org/hook")).expect("https is routable"),
            "https://example.org/hook",
            "the text passes verbatim"
        );
        assert_eq!(
            require_url(Some("http://example.org")).expect("http is routable"),
            "http://example.org",
            "plain http included"
        );
    }

    /// The update-side url check: absent keeps the stored one, present
    /// must validate exactly as on create.
    #[test]
    fn the_update_url_check_only_screens_what_is_present() {
        check_url(None).expect("an absent url changes nothing");
        check_url(Some("https://example.org/moved")).expect("a valid url stands");
        let err =
            check_url(Some("gopher://example.org")).expect_err("a present url must still validate");
        assert!(is_invalid(&err, "url", INVALID_URL), "the same refusal");
    }

    /// The events check: absent is none to check, the three known events
    /// pass in any mix, and anything off the list is the pinned refusal.
    #[test]
    fn events_must_all_be_known() {
        check_events(None).expect("absent events check nothing");
        check_events(Some(&[])).expect("the empty list is no subscription");
        let known: Vec<String> = webhook::KNOWN_EVENTS
            .iter()
            .map(|e| (*e).to_owned())
            .collect();
        check_events(Some(&known)).expect("every known event passes together");
        for bad in ["report.deleted", "account.deleted", "report.created ", ""] {
            let err = check_events(Some(&[bad.to_owned()]))
                .expect_err(&format!("{bad} is no known event"));
            assert!(
                is_invalid(&err, "events", INVALID_EVENT),
                "the pinned wording"
            );
        }
        let mixed = vec!["report.created".to_owned(), "nonsense".to_owned()];
        let err = check_events(Some(&mixed)).expect_err("one bad element spoils the list");
        assert!(
            is_invalid(&err, "events", INVALID_EVENT),
            "checked elementwise"
        );
    }

    /// The url screen itself, as the two handlers use it.
    #[test]
    fn only_http_and_https_with_a_host_are_urls() {
        assert!(
            is_http_url("https://example.org/a?b=c#d"),
            "https with a path"
        );
        assert!(
            is_http_url("http://127.0.0.1:8080/hook"),
            "http with a port"
        );
        assert!(
            !is_http_url("ftp://example.org"),
            "ftp is no delivery scheme"
        );
        assert!(!is_http_url("https://"), "a scheme alone has no host");
        assert!(!is_http_url("example.org/hook"), "no scheme is no url");
    }

    /// The two error halves render their pinned bodies: the invalid arm
    /// the 400 `{"errors":{…}}` map, the wrapped arm the usual [`AppError`].
    #[test]
    fn the_error_halves_render_the_pinned_bodies() {
        let response = WebhookError::Invalid {
            field: "events",
            message: INVALID_EVENT,
        }
        .into_response();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "the validation refusal is a 400"
        );

        let missing = WebhookError::Invalid {
            field: "url",
            message: MISSING_URL,
        }
        .into_response();
        assert_eq!(
            missing.status(),
            StatusCode::BAD_REQUEST,
            "as is the absent url"
        );

        let wrapped = WebhookError::App(AppError::RouteNotFound).into_response();
        assert_eq!(
            wrapped.status(),
            StatusCode::NOT_FOUND,
            "the wrapped arm keeps its status"
        );
    }

    /// An [`AppError`] converts into the wrapper through `From`, which is
    /// what every `?` in the handlers rides on.
    #[test]
    fn an_app_error_wraps_for_the_question_mark() {
        let wrapped: WebhookError = AppError::Forbidden(FORBIDDEN.to_owned()).into();
        assert!(
            matches!(wrapped, WebhookError::App(_)),
            "the App arm carries it"
        );
    }
}
