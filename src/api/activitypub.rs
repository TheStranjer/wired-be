//! `ActivityPub` routes: the URLs other instances fetch actors, objects
//! and activities at (`docs/ARCHITECTURE.md` decision 3).
//!
//! This router is merged at the top level of [`crate::app`], not under
//! `/api`: the URLs it serves are the actor ids, object ids and activity
//! ids that go out in documents, so they must stay exactly
//! `{BASE_URL}/users/{nickname}`, `{BASE_URL}/objects/{uuid}` and
//! `{BASE_URL}/activities/{uuid}`.
//!
//! Today it is the actor document, `GET /users/{nickname}` — Pleroma's
//! `ActivityPubController.user/2`
//! (`lib/pleroma/web/activity_pub/activity_pub_controller.ex`) rendering
//! `UserView.render("user.json", …)`
//! (`lib/pleroma/web/activity_pub/views/user_view.ex`), the object reads —
//! `GET /objects/{uuid}` and its nickname-scoped twin
//! `GET /users/{nickname}/statuses/{uuid}`, one shared core
//! (`object_core`) behind Pleroma's `OStatusController.object/2` and
//! `ActivityPubController.object/2` rendering
//! `ObjectView.render("object.json", …)`, and `GET /notice/{id}`, the same
//! core for an `ActivityPub` request and Pleroma's frontend redirect for a
//! browser, keyed on the status id the model writes into
//! [`Status::url`](crate::models::status::Status::url) —, the activity
//! reads — `GET /activities/{uuid}` (Pleroma's
//! `ActivityPubController.activity/2`) and
//! `GET /users/{nickname}/statuses/{uuid}/activity` (its
//! `notice_activity`) —, the instance
//! actor document, `GET /internal/fetch` — Pleroma's same controller's
//! `internal_fetch/2` action, the JSON-LD schema document,
//! `GET /schemas/litepub-0.1.jsonld` — Pleroma's static
//! `priv/static/schemas/litepub-0.1.jsonld`, which the [`crate::federation::note`]
//! documents name in their `@context` — the five activity-collection
//! documents the actor names, `GET /users/{nickname}/outbox` (and
//! `/followers`, `/following`, `/collections/featured`) and
//! `GET /objects/{uuid}/replies`, built by [`crate::federation::collections`]
//! from the same controller's `outbox/2`, `followers/2`, `following/2`,
//! `pinned/2` and `object_replies/2` actions — and the three inbox
//! `POST`s of the actors those documents name (`inbox/2` feeding
//! `Federator.incoming_ap_doc`). `GET /users/{nickname}/inbox`, the
//! client reads of a personal inbox, belongs to the C2S ticket.
//!
//! Every `GET` of this router negotiates content: an `Accept` header
//! naming one of the two `ActivityPub` media types gets the document; an
//! `Accept` naming `text/html` (and no `ActivityPub` type) is a browser
//! and gets Pleroma's redirect to the frontend page of the same address —
//! 302 with `Location:` `{BASE_URL}/{nickname}` for an actor path,
//! `{BASE_URL}/notice/{uuid}` for an object's (this backend is API-only
//! and serves no HTML of its own,
//! `lib/pleroma/web/feed/user_controller.ex`'s `feed_redirect`); anything
//! else gets Pleroma's 404 `{"error":"Not found"}`. Every answer carries
//! `Vary: Accept`, since the header decides which of the three the client
//! sees.
//!
//! The object reads negotiate differently: an object is a document, so a
//! `GET /objects/{uuid}` whose `Accept` names no `ActivityPub` media type
//! is a plain 404 even from a browser — unlike the actor route it never
//! redirects (see `object_core`, step 2). Only `GET /notice/{id}` has a
//! browser arm: an `Accept` naming `text/html` (and no `ActivityPub` type)
//! gets the 302 to `{FRONTEND_BASE_URL}/notice/{id}`, the same path on the
//! frontend origin. Within the core an unparsable uuid, a status this
//! instance does not own, a reblog row (an `Announce`, served under its
//! `{BASE_URL}/activities/{uuid}` id by the `/activities` slice), and a
//! nickname that does not name the live local author are all the same 404;
//! a soft-deleted status answers 410 with its `Tombstone` document
//! ([`tombstone_json`]) instead of the `{"error":"Gone"}` [`AppError::Gone`]
//! would render; a `local`-visibility status is 404 always, a `private` or
//! `direct` one is served only to a request whose HTTP Signature
//! [`fetch_signer`] proves and [`status_visibility::can_view`] admits, and
//! everyone else — anonymous, unaddressed signer, blocked viewer — gets
//! the same 404. What remains is served as [`note_json`]'s `Note` or
//! `Question`.
//!
//! The activity reads serve the documents whose ids decision 6 mints as
//! `{BASE_URL}/activities/{uuid}` — Pleroma's `activity/2` and
//! `notice_activity` actions. `GET /activities/{uuid}` runs
//! [`find_local_by_id`](crate::models::activity::find_local_by_id), which
//! keys on the activity row's own `UUIDv7` and answers everything the
//! builders need, and renders [`like_json`], [`emoji_react_json`],
//! [`follow_json`] or [`block_json`] for a relation row, or the status'
//! [`note_json`] wrapped in [`create_activity_json`] /
//! [`announce_activity_json`] for a status row. Both routes negotiate like
//! the object reads — an `Accept` naming no `ActivityPub` media type is the
//! plain 404, no HTML branch on either. A `Create`/`Announce` follows its
//! status' visibility exactly as the object core does (`local` is 404,
//! `public`/`unlisted` open, `private`/`direct` only to an addressed
//! signer); a soft-deleted status resolves to no arm at all, so it is 404
//! here, never a `Tombstone`. A `Like`, `EmojiReact`, `Follow` or `Block`
//! is served only to a signed fetch [`fetch_signer`] proves whose
//! `users.id` is either the activity's actor or its target; everyone else —
//! unsigned, unrelated — gets the 404.
//! `GET /users/{nickname}/statuses/{uuid}/activity` serves the `Create` of
//! that live, non-reblog, local status: the object core's checks run first
//! (the nickname must name the author), and unknown id, remote row, reblog
//! row, tombstone, `local`, a mismatched nickname and an unsigned private
//! read are all the same 404 JSON.
//!
//! With `AUTHORIZED_FETCH` on, every `ActivityPub` document route — the
//! actor, the collections, the object replies, the object reads and the
//! activity reads — additionally demands a valid HTTP Signature (see
//! [`fetch_allowed`]).
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
        activities::{EmojiReactContext, block_json, emoji_react_json, follow_json, like_json},
        actor::actor_json,
        authorized_fetch::{fetch_allowed, fetch_signer},
        collections::{self, FollowSide},
        dispatch::KIND_INCOMING,
        inbox_auth::{InboxAuthError, SignedActivity},
        note::{
            LITEPUB_SCHEMA_PATH, announce_activity_json, create_activity_json,
            litepub_context_json, load_note_context, note_json, tombstone_json,
        },
    },
    jobs::QUEUE_FEDERATOR_INCOMING,
    models::{
        activity::{self, LocalActivity, LocalActivityKind},
        job::{NewJob, enqueue},
        status::{self, Status, Visibility},
        status_visibility,
        user::{User, get_by_id, get_by_nickname},
    },
    services::instance_actor,
    state::AppState,
    util::id::parse_id,
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
/// It serves the actor documents peers fetch, the object reads
/// (`get_object`, `get_scoped_object` and `get_notice`), the activity
/// reads (`get_activity` and `get_notice_activity`), the activity
/// collections beside the actor (the account's outbox, followers,
/// following and featured list, an object's replies), and the three
/// inbox routes that accept their deliveries (`POST /inbox`,
/// `POST /users/{nickname}/inbox` and `POST /internal/fetch/inbox`).
/// `GET /users/{nickname}/inbox` is the C2S ticket's, not this router's.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/users/{nickname}", get(get_actor))
        .route("/objects/{uuid}", get(get_object))
        .route("/users/{nickname}/statuses/{uuid}", get(get_scoped_object))
        .route("/notice/{id}", get(get_notice))
        .route("/activities/{uuid}", get(get_activity))
        .route(
            "/users/{nickname}/statuses/{uuid}/activity",
            get(get_notice_activity),
        )
        .route("/users/{nickname}/outbox", get(get_outbox))
        .route("/users/{nickname}/followers", get(get_followers))
        .route("/users/{nickname}/following", get(get_following))
        .route("/users/{nickname}/collections/featured", get(get_featured))
        .route("/objects/{uuid}/replies", get(get_replies))
        .route("/internal/fetch", get(get_instance_actor))
        .route(LITEPUB_SCHEMA_PATH, get(litepub_schema))
        .route("/inbox", post(inbox))
        .route("/users/{nickname}/inbox", post(user_inbox))
        .route("/internal/fetch/inbox", post(inbox))
}

/// What the negotiation and the account lookup settled on for a
/// user-keyed `ActivityPub` `GET`.
enum Reader {
    /// The named account, live and local: the route may serve its
    /// documents to this `ActivityPub` client. Boxed because the row
    /// outgrows the redirect by an order of magnitude.
    Account(Box<User>),
    /// The negotiation already answered the request, with the browser's
    /// redirect to the frontend.
    Redirect(Response),
}

/// The shared front half of every `ActivityPub` `GET` under
/// `/users/{nickname}` — the actor document and the four collections
/// beside it: content negotiation (a browser leaves with the frontend
/// redirect, anyone else with the 404), the `AUTHORIZED_FETCH` gate
/// (after the negotiation, before the account is read, so a client that
/// never speaks `ActivityPub` stays a 404 and no existence leaks to an
/// unsigned caller), and the account checks: unknown or remote is the
/// 404, deactivated the 410.
async fn gate_actor_read(
    state: &AppState,
    nickname: &str,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<Reader> {
    if !wants_activity_json(headers) {
        if wants_html(headers) {
            let redirect = html_redirect(&state.config, nickname)?;
            return Ok(Reader::Redirect(redirect));
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
    Ok(Reader::Account(Box::new(user)))
}

/// `GET /users/{nickname}` — the local actor named `nickname`, as an
/// `ActivityPub` document. No OAuth: federation fetches actors with
/// anonymous requests — or, once `AUTHORIZED_FETCH` is on, with HTTP
/// Signatures, which the gate above checks before the account is read.
///
/// `Vary: Accept` is added to whatever the route sends, so the
/// negotiation itself never has to remember the header.
async fn get_actor(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(nickname): Path<String>,
) -> Response {
    let outcome = gate_actor_read(&state, &nickname, &headers, &uri).await;
    let response = match outcome {
        Ok(Reader::Account(user)) => Ok(actor_response(&user, &state.config)),
        Ok(Reader::Redirect(redirect)) => Ok(redirect),
        Err(err) => Err(err),
    };
    with_vary(response)
}

/// `GET /users/{nickname}/outbox` — the account's public activity
/// stream: the collection root, or with `?page=true` a newest-first keyset
/// page (up to 20 per page, cursor `max_id`) of the `Create` and `Announce`
/// activities the account's publicly visible statuses federate as.
async fn get_outbox(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(nickname): Path<String>,
) -> Response {
    let outcome = gate_actor_read(&state, &nickname, &headers, &uri).await;
    let response = match outcome {
        Ok(Reader::Account(user)) => document(
            collections::outbox_document(&state.db, &state.config, &user, uri.query()).await,
        ),
        Ok(Reader::Redirect(redirect)) => Ok(redirect),
        Err(err) => Err(err),
    };
    with_vary(response)
}

/// `GET /users/{nickname}/followers` — who follows the account: the
/// collection root, or with `?page=true` a newest-first keyset page (up to
/// 10 per page, cursor `max_id` on the follow row id) of follower actor
/// ids. The page lists nothing while `hide_followers` is set; the root
/// carries `totalItems` only while `hide_followers_count` is not.
async fn get_followers(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(nickname): Path<String>,
) -> Response {
    follow_side_document(&state, &nickname, &headers, &uri, FollowSide::Followers).await
}

/// `GET /users/{nickname}/following` — whom the account follows; the same
/// route as [`get_followers`]'s, on the other side of the relationship
/// (and the `hide_follows` / `hide_follows_count` flags).
async fn get_following(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(nickname): Path<String>,
) -> Response {
    follow_side_document(&state, &nickname, &headers, &uri, FollowSide::Following).await
}

/// The followers/following pair's shared body: the actor route's gate,
/// then that side's document.
async fn follow_side_document(
    state: &AppState,
    nickname: &str,
    headers: &HeaderMap,
    uri: &Uri,
    side: FollowSide,
) -> Response {
    let outcome = gate_actor_read(state, nickname, headers, uri).await;
    let response = match outcome {
        Ok(Reader::Account(user)) => document(
            collections::follows_document(&state.db, &state.config, &user, side, uri.query()).await,
        ),
        Ok(Reader::Redirect(redirect)) => Ok(redirect),
        Err(err) => Err(err),
    };
    with_vary(response)
}

/// `GET /users/{nickname}/collections/featured` — the account's pinned
/// posts as one `OrderedCollection` whose `orderedItems` are the full
/// `Note` documents, the counterpart of Pleroma's `pinned` action; the
/// list is inline, not paged.
async fn get_featured(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(nickname): Path<String>,
) -> Response {
    let outcome = gate_actor_read(&state, &nickname, &headers, &uri).await;
    let response = match outcome {
        Ok(Reader::Account(user)) => {
            document(collections::featured_document(&state.db, &state.config, &user).await)
        }
        Ok(Reader::Redirect(redirect)) => Ok(redirect),
        Err(err) => Err(err),
    };
    with_vary(response)
}

/// `GET /objects/{uuid}/replies` — the replies collection of one local
/// status: the `Collection` root with the count of publicly visible
/// answers, or with `?page=true` a `CollectionPage` of their object ids,
/// oldest first, up to 20 per page. An unknown, tombstoned, non-local or
/// not-publicly-visible object answers the same 404 as an unknown actor.
async fn get_replies(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(raw_id): Path<String>,
) -> Response {
    let response = object_replies_answer(&state, &raw_id, &headers, &uri).await;
    with_vary(response)
}

/// The replies route's answer: the same three-way negotiation as the
/// actor route (a browser leaves for the frontend post page, which shows
/// the thread), then the gate, the object, and its document.
async fn object_replies_answer(
    state: &AppState,
    raw_id: &str,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<Response> {
    if !wants_activity_json(headers) {
        if wants_html(headers) {
            return frontend_redirect(&state.config.base_url, &format!("notice/{raw_id}"));
        }
        return Err(AppError::RouteNotFound);
    }
    fetch_allowed(state, &Method::GET, uri, headers).await?;
    let object = publicly_replyable(state, raw_id).await?;
    let built = collections::replies_document(&state.db, &state.config, &object, uri.query()).await;
    document(built)
}

/// The object behind `GET /objects/{uuid}/replies`: the id must parse,
/// the row must be a live status this instance authored, and it must be
/// visible to the anonymous reader (Pleroma's `visible_for_user?` gate of
/// `object_replies`, which a `private`, `direct` or `local` post fails) —
/// every miss the one 404.
async fn publicly_replyable(state: &AppState, raw_id: &str) -> AppResult<Status> {
    let missing = || AppError::RouteNotFound;
    let id = parse_id(raw_id).ok_or_else(missing)?;
    let object = status::get_by_id(&state.db, id)
        .await?
        .filter(|row| row.local)
        .ok_or_else(missing)?;
    if status_visibility::can_view(&state.db, &object, None).await? {
        Ok(object)
    } else {
        Err(missing())
    }
}

/// `GET /objects/{uuid}` — the status document with the `ActivityPub`
/// object id `{BASE_URL}/objects/{uuid}` (decision 6), whatever `Accept`
/// names an `ActivityPub` media type; anything else is the plain 404. No
/// OAuth: peers dereference objects anonymously — or, once
/// `AUTHORIZED_FETCH` is on, with HTTP Signatures.
async fn get_object(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(uuid): Path<String>,
) -> Response {
    let outcome = object_core(&state, &uuid, None, &headers, &uri).await;
    with_vary(outcome)
}

/// `GET /users/{nickname}/statuses/{uuid}` — the same document as
/// [`get_object`], reached through the nickname-scoped path Pleroma
/// answers with the shared `object/2` action: the named account must
/// actually be the status' live local author, or the read is the plain 404.
async fn get_scoped_object(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path((nickname, uuid)): Path<(String, String)>,
) -> Response {
    let outcome = object_core(&state, &uuid, Some(&nickname), &headers, &uri).await;
    with_vary(outcome)
}

/// `GET /notice/{id}` — the human-facing post URL whose `{id}` is the
/// status uuid the model writes into [`Status::url`]. An `Accept` naming
/// an `ActivityPub` media type is a federating client and gets the very
/// document [`get_object`] serves; an `Accept` naming `text/html` (and no
/// `ActivityPub` type) is a browser and gets the 302 to the same path on
/// the frontend origin (see [`notice_redirect`]); anything else, the 404.
async fn get_notice(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(id): Path<String>,
) -> Response {
    let outcome = if wants_activity_json(&headers) {
        object_core(&state, &id, None, &headers, &uri).await
    } else if wants_html(&headers) {
        notice_redirect(&state.config, &id)
    } else {
        Err(AppError::RouteNotFound)
    };
    with_vary(outcome)
}

/// The shared object-read core of the three routes above, the steps in
/// the order the ticket fixes them: an unparsable uuid is the 404 (the
/// route cannot name a document it cannot address); only `ActivityPub`
/// clients get documents here — unlike the actor route, no browser is
/// redirected; then the `AUTHORIZED_FETCH` gate, in the same position
/// [`gate_actor_read`] runs it, so an unsigned read under the gate is the
/// 401 before any status is read; then the row, which must be a live
/// original this instance owns (no remote mirror, no reblog row — an
/// `Announce` is served by its `/activities/{uuid}` id); then, for the
/// nickname-scoped twin, the author check; a soft-deleted row answers 410
/// with its `Tombstone` document; `local` visibility is never an `ActivityPub`
/// document, and `private`/`direct` are served only to a signature the
/// gate proves *and* [`status_visibility::can_view`] admits; and the
/// survivor is rendered by [`note_json`].
async fn object_core(
    state: &AppState,
    id_text: &str,
    nickname: Option<&str>,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<Response> {
    let Some(id) = Uuid::parse_str(id_text).ok() else {
        return Err(AppError::RouteNotFound);
    };
    if !wants_activity_json(headers) {
        return Err(AppError::RouteNotFound);
    }
    fetch_allowed(state, &Method::GET, uri, headers).await?;
    let Some(status) = status::get_by_id_including_deleted(&state.db, id).await? else {
        return Err(AppError::RouteNotFound);
    };
    if !status.local || status.reblog_of_id.is_some() {
        return Err(AppError::RouteNotFound);
    }
    if !nickname_owns(state, nickname, &status).await? {
        return Err(AppError::RouteNotFound);
    }
    if status.deleted_at.is_some() {
        return Ok(tombstone_response(&status));
    }
    if !read_admitted(state, &status, headers, uri).await? {
        return Err(AppError::RouteNotFound);
    }
    let context = load_note_context(&state.db, &state.config, status.id).await?;
    let Some(note) = note_json(&context) else {
        // Unreachable: `note_json` is `None` only for `local`, which the
        // check above already refused. A document this route cannot render
        // is the same 404 as one it cannot find.
        return Err(AppError::RouteNotFound);
    };
    Ok(document_response(&note))
}

/// Whether `nickname` names the status' author: the live local account it
/// names must be the one who wrote the row. `None` — the unscoped paths —
/// always owns the read; an unknown, remote, deactivated or different
/// author is a `false`, and the caller answers the plain 404 either way.
async fn nickname_owns(
    state: &AppState,
    nickname: Option<&str>,
    status: &Status,
) -> AppResult<bool> {
    let Some(nickname) = nickname else {
        return Ok(true);
    };
    let owner = get_by_nickname(&state.db, nickname).await?;
    Ok(owner.is_some_and(|owner| {
        owner.id == status.user_id && owner.local.get() && owner.is_active.get()
    }))
}

/// Whether the visibility admits this read: `public` and `unlisted` answer
/// anyone, `local` is never federated and answers nobody, and `private`
/// and `direct` are served only when an HTTP Signature proves a reader —
/// with `AUTHORIZED_FETCH` off, unsigned reads answer `Ok(None)` for that
/// reader — and [`status_visibility::can_view`] admits the one it proved.
/// A signature that fails to verify is `fetch_signer`'s 401, not a 404:
/// the requester claimed an identity and could not back it.
async fn read_admitted(
    state: &AppState,
    status: &Status,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<bool> {
    match status.visibility {
        Visibility::Local => Ok(false),
        Visibility::Public | Visibility::Unlisted => Ok(true),
        Visibility::Private | Visibility::Direct => {
            let Some(signer) = fetch_signer(state, &Method::GET, uri, headers).await? else {
                return Ok(false);
            };
            status_visibility::can_view(&state.db, status, Some(&signer)).await
        }
    }
}

/// `GET /activities/{uuid}` — the local activity whose row `UUIDv7` names
/// it (Pleroma's `ActivityPubController.activity/2`): the `Create` or
/// `Announce` of a status row, or the `Like`, `EmojiReact`, `Follow` or
/// `Block` behind a relation row, at the `{BASE_URL}/activities/{uuid}` id
/// decision 6 mints. Negotiation and gates as the object reads: `Accept`
/// must name an `ActivityPub` media type, and `AUTHORIZED_FETCH` demands
/// the signature before anything resolves.
async fn get_activity(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(uuid): Path<String>,
) -> Response {
    let outcome = activity_core(&state, &uuid, &headers, &uri).await;
    with_vary(outcome)
}

/// `GET /users/{nickname}/statuses/{uuid}/activity` — Pleroma's
/// `notice_activity`: the `Create` activity of that live, non-reblog,
/// local status, the object core's document wrapped. No HTML branch: like
/// the activity route it answers only `ActivityPub` clients.
async fn get_notice_activity(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path((nickname, uuid)): Path<(String, String)>,
) -> Response {
    let outcome = notice_activity_core(&state, &uuid, &nickname, &headers, &uri).await;
    with_vary(outcome)
}

/// The shared `GET /activities/{uuid}` core, in the order the ticket fixes
/// it: an `Accept` naming no `ActivityPub` media type is the plain 404, the
/// `AUTHORIZED_FETCH` gate runs next, an unparsable uuid names no row, and
/// an id no activity table carries resolves to nothing. What does resolve
/// is a status arm (`Create`/`Announce`), gated exactly like the object
/// core, or a relation arm (`Like`/`EmojiReact`/`Follow`/`Block`), served
/// only to the activity's two parties over a signature.
async fn activity_core(
    state: &AppState,
    id_text: &str,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<Response> {
    if !wants_activity_json(headers) {
        return Err(AppError::RouteNotFound);
    }
    fetch_allowed(state, &Method::GET, uri, headers).await?;
    let Some(id) = Uuid::parse_str(id_text).ok() else {
        return Err(AppError::RouteNotFound);
    };
    let Some(resolved) = activity::find_local_by_id(&state.db, id).await? else {
        return Err(AppError::RouteNotFound);
    };
    let document = match resolved.kind {
        LocalActivityKind::Create | LocalActivityKind::Announce => {
            status_activity_document(state, &resolved, headers, uri).await?
        }
        LocalActivityKind::Like
        | LocalActivityKind::EmojiReact
        | LocalActivityKind::Follow
        | LocalActivityKind::Block => {
            relation_activity_document(state, &resolved, headers, uri).await?
        }
    };
    Ok(document_response(&document))
}

/// The `Create` or `Announce` behind a resolved status arm. The row goes
/// through the object core's checks on the re-read the gate and the
/// builders work from: a row this instance does not own, a deletion that
/// raced in since the arm resolved (a soft-deleted status never resolves
/// to an arm, and this route answers its id 404, never a `Tombstone`), and
/// a visibility [`read_admitted`] refuses are all the plain 404.
async fn status_activity_document(
    state: &AppState,
    resolved: &LocalActivity,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<Value> {
    let Some(stored) = status::get_by_id_including_deleted(&state.db, resolved.id).await? else {
        return Err(AppError::RouteNotFound);
    };
    if !stored.local || stored.deleted_at.is_some() {
        return Err(AppError::RouteNotFound);
    }
    if !read_admitted(state, &stored, headers, uri).await? {
        return Err(AppError::RouteNotFound);
    }
    if stored.reblog_of_id.is_some() {
        announce_document(state, resolved, &stored).await
    } else {
        create_document(state, &stored).await
    }
}

/// The status' `Create`: the very [`note_json`] document [`object_core`]
/// serves, wrapped by [`create_activity_json`]. Its `id` is
/// `{BASE_URL}/activities/{status id}` (decision 6), exactly
/// [`activity_id`](crate::federation::activities::activity_id) of the row
/// that resolved. [`note_json`]'s `None` is a `local` status, which
/// [`read_admitted`] has just refused, so the unrenderable case is the
/// same 404 as on the object read.
async fn create_document(state: &AppState, stored: &Status) -> AppResult<Value> {
    let context = load_note_context(&state.db, &state.config, stored.id).await?;
    let Some(note) = note_json(&context) else {
        return Err(AppError::RouteNotFound);
    };
    Ok(create_activity_json(&note, stored, &state.config))
}

/// The reblog's `Announce`: the row in hand and its author, naming the
/// boosted original's `uri` and its author's `ap_id` — both carried by the
/// resolved arm, so no second read of the original. The builder's `None`
/// is a `local` reblog, which [`read_admitted`] has just refused.
async fn announce_document(
    state: &AppState,
    resolved: &LocalActivity,
    stored: &Status,
) -> AppResult<Value> {
    let Some(author) = get_by_id(&state.db, stored.user_id).await? else {
        return Err(AppError::RouteNotFound);
    };
    let Some(original_author_ap_id) = resolved.target_ap_id.as_deref() else {
        return Err(AppError::RouteNotFound);
    };
    announce_activity_json(
        stored,
        &author,
        &resolved.object_ap_id,
        original_author_ap_id,
        &state.config,
    )
    .ok_or(AppError::RouteNotFound)
}

/// The `Like`, `EmojiReact`, `Follow` or `Block` behind a resolved
/// relation arm, served only to a signed fetch whose reader is one of the
/// activity's two parties: [`fetch_signer`]'s `None` — the anonymous read
/// with `AUTHORIZED_FETCH` off — is a 404 like any unrelated reader's, and
/// a signature that fails to verify is its 401.
async fn relation_activity_document(
    state: &AppState,
    resolved: &LocalActivity,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<Value> {
    let Some(signer) = fetch_signer(state, &Method::GET, uri, headers).await? else {
        return Err(AppError::RouteNotFound);
    };
    let party = signer.id == resolved.actor_user_id
        || resolved
            .target_user_id
            .is_some_and(|target| target == signer.id);
    if !party {
        return Err(AppError::RouteNotFound);
    }
    relation_json(state, resolved)
}

/// The relation document rendered straight from the resolved row's
/// columns — the ids, the object, the publication moment, and the emoji
/// pair only the `EmojiReact` arm carries — with no further read. The
/// status arms never reach here; should one, its id names no relation
/// document and the plain 404 answers.
fn relation_json(state: &AppState, resolved: &LocalActivity) -> AppResult<Value> {
    let config = &state.config;
    let id = resolved.id;
    let actor = resolved.actor_ap_id.as_str();
    let object = resolved.object_ap_id.as_str();
    let published = &resolved.inserted_at;
    match resolved.kind {
        LocalActivityKind::Like => Ok(like_json(config, id, actor, object, published)),
        LocalActivityKind::EmojiReact => {
            let Some(emoji) = resolved.emoji.as_deref() else {
                return Err(AppError::RouteNotFound);
            };
            let context = EmojiReactContext {
                config,
                id,
                actor_ap_id: actor,
                object_uri: object,
                emoji,
                emoji_url: resolved.emoji_url.as_deref(),
                published,
            };
            Ok(emoji_react_json(&context))
        }
        LocalActivityKind::Follow => Ok(follow_json(config, id, actor, object, published)),
        LocalActivityKind::Block => Ok(block_json(config, id, actor, object, published)),
        LocalActivityKind::Create | LocalActivityKind::Announce => Err(AppError::RouteNotFound),
    }
}

/// The `/users/{nickname}/statuses/{uuid}/activity` core: [`object_core`]'s
/// checks in [`object_core`]'s order — unparsable uuid, non-`ActivityPub`
/// client, `AUTHORIZED_FETCH` gate, then a row this instance does not own,
/// a reblog row, or a nickname that is not the live local author — with
/// one difference: a soft-deleted status is the 404 JSON here, not a
/// `Tombstone`. The survivor's [`note_json`] document rides out wrapped in
/// [`create_activity_json`].
async fn notice_activity_core(
    state: &AppState,
    id_text: &str,
    nickname: &str,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<Response> {
    let Some(id) = Uuid::parse_str(id_text).ok() else {
        return Err(AppError::RouteNotFound);
    };
    if !wants_activity_json(headers) {
        return Err(AppError::RouteNotFound);
    }
    fetch_allowed(state, &Method::GET, uri, headers).await?;
    let Some(stored) = status::get_by_id_including_deleted(&state.db, id).await? else {
        return Err(AppError::RouteNotFound);
    };
    if !stored.local || stored.reblog_of_id.is_some() || stored.deleted_at.is_some() {
        return Err(AppError::RouteNotFound);
    }
    if !nickname_owns(state, Some(nickname), &stored).await? {
        return Err(AppError::RouteNotFound);
    }
    if !read_admitted(state, &stored, headers, uri).await? {
        return Err(AppError::RouteNotFound);
    }
    Ok(document_response(&create_document(state, &stored).await?))
}

/// The `Tombstone` a soft-deleted status is served as: the 410 whose body
/// is [`tombstone_json`]'s document. Built by hand rather than through
/// [`AppError::Gone`], which would answer `{"error":"Gone"}` where a
/// federating reader expects the tombstone.
fn tombstone_response(status: &Status) -> Response {
    (
        StatusCode::GONE,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(ACTIVITY_JSON),
        )],
        Json(tombstone_json(status)),
    )
        .into_response()
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

/// `GET /schemas/litepub-0.1.jsonld` — the JSON-LD context document every
/// `Note`/`Question` and activity this instance serves references from its
/// `@context`, the counterpart of Pleroma's static
/// `priv/static/schemas/litepub-0.1.jsonld`. Content negotiation, the
/// `AUTHORIZED_FETCH` gate and the account read are all irrelevant here:
/// it is one fixed document, so peers can expand our vocabulary before
/// they have ever authenticated to us.
async fn litepub_schema() -> Json<Value> {
    Json(litepub_context_json())
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
    frontend_redirect(config.base_url.as_str(), nickname)
}

/// The 302 to the frontend at `{base_url}/{path}`, shared by the actor
/// route's profile redirect and the replies route's redirect to the post
/// page. Text that cannot ride a `Location` header names no page, and
/// answers the plain 404.
fn frontend_redirect(base_url: &str, path: &str) -> AppResult<Response> {
    let location = format!("{base_url}/{path}");
    let value = HeaderValue::from_str(&location)
        .ok()
        .ok_or(AppError::RouteNotFound)?;
    Ok((StatusCode::FOUND, [(header::LOCATION, value)]).into_response())
}

/// The 302 a browser gets on `GET /notice/{id}`: the same path on the
/// frontend origin, `{FRONTEND_BASE_URL}/notice/{id}` — the notice lives
/// on the frontend, and this instance serves no HTML of its own
/// (decision 1). `FRONTEND_BASE_URL` defaults to `BASE_URL`, so without a
/// separate frontend this is the redirect to this server's own path, and
/// the frontend decides whether that notice exists. An `{id}` whose text
/// cannot ride a `Location` header names no page either, and answers the
/// 404.
fn notice_redirect(config: &Config, id: &str) -> AppResult<Response> {
    let location = format!("{}/notice/{id}", config.frontend_base_url);
    let value = HeaderValue::from_str(&location)
        .ok()
        .ok_or(AppError::RouteNotFound)?;
    Ok((StatusCode::FOUND, [(header::LOCATION, value)]).into_response())
}

/// The actor document for `user` as an `ActivityPub` response.
fn actor_response(user: &User, config: &Config) -> Response {
    document_response(&actor_json(user, config))
}

/// A built document's answer, the collection routes' shared ending.
fn document(built: AppResult<Value>) -> AppResult<Response> {
    built.map(|document| document_response(&document))
}

/// Any `ActivityPub` document as a response: the JSON body under the
/// `activity+json` content type.
fn document_response(document: &Value) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(ACTIVITY_JSON),
        )],
        Json(document),
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

    #[test]
    fn browsers_on_the_notice_route_are_redirected_to_the_frontend_origin() {
        let config = Config::for_tests();
        let response = notice_redirect(&config, "01934568-1111-7000-8000-abcdef012345")
            .expect("a uuid redirects");
        assert_eq!(
            response.status(),
            StatusCode::FOUND,
            "the redirect is a 302"
        );
        assert_eq!(
            response.headers().get(header::LOCATION),
            Some(&HeaderValue::from_static(
                "https://test.example/notice/01934568-1111-7000-8000-abcdef012345"
            )),
            "the Location is the same path on the frontend origin, which the \
             test config defaults onto the base URL"
        );
    }

    #[test]
    fn a_notice_id_no_header_can_carry_is_the_plain_404() {
        let config = Config::for_tests();
        let err = notice_redirect(&config, "bad\nid")
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
