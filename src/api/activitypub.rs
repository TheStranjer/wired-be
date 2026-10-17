//! `ActivityPub` routes: the URLs other instances fetch actors and
//! collections at (`docs/ARCHITECTURE.md` decision 3).
//!
//! This router is merged at the top level of [`crate::app`], not under
//! `/api`: the URLs it serves are the actor ids that go out in activities,
//! so they must stay exactly `{BASE_URL}/users/{nickname}`.
//!
//! Today it is the actor document, `GET /users/{nickname}` — Pleroma's
//! `ActivityPubController.user/2`
//! (`lib/pleroma/web/activity_pub/activity_pub_controller.ex`) rendering
//! `UserView.render("user.json", …)`
//! (`lib/pleroma/web/activity_pub/views/user_view.ex`), the instance
//! actor document, `GET /internal/fetch` — Pleroma's same controller's
//! `internal_fetch/2` action, and the three inbox `POST`s of the actors
//! those documents name (`inbox/2` feeding `Federator.incoming_ap_doc`).
//! The outboxes and the collections beside them arrive in later tickets
//! and get their handlers here; `GET /users/{nickname}/inbox`, the client
//! reads of a personal inbox, belongs to the C2S ticket.
//!
//! The route negotiates content: an `Accept` header naming one of the two
//! `ActivityPub` media types gets the actor document; an `Accept` naming
//! `text/html` (and no `ActivityPub` type) is a browser and gets Pleroma's
//! redirect to the frontend profile page — 302 with `Location:`
//! `{BASE_URL}/{nickname}` (this backend is API-only and serves no HTML of
//! its own, `lib/pleroma/web/feed/user_controller.ex`'s `feed_redirect`);
//! anything else gets Pleroma's 404 `{"error":"Not found"}`. Every answer
//! carries `Vary: Accept`, since the header decides which of the three the
//! client sees.
//!
//! With `AUTHORIZED_FETCH` on, the user actor route additionally demands a
//! valid HTTP Signature (see [`fetch_allowed`]).
//! `/internal/fetch` is not gated: it is the document a peer reads to
//! learn the key it has to sign with, so gating it would deadlock the
//! first fetch from any instance that never saw this one's actor.
//!
//! Beside the documents live the inboxes those documents point at —
//! `POST /inbox` (the instance's shared inbox), `POST /users/{nickname}/inbox`
//! (a local account's personal one) and `POST /internal/fetch/inbox` (the
//! instance actor's own) — Pleroma's `ActivityPubController.inbox` action
//! feeding `Pleroma.Web.Federator.incoming_ap_doc/1`
//! (`lib/pleroma/web/federator.ex`). None of them processes anything
//! synchronously: the [`SignedActivity`] extractor authenticates the
//! delivery, the handler enqueues exactly one `federator.incoming` job —
//! deduplicated on the activity's id, so redelivery queues it once — and
//! answers `202` with an empty body. A personal inbox belongs to a live
//! local account or the delivery is the plain 404; an activity without a
//! string `id`, a string `type` and an `actor` is the ticket's 400. While
//! `FEDERATION_ENABLED` is off all three routes refuse every delivery with
//! 403 `{"error":"Federation is disabled"}`, before authenticating
//! anything.
//!
//! Pleroma runs these actions through its `ap_routes` throttle bucket
//! (`config/config.exs`, disabled by default); this server has no rate
//! limiter yet, and the ticket that adds one registers these routes under
//! that bucket name ([`RATE_LIMIT_BUCKET`]).
//!
//! [`SignedActivity`]: crate::federation::inbox_auth::SignedActivity

use axum::{
    Json, Router,
    extract::{FromRequest as _, Path, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use tracing::debug;
use uuid::Uuid;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    federation::{
        actor::actor_json,
        authorized_fetch::fetch_allowed,
        dispatch::KIND_INCOMING,
        inbox_auth::{InboxAuthError, SignedActivity},
    },
    jobs::QUEUE_FEDERATOR_INCOMING,
    models::{
        job::{NewJob, enqueue},
        user::{User, get_by_nickname},
    },
    services::instance_actor,
    state::AppState,
};

/// `Content-Type` of an actor document: `ActivityPub` is JSON-LD with its
/// own registered media type, and the charset is spelled out because
/// axum's `Json` type would send it without one.
const ACTIVITY_JSON: &str = "application/activity+json; charset=utf-8";
/// The `Vary` value of every response: the `Accept` header picks the
/// document, the redirect or the 404.
const VARY_ACCEPT: &str = "Accept";
/// The media types whose presence in `Accept` means "this is an
/// `ActivityPub` client", the pair Pleroma's `:activitypub` accepts
/// pipeline lists.
const AP_MEDIA_TYPES: [&str; 2] = ["application/activity+json", "application/ld+json"];
/// The media type whose presence in `Accept` means "this is a browser":
/// Pleroma's HTML clients are sent to the frontend, and so here.
const HTML_MEDIA_TYPE: &str = "text/html";
/// 400 body when an authenticated activity carries no string `id`, no
/// string `type` or no `actor` (see [`require_activity_fields`]).
const INVALID_ACTIVITY: &str = "Invalid activity";
/// 403 body of every inbox route while `FEDERATION_ENABLED` is off.
const FEDERATION_DISABLED: &str = "Federation is disabled";
/// Prefix of the incoming jobs' deduplication key: `incoming:{activity
/// id}`, the counterpart of the delivery queue's `deliver:…` key.
const INCOMING_UNIQUE_PREFIX: &str = "incoming";
/// Attempts an incoming-activity job gets before it is given up on: the
/// ticket's five, the queue's own column default spelled out at the call.
const INCOMING_MAX_ATTEMPTS: i32 = 5;
/// The Pleroma throttle bucket these `ActivityPub` routes belong to.
///
/// `config/config.exs`'s `:ap_routes`: ten thousand requests per ten
/// seconds, disabled by default. This server has no rate limiter yet; the
/// ticket that adds one registers the inbox routes (and their bucket name)
/// with it.
pub const RATE_LIMIT_BUCKET: &str = "ap_routes";

/// The top-level `ActivityPub` router, merged by [`crate::app`].
///
/// It serves the actor documents peers fetch and the three inbox routes
/// that accept their deliveries (`POST /inbox`, `POST /users/{nickname}/inbox`
/// and `POST /internal/fetch/inbox`). `GET /users/{nickname}/inbox` is the
/// C2S ticket's, not this router's.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/users/{nickname}", get(get_actor))
        .route("/internal/fetch", get(get_instance_actor))
        .route("/inbox", post(inbox))
        .route("/users/{nickname}/inbox", post(user_inbox))
        .route("/internal/fetch/inbox", post(inbox))
}

/// `GET /users/{nickname}` — the local actor named `nickname`, as an
/// `ActivityPub` document. No OAuth: federation fetches actors with
/// anonymous requests — or, once `AUTHORIZED_FETCH` is on, with HTTP
/// Signatures, which the gate below checks before the account is read.
///
/// `Vary: Accept` is added to whatever the handler below decides, so the
/// negotiation itself never has to remember the header.
async fn get_actor(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(nickname): Path<String>,
) -> Response {
    let response = negotiated_actor(&state, &nickname, &headers, &uri).await;
    with_vary(response)
}

/// The response the route sends: the actor document for an `ActivityPub`
/// client that asks for a live local account, Pleroma's frontend redirect
/// for an HTML client, and the errors the ticket prescribes for everyone
/// else. The `AUTHORIZED_FETCH` gate runs after the content negotiation
/// (a client that never speaks `ActivityPub` stays a 404, and an HTML
/// redirect needs no signature) and before the account is read (no
/// existence leak to unsigned callers).
async fn negotiated_actor(
    state: &AppState,
    nickname: &str,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<Response> {
    if !wants_activity_json(headers) {
        if wants_html(headers) {
            return html_redirect(&state.config, nickname);
        }
        return Err(AppError::RouteNotFound);
    }
    fetch_allowed(state, &Method::GET, uri, headers).await?;
    let Some(user) = get_by_nickname(&state.db, nickname).await? else {
        return Err(AppError::RouteNotFound);
    };
    if !user.local.get() {
        return Err(AppError::RouteNotFound);
    }
    if !user.is_active.get() {
        return Err(AppError::Gone);
    }
    Ok(actor_response(&user, &state.config))
}

/// `GET /internal/fetch` — the instance actor document, served by the
/// same builder as a user's and typed `Application` by its stored
/// `actor_type`. Provisioned lazily here, so the route answers from the
/// first request even if startup never got to it; not gated by
/// `AUTHORIZED_FETCH` (see the module docs — this is the document a peer
/// needs in order to be able to sign).
async fn get_instance_actor(State(state): State<AppState>) -> Response {
    match instance_actor::ensure_instance_actor(&state.db, &state.config).await {
        Ok(actor) => actor_response(&actor, &state.config),
        Err(err) => err.into_response(),
    }
}

/// Either shared inbox — the instance's `POST /inbox` and the instance
/// actor's `POST /internal/fetch/inbox`, the two paths Pleroma's `inbox`
/// action answers on besides the personal one. A delivery accepted here
/// is queued with a null `inbox_owner`: no local account claims it, the
/// instance does.
async fn inbox(State(state): State<AppState>, request: Request) -> Response {
    inbox_answer(&state, None, request)
        .await
        .unwrap_or_else(InboxAuthError::into_response)
}

/// `POST /users/{nickname}/inbox` — the personal inbox of the local account
/// `nickname`: like the shared ones, but the named account must be a live
/// local user or the delivery is the plain 404 (step 1 of the ticket), and
/// the queued job records it as the activity's `inbox_owner`.
async fn user_inbox(
    State(state): State<AppState>,
    Path(nickname): Path<String>,
    request: Request,
) -> Response {
    inbox_answer(&state, Some(&nickname), request)
        .await
        .unwrap_or_else(InboxAuthError::into_response)
}

/// Everything an accepted `ActivityPub` delivery meets, in the order the
/// ticket fixes it: the `FEDERATION_ENABLED` gate first — a non-federating
/// instance refuses the door without authenticating, fetching or queueing
/// anything — then the [`SignedActivity`] extractor's authentication (whose
/// 400/401/403/413/415 answers, and the accepted-and-dropped `Delete` of a
/// gone actor, live in [`crate::federation::inbox_auth`]), then the owner
/// check of a personal inbox (`Some(nickname)`), then the activity's
/// sanity, then exactly one [`KIND_INCOMING`] job and the 202. Nothing is
/// ever processed here: the worker runs it.
async fn inbox_answer(
    state: &AppState,
    owner_nickname: Option<&str>,
    request: Request,
) -> Result<Response, InboxAuthError> {
    if !state.config.federation_enabled.get() {
        debug!("inbox delivery refused: federation is disabled");
        return Err(AppError::Forbidden(FEDERATION_DISABLED.to_owned()).into());
    }
    let activity = SignedActivity::from_request(request, state).await?;
    let owner_id = inbox_owner_id(state, owner_nickname).await?;
    let activity_id = require_activity_fields(&activity.json)?;
    enqueue_incoming(state, &activity, owner_id, activity_id).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// The `users.id` to record as the queued job's `inbox_owner`: the live
/// local account behind a personal inbox route, or null for the shared
/// inboxes, which no account owns. A nickname that is unknown, a remote
/// mirror's, or a deactivated account's is the plain 404
/// `{"error":"Not found"}` — the route keeps whether anyone by that name
/// used to exist to itself.
async fn inbox_owner_id(state: &AppState, nickname: Option<&str>) -> AppResult<Option<Uuid>> {
    let Some(name) = nickname else {
        return Ok(None);
    };
    let owner = get_by_nickname(&state.db, name)
        .await?
        .filter(|user| user.local.get() && user.is_active.get())
        .ok_or(AppError::RouteNotFound)?;
    Ok(Some(owner.id))
}

/// The quick sanity every authenticated activity must pass: a string
/// `id`, a string `type` and an `actor`, or the ticket's 400. The
/// [`SignedActivity`] extractor has already made `actor` a fact (it must
/// name the signer), and the one id-less document the fediverse accepts —
/// Mastodon's `Delete` of an actor this instance never saw — was answered
/// `202` and dropped inside the extractor, so anything failing here
/// describes no activity the dispatcher could fold. Returns the `id` for
/// the deduplication key.
fn require_activity_fields(json: &Value) -> AppResult<&str> {
    let invalid = || AppError::BadRequest(INVALID_ACTIVITY.to_owned());
    let id = json.get("id").and_then(Value::as_str).ok_or_else(invalid)?;
    if !json.get("type").is_some_and(Value::is_string) {
        return Err(invalid());
    }
    if json.get("actor").is_none() {
        return Err(invalid());
    }
    Ok(id)
}

/// Queues the authenticated `activity` for processing: one
/// [`KIND_INCOMING`] job on [`QUEUE_FEDERATOR_INCOMING`] with the
/// [`INCOMING_MAX_ATTEMPTS`] budget, carrying the activity's JSON, the
/// signer's id and the local owner's id (null for the shared inboxes),
/// deduplicated on `incoming:{activity id}` so a redelivered activity is
/// processed once. A duplicate of a still-live job is swallowed by the
/// queue and changes the answer not at all — both deliveries are
/// accepted, one job exists.
async fn enqueue_incoming(
    state: &AppState,
    activity: &SignedActivity,
    owner_id: Option<Uuid>,
    activity_id: &str,
) -> AppResult<()> {
    let unique_key = format!("{INCOMING_UNIQUE_PREFIX}:{activity_id}");
    let queued = enqueue(
        &state.db,
        NewJob {
            queue: QUEUE_FEDERATOR_INCOMING,
            kind: KIND_INCOMING,
            args: json!({
                "activity": activity.json,
                "signer_id": activity.signer.id,
                "inbox_owner": owner_id,
            }),
            run_at: None,
            max_attempts: Some(INCOMING_MAX_ATTEMPTS),
            unique_key: Some(&unique_key),
        },
    )
    .await?;
    if let Some(job) = queued {
        debug!(job_id = %job.id, activity_id, "queued incoming activity");
    } else {
        debug!(activity_id, "incoming activity already queued");
    }
    Ok(())
}

/// Whether `headers` name an `ActivityPub` media type in `Accept`. An
/// absent header is not one: a client that states nothing gets the
/// non-`ActivityPub` answer. The comparison is case-insensitive, as the
/// media type of a header value is.
fn wants_activity_json(headers: &HeaderMap) -> bool {
    let Some(accept) = accept_text(headers) else {
        return false;
    };
    let folded = accept.to_ascii_lowercase();
    AP_MEDIA_TYPES
        .iter()
        .copied()
        .any(|media_type| folded.contains(media_type))
}

/// Whether `headers` name `text/html` in `Accept`: a browser, which the
/// frontend — not this API — serves. Case-insensitive, and an absent
/// header is no browser.
fn wants_html(headers: &HeaderMap) -> bool {
    accept_text(headers).is_some_and(|accept| accept.to_ascii_lowercase().contains(HTML_MEDIA_TYPE))
}

/// The readable `Accept` header value: `None` when the header is absent
/// or holds bytes that are not text.
fn accept_text(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
}

/// The 302 an HTML client gets instead of a document: Pleroma sends
/// browsers to the frontend profile page, `{BASE_URL}/{nickname}` (this
/// instance serves no HTML of its own, decision 1), and lets the frontend
/// decide whether that page exists. A nickname whose text cannot ride a
/// `Location` header names no page either, and answers the 404.
fn html_redirect(config: &Config, nickname: &str) -> AppResult<Response> {
    let location = format!("{}/{}", config.base_url, nickname);
    let value = HeaderValue::from_str(&location)
        .ok()
        .ok_or(AppError::RouteNotFound)?;
    Ok((StatusCode::FOUND, [(header::LOCATION, value)]).into_response())
}

/// The actor document for `user` as an `ActivityPub` response.
fn actor_response(user: &User, config: &Config) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(ACTIVITY_JSON),
        )],
        Json(actor_json(user, config)),
    )
        .into_response()
}

/// The handler's outcome as a response, with the `Vary: Accept` every
/// answer of this route carries: the same URL means a document or a 404
/// depending on the request header, so the errors the negotiation produced
/// get the header too.
fn with_vary(outcome: AppResult<Response>) -> Response {
    let mut response = outcome.unwrap_or_else(AppError::into_response);
    let previous = response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static(VARY_ACCEPT));
    drop(previous);
    response
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    /// A [`HeaderMap`] holding only `Accept: <accept>` (`None` sends a map
    /// with no `Accept` at all).
    fn headers_with_accept(accept: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(accept) = accept {
            let value = HeaderValue::from_str(accept).unwrap();
            let previous = headers.insert(header::ACCEPT, value);
            drop(previous);
        }
        headers
    }

    #[test]
    fn the_activitypub_media_types_are_recognised() {
        for accept in [
            "application/activity+json",
            "application/ld+json",
            "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"",
            "application/json, application/activity+json",
            "text/html, application/ld+json;q=0.9, */*;q=0.8",
            "APPLICATION/Activity+JSON",
            " application/activity+json ",
        ] {
            let headers = headers_with_accept(Some(accept));
            assert!(
                wants_activity_json(&headers),
                "{accept} names an ActivityPub media type"
            );
        }
    }

    #[test]
    fn any_other_accept_is_not_an_activitypub_client() {
        for accept in [
            "*/*",
            "text/html",
            "application/json",
            "application/activity+xml",
            "application/jsonld",
            "text/html;q=0.9",
        ] {
            let headers = headers_with_accept(Some(accept));
            assert!(
                !wants_activity_json(&headers),
                "{accept} asks for something else"
            );
        }
    }

    #[test]
    fn an_absent_or_unreadable_accept_is_not_an_activitypub_client() {
        let absent = headers_with_accept(None);
        assert!(
            !wants_activity_json(&absent),
            "a request that states nothing gets the 404"
        );

        // A header value with bytes that are not visible ASCII is legal on
        // the wire but cannot be read as text; that is not an ActivityPub
        // request either.
        let mut unreadable = HeaderMap::new();
        let previous = unreadable.insert(
            header::ACCEPT,
            HeaderValue::from_bytes(b"application/activity+json\xff").unwrap(),
        );
        drop(previous);
        assert!(
            !wants_activity_json(&unreadable),
            "an unreadable Accept is not ActivityPub"
        );
    }

    #[test]
    fn every_answer_varies_on_accept() {
        let response = with_vary(Err(AppError::RouteNotFound));
        assert_eq!(
            response.headers().get(header::VARY),
            Some(&HeaderValue::from_static(VARY_ACCEPT)),
            "the header the route negotiates on is named in Vary"
        );
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "adding the header keeps the error's status"
        );
    }

    #[test]
    fn html_accepts_are_browsers_and_everything_else_is_not() {
        for accept in [
            "text/html",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            "TEXT/HTML;q=0.9",
            "application/json, text/html",
        ] {
            let headers = headers_with_accept(Some(accept));
            assert!(wants_html(&headers), "{accept} names text/html");
        }
        for accept in [
            "*/*",
            "application/json",
            "text/plain",
            "application/xhtml+xml",
            "application/activity+json",
        ] {
            let headers = headers_with_accept(Some(accept));
            assert!(!wants_html(&headers), "{accept} is not a browser");
        }
        assert!(
            !wants_html(&headers_with_accept(None)),
            "a request that states nothing is not a browser"
        );
    }

    #[test]
    fn browsers_are_redirected_to_the_frontend_profile_url() {
        let config = Config::for_tests();
        let response = html_redirect(&config, "alice").expect("a plain nickname redirects");
        assert_eq!(
            response.status(),
            StatusCode::FOUND,
            "the redirect is a 302"
        );
        assert_eq!(
            response.headers().get(header::LOCATION),
            Some(&HeaderValue::from_static("https://test.example/alice")),
            "the Location is the frontend profile URL built from the base URL"
        );
    }

    #[test]
    fn a_nickname_no_header_can_carry_is_the_plain_404() {
        let config = Config::for_tests();
        let err = html_redirect(&config, "bad\nnickname")
            .expect_err("no Location header can carry a newline");
        assert_eq!(
            err.to_string(),
            "Not found",
            "and then the route answers like any other unrouted request"
        );
    }

    /// An activity must carry a string `id`, a string `type` and an
    /// `actor`; anything else is the ticket's 400, and the id that passes
    /// is the one returned for the deduplication key.
    #[test]
    fn an_activity_needs_a_string_id_a_string_type_and_an_actor() {
        let good = json!({
            "id": "https://peer.example/activities/1",
            "type": "Follow",
            "actor": "https://peer.example/users/alice",
        });
        let id = require_activity_fields(&good).expect("a complete activity passes");
        assert_eq!(
            id, "https://peer.example/activities/1",
            "the route's answer is the activity's own id"
        );

        for missing in ["id", "type", "actor"] {
            let mut broken = good.clone();
            let _removed = broken
                .as_object_mut()
                .expect("an activity is a JSON object")
                .remove(missing);
            let err = require_activity_fields(&broken)
                .expect_err("a missing field makes the activity invalid");
            assert_eq!(
                err.to_string(),
                INVALID_ACTIVITY,
                "without {missing} the delivery is invalid"
            );
        }

        let numeric_id = json!({"id": 7, "type": "Follow", "actor": "a"});
        assert_eq!(
            require_activity_fields(&numeric_id)
                .expect_err("a non-string id is no id")
                .to_string(),
            INVALID_ACTIVITY,
            "numbers do not name activities"
        );
        let array_type = json!({"id": "x", "type": ["Follow"], "actor": "a"});
        assert_eq!(
            require_activity_fields(&array_type)
                .expect_err("an array type is no string type")
                .to_string(),
            INVALID_ACTIVITY,
            "the ticket asks for a string type"
        );
        let object_actor = json!({"id": "x", "type": "Follow", "actor": {"id": "a"}});
        assert!(
            require_activity_fields(&object_actor).is_ok(),
            "an embedded actor object still names an actor"
        );
    }

    #[test]
    fn the_inbox_routes_belong_to_the_ap_routes_bucket() {
        assert_eq!(
            RATE_LIMIT_BUCKET, "ap_routes",
            "Pleroma's bucket name, off by default there and waiting for \
             this server's first rate limiter"
        );
    }
}
