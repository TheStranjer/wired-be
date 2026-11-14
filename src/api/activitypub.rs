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
//! `Federator.incoming_ap_doc`).
//!
//! Beside the federation face sits the client-to-server one, Pleroma's
//! `ActivityPubController` actions `read_inbox`, `update_outbox`,
//! `whoami` and `upload_media`: `GET /users/{nickname}/inbox`, the
//! owner's `OrderedCollection` of the `Create` activities addressed to
//! them, `POST /users/{nickname}/outbox`, which folds an activity posted
//! by the owner into the same writes the Mastodon client API runs and
//! answers `201` with the new activity's id in `Location`,
//! `GET /api/ap/whoami`, the authenticated user's own actor document,
//! and `POST /api/ap/upload_media`, the media upload answered as an
//! `ActivityStreams` `Document`. These four are OAuth routes, not
//! federation: they authenticate with the bearer token (decision 9),
//! demand the token's user *be* the `{nickname}` of the path — every
//! other pairing is 403 `{"error":"Forbidden"}`, Pleroma's catch-all
//! arm — and they answer regardless of what `Accept` says, since no
//! `ActivityPub` peer ever calls them.
//!
//! Every federation `GET` of this router (never a C2S one) negotiates
//! content: an `Accept` header
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
    api::{
        blocks::after_block_changed,
        media::{UploadParams, store_upload},
        statuses::{CANNOT_REBLOG, action_target_id},
    },
    auth::AuthUser,
    config::Config,
    error::{AppError, AppResult},
    extract::{
        ApiBody,
        multipart::{MultipartForm, upload_body_limit},
    },
    federation::{
        activities::{EmojiReactContext, block_json, emoji_react_json, follow_json, like_json},
        actor::actor_json,
        authorized_fetch::{fetch_allowed, fetch_signer},
        collections::{self, FollowSide},
        dispatch::KIND_INCOMING,
        inbox_auth::{InboxAuthError, SignedActivity},
        json as ap_json,
        note::{
            LITEPUB_SCHEMA_PATH, announce_activity_json, create_activity_json,
            litepub_context_json, load_note_context, note_json, tombstone_json,
        },
        note_parser,
    },
    jobs::QUEUE_FEDERATOR_INCOMING,
    models::{
        activity::{self, LocalActivity, LocalActivityKind},
        block::{self, Block},
        favourite,
        follow::{self, Follow},
        job::{NewJob, enqueue},
        media_attachment, mention, oauth_app,
        status::{self, Status, Visibility},
        status_visibility,
        user::{self, User, get_by_id, get_by_nickname},
    },
    oauth::scopes::{READ, READ_STATUSES, WRITE_MEDIA, WRITE_STATUSES},
    services::{
        create_status::{self, CreateStatusParams},
        delete_status::delete_status,
        instance_actor, notify,
    },
    state::AppState,
    util::id::{new_id, parse_id},
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
/// 403 body of the two C2S actor routes when the bearer's user is not the
/// `{nickname}` of the path (the ticket's wording of Pleroma's
/// catch-all `can't read inbox of … as …` arm).
const FORBIDDEN: &str = "Forbidden";
/// 400 body of the C2S outbox for an activity whose type it does not
/// fold — and for a `Create` whose object is not a `Note`.
const UNSUPPORTED_ACTIVITY_TYPE: &str = "Unsupported activity type";
/// The `type` a posted `Create`'s object must wear to be folded: the
/// outbox takes the plain post and nothing else (a poll's `Question`, a
/// `Tombstone`, an actor — all unsupported).
const OBJECT_TYPE_NOTE: &str = "Note";
/// The collection `type` of the C2S inbox root.
const ORDERED_COLLECTION: &str = "OrderedCollection";
/// How many activities one C2S inbox page holds — the outbox page's size,
/// the ticket's twenty.
const INBOX_PAGE_SIZE: i64 = 20;

/// The top-level `ActivityPub` router, merged by [`crate::app`].
///
/// It serves the actor documents peers fetch, the object reads
/// (`get_object`, `get_scoped_object` and `get_notice`), the activity
/// reads (`get_activity` and `get_notice_activity`), the activity
/// collections beside the actor (the account's outbox, followers,
/// following and featured list, an object's replies), and the three
/// inbox routes that accept their deliveries (`POST /inbox`,
/// `POST /users/{nickname}/inbox` and `POST /internal/fetch/inbox`).
/// On top of these live the four OAuth-authenticated client-to-server
/// routes: `GET /users/{nickname}/inbox`, `POST /users/{nickname}/outbox`,
/// `GET /api/ap/whoami` and `POST /api/ap/upload_media`.
///
/// `config` is what the C2S upload route sizes its body limit from: the
/// multipart extractor refuses bodies over 2 MiB unless the route raises
/// that first (see [`upload_body_limit`], and the same on the Mastodon
/// upload routes in [`crate::api::media`]).
pub fn router(config: &Config) -> Router<AppState> {
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
        .route("/users/{nickname}/inbox", get(read_inbox).post(user_inbox))
        .route(
            "/users/{nickname}/outbox",
            get(get_outbox).post(update_outbox),
        )
        .route("/users/{nickname}/followers", get(get_followers))
        .route("/users/{nickname}/following", get(get_following))
        .route("/users/{nickname}/collections/featured", get(get_featured))
        .route("/objects/{uuid}/replies", get(get_replies))
        .route("/internal/fetch", get(get_instance_actor))
        .route(LITEPUB_SCHEMA_PATH, get(litepub_schema))
        .route("/inbox", post(inbox))
        .route("/internal/fetch/inbox", post(inbox))
        .route("/api/ap/whoami", get(whoami))
        .route(
            "/api/ap/upload_media",
            post(ap_upload_media).layer(upload_body_limit(config)),
        )
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

// ---------------------------------------------------------------------------
// Client-to-server: the owner's reads and writes at the same addresses
// (Pleroma's `read_inbox`, `update_outbox`, `whoami` and `upload_media`).
// ---------------------------------------------------------------------------

/// `GET /users/{nickname}/inbox` — the owner's personal inbox, Pleroma's
/// `read_inbox`: the collection root, or with `?page=true` a newest-first
/// page (20 per page, cursor `max_id`) of the `Create` activities of the
/// statuses addressed to the account — the ones that @-mention it
/// (`status_mentions`: the replies, the direct messages and the mentioned
/// posts all link the addressee there), rendered exactly as the outbox
/// renders its own. The gates are the ticket's: a bearer token granting
/// `read` or `read:statuses` (without one the extractor's 401 answers,
/// Pleroma's `EnsureAuthenticatedPlug` word for word), and the token's
/// user must *be* the `{nickname}` of the path — every other pairing is
/// the 403. No `Accept` negotiation and no HTTP Signature: this is the
/// client API on an actor address, not the document a peer fetches.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the scope check; [`AppError::Forbidden`] with [`FORBIDDEN`] for
/// somebody else's inbox; [`AppError::BadRequest`] from the page cursors;
/// any database failure arrives through `?` as [`AppError::Database`].
async fn read_inbox(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(nickname): Path<String>,
    uri: Uri,
) -> AppResult<Response> {
    auth.require_any(&[READ, READ_STATUSES])?;
    ensure_own_actor(&auth.0, &nickname)?;
    let collection = user::inbox(&auth.0.ap_id);
    let document = if collections::page_requested(uri.query()) {
        inbox_page(&state, auth.0.id, &collection, uri.query()).await?
    } else {
        collections::collection_root(&collection, ORDERED_COLLECTION, None, &state.config)
    };
    Ok(document_response(&document))
}

/// One `?page=true` of the personal inbox: the ids of the statuses
/// mentioning the owner, newest first off the keyset (decision 4), as
/// their `Create` activities inside the same page envelope the outbox
/// writes.
async fn inbox_page(
    state: &AppState,
    owner_id: Uuid,
    collection: &str,
    query: Option<&str>,
) -> AppResult<Value> {
    let page = collections::keyset_page(query, INBOX_PAGE_SIZE)?;
    let ids = mention::mentioned_status_ids(&state.db, owner_id, page).await?;
    let items = inbox_creates(state, &ids).await?;
    let links = collections::descending_page_links(
        collection,
        query,
        ids.first().copied(),
        ids.last().copied(),
        i64::try_from(ids.len()).unwrap_or(i64::MAX) == INBOX_PAGE_SIZE,
    );
    Ok(collections::ordered_collection_page(
        &links,
        items,
        &state.config,
    ))
}

/// The page's status ids as `Create` activities, page order kept: the
/// live rows come back through one batch read and each renders through
/// the outbox's own loader. A row that vanished between the id page and
/// its render — or that federates as nothing (`local` visibility) — drops
/// out rather than failing the page.
async fn inbox_creates(state: &AppState, ids: &[Uuid]) -> AppResult<Vec<Value>> {
    let rows = status::get_many(&state.db, ids).await?;
    let mut items = Vec::with_capacity(rows.len());
    for row in ids
        .iter()
        .filter_map(|id| rows.iter().find(|row| row.id == *id))
    {
        items.extend(create_activity_of(state, row.id).await?);
    }
    Ok(items)
}

/// One live status row as its `Create` activity, `None` when its object
/// is gone or wears no document.
async fn create_activity_of(state: &AppState, status_id: Uuid) -> AppResult<Option<Value>> {
    match load_note_context(&state.db, &state.config, status_id).await {
        Ok(ctx) => {
            Ok(note_json(&ctx)
                .map(|object| create_activity_json(&object, &ctx.status, &state.config)))
        }
        // The row fell between the id page and its render.
        Err(AppError::NotFound) => Ok(None),
        Err(err) => Err(err),
    }
}

/// `POST /users/{nickname}/outbox` — the C2S write, Pleroma's
/// `update_outbox`: one `ActivityStreams` activity by the owner, folded
/// into the same writes the Mastodon client API runs. `Create` of a
/// `Note` posts through [`create_status::create_status`]; `Delete`
/// tombstones the caller's own status; `Follow` and `Block` write the
/// relationship through the models; `Like` and `Announce` are the
/// favourite and the reblog; `Undo` withdraws a `Follow`, `Like`,
/// `Announce` or `Block`, named either by the embedded activity or by the
/// id its creation answered in `Location`. Any other type is the 400
/// [`UNSUPPORTED_ACTIVITY_TYPE`]. A folded activity is not stored as
/// such — decision 6 keeps the facts relationally and renders their
/// activities from them — so the answer is the ticket's: `201` with the
/// new object/activity id in `Location` and an empty body.
///
/// The gates are [`read_inbox`]'s, on `write:statuses` (Pleroma runs the
/// whole outbox under one scope, not per activity type).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the scope check; [`AppError::Forbidden`] with [`FORBIDDEN`] for
/// somebody else's outbox (and for a `Delete` of a post the caller does
/// not own); [`AppError::BadRequest`] with [`UNSUPPORTED_ACTIVITY_TYPE`];
/// the inner write's own refusals — the create's 422s and reply 404, the
/// self-follow and block refusals, the boost 422 — and
/// [`AppError::NotFound`] for an object the activity names that no stored
/// row carries.
async fn update_outbox(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(nickname): Path<String>,
    ApiBody(activity): ApiBody<Value>,
) -> AppResult<Response> {
    auth.require(WRITE_STATUSES)?;
    ensure_own_actor(&auth.0, &nickname)?;
    let location = post_activity(&state, &auth, &activity).await?;
    debug!(
        actor = %auth.0.id,
        %location,
        "c2s activity accepted"
    );
    let value = HeaderValue::from_str(&location)
        .map_err(|err| AppError::Internal(format!("the activity id is not header-safe: {err}")))?;
    Ok((StatusCode::CREATED, [(header::LOCATION, value)]).into_response())
}

/// The outbox's type dispatch: the seven activity types the ticket folds,
/// each answered by the `Location` id of the activity the write
/// corresponds to, and everything else — including a body that names no
/// string type at all — by the 400.
async fn post_activity(state: &AppState, auth: &AuthUser, activity: &Value) -> AppResult<String> {
    match activity
        .get("type")
        .and_then(ap_json::first_type)
        .as_deref()
    {
        Some("Create") => post_create(state, auth, activity).await,
        Some("Delete") => post_delete(state, auth, activity).await,
        Some("Follow") => post_follow(state, auth, activity).await,
        Some("Like") => post_like(state, auth, activity).await,
        Some("Announce") => post_announce(state, auth, activity).await,
        Some("Block") => post_block(state, auth, activity).await,
        Some("Undo") => post_undo(state, auth, activity).await,
        Some(_) | None => Err(AppError::BadRequest(UNSUPPORTED_ACTIVITY_TYPE.to_owned())),
    }
}

/// The C2S owner gate of the two actor-keyed routes: the bearer's user
/// must *be* the account the path names — Pleroma matches the token's
/// nickname against the route's in the controller head, and its
/// catch-all arm answers every other pairing the 403. No lookup stands
/// between the two: the extractor already handed a live local account,
/// so a matching nickname can only name that one.
fn ensure_own_actor(user: &User, nickname: &str) -> AppResult<()> {
    if user.nickname == nickname {
        Ok(())
    } else {
        Err(AppError::Forbidden(FORBIDDEN.to_owned()))
    }
}

/// `Create` of a `Note` — the post, through the client API's own write
/// service. The note's `content` is the status text, its `summary` the
/// content warning and its `sensitive` flag the fold; the audience is
/// read off `to`/`cc` exactly as the federation parser reads them
/// ([`note_parser::parse_visibility`]: the public addressee in `to` is
/// `public`, in `cc` alone `unlisted`, the followers collection
/// `private`, anything else `direct`); `inReplyTo` names the answered
/// status by object id or id text; and every `attachment` reference
/// carries an upload id from `POST /api/ap/upload_media` — the `media_ids`
/// the create claims, whose "the caller's own, not yet posted" rules are
/// the media model's. `Location` names the `Create` the post federates
/// as, `{BASE_URL}/activities/{status id}` (decision 6, and the id
/// [`create_activity_json`] writes).
async fn post_create(state: &AppState, auth: &AuthUser, activity: &Value) -> AppResult<String> {
    let object = note_object(activity)?;
    let params = CreateStatusParams {
        status: text_field(object, "content"),
        spoiler_text: text_field(object, "summary"),
        sensitive: object.get("sensitive").and_then(Value::as_bool),
        visibility: Some(
            note_parser::parse_visibility(&create_addressing(object, activity), &auth.0)
                .as_str()
                .to_owned(),
        ),
        in_reply_to_id: replied_to(state, object).await?,
        media_ids: Some(attachment_ids(object)),
        ..CreateStatusParams::default()
    };
    let app = oauth_app::get_by_id(&state.db, auth.1.app_id).await?;
    let entity = create_status::create_status(state, &auth.0, app.as_ref(), params).await?;
    Ok(format!(
        "{}/activities/{}",
        state.config.base_url, entity.id
    ))
}

/// The `Create`'s object, when it is a `Note` document: a bare reference,
/// a missing object, or any other object type is the 400
/// [`UNSUPPORTED_ACTIVITY_TYPE`] — the outbox posts a plain note and
/// nothing else.
fn note_object(activity: &Value) -> AppResult<&Value> {
    let unsupported = || AppError::BadRequest(UNSUPPORTED_ACTIVITY_TYPE.to_owned());
    activity
        .get("object")
        .filter(|object| {
            object.is_object()
                && object.get("type").and_then(ap_json::first_type).as_deref()
                    == Some(OBJECT_TYPE_NOTE)
        })
        .ok_or_else(unsupported)
}

/// The string `object` carries under `key`: `None` for an absent, `null`
/// or non-text value, which for `content` leaves the create's own empty
/// refusal and for `summary` no content warning.
fn text_field(object: &Value, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// The `to`/`cc` the create's audience is read from: the note's own when
/// it addresses anyone, the enclosing `Create`'s otherwise — C2S clients
/// write the audience on either half, and the two places Pleroma's
/// pipeline checks. The result is the shape [`note_parser::parse_visibility`]
/// reads: one object with `to` and `cc`.
fn create_addressing(object: &Value, activity: &Value) -> Value {
    let audience = |key: &str| {
        object
            .get(key)
            .filter(|value| !is_empty_audience(value))
            .or_else(|| activity.get(key))
            .cloned()
            .unwrap_or_else(|| json!([]))
    };
    json!({ "to": audience("to"), "cc": audience("cc") })
}

/// Whether an addressing value names nobody: no non-empty reference
/// anywhere in it — `null`, the empty array, the empty string, or a
/// value that carries no reference at all all mean "this half holds no
/// audience", and the create falls back to the other half.
fn is_empty_audience(value: &Value) -> bool {
    !ap_json::references(value)
        .iter()
        .any(|recipient| !recipient.is_empty())
}

/// The note's `inReplyTo` as the create's id text: the answered status
/// must be a row this instance stores (an object id or the bare id text,
/// through the same read a `Like` uses), or the shared 404. An absent or
/// `null` reference is a root post.
async fn replied_to(state: &AppState, object: &Value) -> AppResult<Option<String>> {
    let Some(reference) = object.get("inReplyTo").and_then(ap_json::value_or_id) else {
        return Ok(None);
    };
    let row = status_by_reference(state, &reference)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Some(row.id.to_string()))
}

/// The `attachment` array's upload ids, submission order kept: every
/// reference's last path segment — the media id `POST /api/ap/upload_media`
/// answers as the `Document`'s `id`, bare or under an id URL. The create
/// itself decides what each id may mean: the media model claims only the
/// poster's own, not-yet-posted uploads and refuses any other with its
/// 422.
fn attachment_ids(object: &Value) -> Vec<String> {
    object
        .get("attachment")
        .map_or(&[][..], ap_json::as_array)
        .iter()
        .filter_map(ap_json::value_or_id)
        .map(|reference| match reference.rsplit_once('/') {
            Some((_origin_and_path, last_segment)) => last_segment.to_owned(),
            None => reference,
        })
        .collect()
}

/// `Delete` of the caller's own status, named by object id or id text:
/// the same write `DELETE /api/v1/statuses/{id}` runs — the tombstone of
/// the row and its reblogs, the counter steps and the post-delete hooks
/// ([`delete_status`]). Somebody else's post is the 403, Pleroma's
/// permission arm of `fix_user_message`; an unknown or already deleted
/// one is the shared 404. `Location` names the `Delete` itself: a fresh
/// `{BASE_URL}/activities/{uuid}` id, since a withdrawal creates no row
/// to key one on (decision 6).
async fn post_delete(state: &AppState, auth: &AuthUser, activity: &Value) -> AppResult<String> {
    let reference = activity_reference(activity)?;
    let row = status_by_reference(state, &reference)
        .await?
        .ok_or(AppError::NotFound)?;
    if row.user_id != auth.0.id {
        return Err(AppError::Forbidden(FORBIDDEN.to_owned()));
    }
    // The write is idempotent: the `false` is the concurrent delete that
    // turned the row into a tombstone between the read and this write —
    // the fact already withdrawn, and the same accepted answer.
    let _restated = delete_status(state, &row).await?;
    Ok(new_activity_id(&state.config))
}

/// `Follow` of an actor: the object names the followed account by actor
/// id, resolved among the stored rows (remote mirrors included — this
/// endpoint reads what is stored and does not fetch an unseen actor over
/// the network), and the write is the model's own [`follow::follow`], the
/// very one `POST /api/v1/accounts/{id}/follow` runs: a locked target
/// leaves a `pending` request, a repeat restates the relationship, and
/// the block refusals are the model's. `Location` carries the follow's
/// activity id — the `ap_id` the row keeps (decision 6), which is what
/// an `Undo` of it names.
async fn post_follow(state: &AppState, auth: &AuthUser, activity: &Value) -> AppResult<String> {
    let reference = activity_reference(activity)?;
    let target = stored_actor(state, &reference).await?;
    let created = follow::follow(&state.db, &state.config.base_url, &auth.0, &target).await?;
    notify::on_follow(state, &auth.0, &target, Some(&created), true).await;
    Ok(row_activity_id(&state.config, created.ap_id.as_deref()))
}

/// `Like` of a status: the favourite, on the same gate and with the same
/// boost-folding as `POST /api/v1/statuses/{id}/favourite` — what the
/// caller may not see is the 404, and a reblog's id likes its original.
/// A repeat is the existing row's activity id with the same `201` (one
/// like, told once: the notification fires only on the write that made
/// the row).
async fn post_like(state: &AppState, auth: &AuthUser, activity: &Value) -> AppResult<String> {
    let row = interactable_status(state, auth, activity).await?;
    let liked_id = action_target_id(&row);
    let created =
        favourite::favourite(&state.db, &state.config.base_url, auth.0.id, liked_id).await?;
    let stored = match created {
        Some(created) => {
            notify::on_favourite(state, &created, true).await;
            Some(created)
        }
        None => favourite::get(&state.db, auth.0.id, liked_id).await?,
    };
    Ok(row_activity_id(
        &state.config,
        stored.as_ref().and_then(|found| found.ap_id.as_deref()),
    ))
}

/// `Announce` of a status: the reblog at `public` visibility, under the
/// shared gate, the boost-folding ("nobody boosts a boost") and the
/// `private`/`direct` refusal of `POST /api/v1/statuses/{id}/reblog`. A
/// reblog row *is* its `Announce` (decision 6), so `Location` is that
/// row's own `uri` — which is what an `Undo` of the boost names.
async fn post_announce(state: &AppState, auth: &AuthUser, activity: &Value) -> AppResult<String> {
    let row = interactable_status(state, auth, activity).await?;
    let original = reblog_target(state, &row).await?;
    if matches!(
        original.visibility,
        Visibility::Private | Visibility::Direct
    ) {
        return Err(AppError::Unprocessable(CANNOT_REBLOG.to_owned()));
    }
    let (reblog, created) = status::create_reblog(
        &state.db,
        &state.config.base_url,
        &auth.0,
        &original,
        Visibility::Public,
    )
    .await?;
    if created {
        notify::on_reblog(state, &reblog, true).await;
    }
    Ok(reblog.uri)
}

/// `Block` of an actor: [`block::block`], the same write
/// `POST /api/v1/accounts/{id}/block` runs — idempotent on the pair, and
/// the self-block refusal is the model's. `Location` carries the block's
/// activity id, the `ap_id` the row keeps.
async fn post_block(state: &AppState, auth: &AuthUser, activity: &Value) -> AppResult<String> {
    let reference = activity_reference(activity)?;
    let target = stored_actor(state, &reference).await?;
    let created = block::block(&state.db, &state.config.base_url, &auth.0, &target).await?;
    after_block_changed(state, &created, true).await;
    Ok(row_activity_id(&state.config, created.ap_id.as_deref()))
}

/// `Undo` of one of those: the withdrawn activity named by id alone (the
/// `Location` the creation answered — the four relationship tables and
/// the reblog rows all key on it, decision 6) or as an embedded activity
/// whose inner type and object name the fact. The withdrawals are the
/// Mastodon routes' own writes, idempotent for the same peace: an `Undo`
/// of something already withdrawn is still accepted. `Location` names the
/// `Undo` itself, a fresh activity id.
async fn post_undo(state: &AppState, auth: &AuthUser, activity: &Value) -> AppResult<String> {
    match activity.get("object") {
        Some(inner) if inner.is_object() => undo_embedded(state, auth, inner).await,
        Some(object) => {
            let reference = ap_json::value_or_id(object).ok_or(AppError::NotFound)?;
            undo_by_activity_id(state, auth, &reference).await
        }
        None => Err(AppError::NotFound),
    }
}

/// The `Undo` of an embedded activity: the inner `type` says what is
/// withdrawn and the inner `object` names the target — the four types the
/// outbox itself creates, and any other inner type is the 400.
async fn undo_embedded(state: &AppState, auth: &AuthUser, inner: &Value) -> AppResult<String> {
    let reference = inner
        .get("object")
        .and_then(ap_json::value_or_id)
        .ok_or(AppError::NotFound)?;
    match inner.get("type").and_then(ap_json::first_type).as_deref() {
        Some("Follow") => undo_follow_of(state, auth, &reference).await?,
        Some("Like") => undo_like_of(state, auth, &reference).await?,
        Some("Announce") => undo_announce_of(state, auth, &reference).await?,
        Some("Block") => undo_block_of(state, auth, &reference).await?,
        Some(_) | None => {
            return Err(AppError::BadRequest(UNSUPPORTED_ACTIVITY_TYPE.to_owned()));
        }
    }
    Ok(new_activity_id(&state.config))
}

/// An `Undo` naming the withdrawn activity by its id alone: whichever of
/// the caller's own rows carries the id (a `follows`, `favourite` or
/// `block` `ap_id`, or the `uri` of the caller's reblog row — an id
/// somebody else's activity bears names none of theirs, and a plain
/// status `uri` is no activity) is withdrawn, with the same hooks the
/// embedded form runs. Nothing carrying it is the shared 404.
async fn undo_by_activity_id(
    state: &AppState,
    auth: &AuthUser,
    reference: &str,
) -> AppResult<String> {
    if let Some(found) = follow::get_by_ap_id(&state.db, auth.0.id, reference).await? {
        return undo_named_follow(state, auth, &found).await;
    }
    if let Some(found) = favourite::get_by_ap_id(&state.db, auth.0.id, reference).await? {
        if let Some(removed) = favourite::unfavourite(&state.db, auth.0.id, found.status_id).await?
        {
            notify::on_favourite(state, &removed, false).await;
        }
        return Ok(new_activity_id(&state.config));
    }
    if let Some(found) = block::get_by_blocker_and_ap_id(&state.db, auth.0.id, reference).await? {
        return undo_named_block(state, auth, &found).await;
    }
    if let Some(row) = status::get_by_uri(&state.db, reference).await?
        && let Some(original_id) = row.reblog_of_id.filter(|_| row.user_id == auth.0.id)
    {
        if let Some(tombstone) = status::remove_reblog(&state.db, auth.0.id, original_id).await? {
            notify::on_reblog(state, &tombstone, false).await;
        }
        return Ok(new_activity_id(&state.config));
    }
    Err(AppError::NotFound)
}

/// The withdrawal of a follow found by its activity id: the followed
/// account is read off the row, and [`follow::unfollow`] takes it back —
/// the Mastodon route's unfollow ending, where a withdrawal reaches no
/// telling back, so the result needs only binding.
async fn undo_named_follow(state: &AppState, auth: &AuthUser, found: &Follow) -> AppResult<String> {
    let target = user_by_id(state, found.followee_id).await?;
    let _withdrawn = follow::unfollow(&state.db, &auth.0, &target).await?;
    Ok(new_activity_id(&state.config))
}

/// The withdrawal of a block found by its activity id: the blocked
/// account is read off the row, [`block::unblock`] takes the pair back,
/// and the hook fires for the row that went — the Mastodon route's own
/// ending.
async fn undo_named_block(state: &AppState, auth: &AuthUser, found: &Block) -> AppResult<String> {
    let target = user_by_id(state, found.blocked_id).await?;
    if let Some(removed) = block::unblock(&state.db, &auth.0, &target).await? {
        after_block_changed(state, &removed, false).await;
    }
    Ok(new_activity_id(&state.config))
}

/// The `Undo` of an embedded `Follow` naming its target actor.
async fn undo_follow_of(state: &AppState, auth: &AuthUser, reference: &str) -> AppResult<()> {
    let target = stored_actor(state, reference).await?;
    let previous = follow::get(&state.db, auth.0.id, target.id).await?;
    if follow::unfollow(&state.db, &auth.0, &target).await? {
        notify::on_follow(state, &auth.0, &target, previous.as_ref(), false).await;
    }
    Ok(())
}

/// The `Undo` of an embedded `Like` naming its status (or of a
/// withdrawn-by-id `Like`, whose row already names the status).
async fn undo_like_of(state: &AppState, auth: &AuthUser, reference: &str) -> AppResult<()> {
    let row = status_by_reference(state, reference)
        .await?
        .ok_or(AppError::NotFound)?;
    if let Some(removed) =
        favourite::unfavourite(&state.db, auth.0.id, action_target_id(&row)).await?
    {
        notify::on_favourite(state, &removed, false).await;
    }
    Ok(())
}

/// The `Undo` of an embedded `Announce` naming the boosted status: the
/// reblog-row-withdrawal of `POST …/unreblog`, tolerant of a pair with
/// nothing to take back.
async fn undo_announce_of(state: &AppState, auth: &AuthUser, reference: &str) -> AppResult<()> {
    let row = status_by_reference(state, reference)
        .await?
        .ok_or(AppError::NotFound)?;
    if let Some(tombstone) =
        status::remove_reblog(&state.db, auth.0.id, action_target_id(&row)).await?
    {
        notify::on_reblog(state, &tombstone, false).await;
    }
    Ok(())
}

/// The `Undo` of an embedded `Block` naming its target actor.
async fn undo_block_of(state: &AppState, auth: &AuthUser, reference: &str) -> AppResult<()> {
    let target = stored_actor(state, reference).await?;
    if let Some(removed) = block::unblock(&state.db, &auth.0, &target).await? {
        after_block_changed(state, &removed, false).await;
    }
    Ok(())
}

/// `GET /api/ap/whoami` — the authenticated user's own actor document:
/// Pleroma's `whoami` rendering `user.json` for the token's user, the
/// very bytes [`get_actor`] serves a federation fetch. The gate is a
/// bearer token granting `read`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read` check.
async fn whoami(State(state): State<AppState>, auth: AuthUser) -> AppResult<Response> {
    auth.require(READ)?;
    Ok(document_response(&actor_json(&auth.0, &state.config)))
}

/// `POST /api/ap/upload_media` — the C2S media upload, Pleroma's
/// `upload_media`: the very processing `POST /api/v1/media` runs
/// ([`store_upload`]), answered — as Pleroma does, and the ticket fixes
/// it — `200` with the stored row as an `ActivityStreams` `Document`. The
/// `id` it carries is the media id, which is what a posted `Create` names
/// in its `attachment` to claim the upload.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:media` check; whatever [`store_upload`] refuses — the
/// 413 over the upload limit, the 422 for a missing, blank or
/// undetectable file — and its storage and database failures.
async fn ap_upload_media(
    State(state): State<AppState>,
    auth: AuthUser,
    form: MultipartForm<UploadParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_MEDIA)?;
    let row = store_upload(&state, auth.0.id, form).await?;
    Ok(Json(document_json(&row)))
}

/// The stored media row as its `ActivityStreams` `Document`: Pleroma's
/// object shape — `url` a one-entry `Link` array carrying the file's type
/// and address, `name` the alt text (empty when it carries none), the
/// `mediaType` beside it and the `blurhash` when the analysis computed
/// one — with the `id` a client quotes from a create's attachment: the
/// media id itself, decision 4's hyphenated text.
fn document_json(row: &media_attachment::MediaAttachment) -> Value {
    let mut fields = vec![
        ("type", json!("Document")),
        (
            "url",
            json!([
                { "type": "Link", "mediaType": &row.content_type, "href": &row.url },
            ]),
        ),
        ("name", json!(row.description.clone().unwrap_or_default())),
        ("mediaType", json!(&row.content_type)),
    ];
    if let Some(blurhash) = &row.blurhash {
        fields.push(("blurhash", json!(blurhash)));
    }
    fields.push(("id", json!(row.id)));
    Value::Object(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

/// The single reference an interaction activity's `object` carries — a
/// bare id string or an embedded document's own — or the shared 404 for
/// an activity that names nothing: there is no object to act on.
fn activity_reference(activity: &Value) -> AppResult<String> {
    activity
        .get("object")
        .and_then(ap_json::value_or_id)
        .ok_or(AppError::NotFound)
}

/// The live status a reference names: text that parses as an id reads by
/// primary key, anything else as the object's `uri` — the two forms an
/// `ActivityPub` client may know a status by (`{BASE_URL}/objects/{uuid}`
/// included, which is what a `Like`'s `object` and a note's `inReplyTo`
/// carry). Tombstones and unknown references answer `None`, and the
/// caller turns that into the 404.
async fn status_by_reference(state: &AppState, reference: &str) -> AppResult<Option<Status>> {
    match parse_id(reference) {
        Some(id) => status::get_by_id(&state.db, id).await,
        None => status::get_by_uri(&state.db, reference).await,
    }
}

/// The stored, live account an actor reference names — the object of a
/// `Follow` or a `Block` (remote mirrors included; an unseen remote actor
/// is this endpoint's 404, and a deactivated one hides behind it, like
/// [`crate::api::accounts`]'s follow target gate).
async fn stored_actor(state: &AppState, reference: &str) -> AppResult<User> {
    user::get_by_ap_id(&state.db, reference)
        .await?
        .filter(|found| found.is_active.get())
        .ok_or(AppError::NotFound)
}

/// The row of a followed or blocked account a relationship names: the
/// pair gate read it live before the write, so its absence means the
/// account row itself is gone underneath the relationship — the 404.
async fn user_by_id(state: &AppState, id: Uuid) -> AppResult<User> {
    get_by_id(&state.db, id).await?.ok_or(AppError::NotFound)
}

/// The status an interaction activity points at, as its actor may see it:
/// the same gate [`crate::api::helpers::status_for_viewer_or_404`] is for
/// the Mastodon routes — a live row named by id or object `uri`, visible
/// to the caller, or the one 404 that never reveals a private post.
async fn interactable_status(
    state: &AppState,
    auth: &AuthUser,
    activity: &Value,
) -> AppResult<Status> {
    let reference = activity_reference(activity)?;
    let row = status_by_reference(state, &reference)
        .await?
        .ok_or(AppError::NotFound)?;
    if status_visibility::can_view(&state.db, &row, Some(&auth.0)).await? {
        Ok(row)
    } else {
        Err(AppError::NotFound)
    }
}

/// The status a boost lands on: `row` itself, or its original when the
/// client named a reblog row — nobody boosts a boost, the same fold
/// [`crate::api::statuses`]'s reblog route applies.
async fn reblog_target(state: &AppState, row: &Status) -> AppResult<Status> {
    match row.reblog_of_id {
        None => Ok(row.clone()),
        Some(original_id) => status::get_by_id(&state.db, original_id)
            .await?
            .ok_or(AppError::NotFound),
    }
}

/// The activity id a relationship row answers in `Location`: its own
/// `ap_id`, which [`follow::follow`], [`favourite::favourite`] and
/// [`block::block`] mint for every local maker (decision 6) — and a
/// freshly minted one for the row that somehow lacks it, so no arm has a
/// `None` to explain away.
fn row_activity_id(config: &Config, ap_id: Option<&str>) -> String {
    ap_id.map_or_else(|| new_activity_id(config), str::to_owned)
}

/// A freshly minted local activity id, `{BASE_URL}/activities/{uuid}`
/// (decision 6): the answer's `Location` for the writes that retire a
/// fact instead of leaving a row behind — the `Delete` and the `Undo`s.
fn new_activity_id(config: &Config) -> String {
    format!("{}/activities/{}", config.base_url, new_id())
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

    /// The audience rule of the C2S create: only values the visibility
    /// parser can read a recipient out of count as an audience.
    #[test]
    fn an_audience_names_nobody_when_it_carries_no_reference() {
        for empty in [Value::Null, json!([]), json!("")] {
            assert!(is_empty_audience(&empty), "`{empty}` addresses nobody");
        }
        for filled in [
            json!("https://test.example/users/bob"),
            json!(["https://test.example/users/bob"]),
            json!([{"id": "https://test.example/users/bob"}]),
        ] {
            assert!(
                !is_empty_audience(&filled),
                "`{filled}` names at least one recipient"
            );
        }
    }

    /// The create's audience is the note's own when it carries one and the
    /// enclosing activity's otherwise — the two places C2S clients write
    /// `to`/`cc`.
    #[test]
    fn the_addressing_falls_back_from_the_object_to_the_activity() {
        let activity = json!({
            "type": "Create",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": ["https://test.example/users/alice/followers"],
        });
        let own = json!({ "type": "Note", "to": ["https://test.example/users/bob"] });
        let merged = create_addressing(&own, &activity);
        assert_eq!(
            merged["to"],
            json!(["https://test.example/users/bob"]),
            "the object's own audience wins"
        );
        assert_eq!(
            merged["cc"],
            json!(["https://test.example/users/alice/followers"]),
            "and each half falls back on its own"
        );

        let empty = json!({ "type": "Note", "to": [], "cc": Value::Null });
        let echoed = create_addressing(&empty, &activity);
        assert_eq!(
            echoed["to"], activity["to"],
            "an empty array is no audience: the activity's applies"
        );
        assert_eq!(echoed["cc"], activity["cc"], "and so is a null one");

        let naked = json!({ "type": "Note" });
        let bare = create_addressing(&naked, &json!({"type": "Create"}));
        assert_eq!(
            bare,
            json!({ "to": [], "cc": [] }),
            "an activity that addresses nobody reads as addressing nobody"
        );
    }

    /// The attachment references a posted create may carry — the bare
    /// upload id or a URL ending in it — all read as the ids the create's
    /// `media_ids` wants, in submission order.
    #[test]
    fn attachment_references_read_as_upload_ids_in_order() {
        let object = json!({
            "type": "Note",
            "attachment": [
                {"type": "Document", "id": "01934567-89ab-7cde-f012-3456789abcde"},
                {"id": "https://test.example/api/v1/media/01934567-89ab-7cde-f012-4456789abcde"},
                "01934567-89ab-7cde-f012-5456789abcde",
                {"type": "Document"},
                {"name": "no reference at all"},
            ],
        });
        assert_eq!(
            attachment_ids(&object),
            [
                "01934567-89ab-7cde-f012-3456789abcde",
                "01934567-89ab-7cde-f012-4456789abcde",
                "01934567-89ab-7cde-f012-5456789abcde",
            ],
            "ids bare and as the last path segment; reference-less entries drop out"
        );
        assert!(
            attachment_ids(&json!({"type": "Note"})).is_empty(),
            "a note with no attachments claims no uploads"
        );
    }

    /// A posted `Create` folds only a `Note` object: every other shape is
    /// the 400 `Unsupported activity type`.
    #[test]
    fn only_a_note_document_is_a_creatable_object() {
        for object in [
            json!({"type": "Note", "content": "hi"}),
            json!({"type": ["Note", "Article"], "content": "hi"}),
        ] {
            let activity = json!({"type": "Create", "object": object});
            let note = note_object(&activity).expect("`Note` is the supported object type");
            assert_eq!(
                note["content"], "hi",
                "and it is the object that comes back"
            );
        }
        for activity in [
            json!({"type": "Create"}),
            json!({"type": "Create", "object": "https://test.example/objects/1"}),
            json!({"type": "Create", "object": {"type": "Question"}}),
            json!({"type": "Create", "object": {"content": "no type"}}),
            json!({"type": "Create", "object": Value::Null}),
        ] {
            let err = note_object(&activity)
                .expect_err("anything but an embedded Note is not a creatable object");
            assert_eq!(
                err.to_string(),
                UNSUPPORTED_ACTIVITY_TYPE,
                "and it is refused as the unsupported shape it is"
            );
        }
    }

    /// A media row, `document_json` renders twice over: `url` a one-entry
    /// `Link` array, `name` the alt text, `mediaType` beside it,
    /// `blurhash` only when there is one, and the media `id` for the
    /// create to quote.
    #[test]
    fn the_upload_document_is_the_c2s_attachment_shape() {
        let row = media_row(Some("a gradient"), Some("UBLF5j00Rj"));
        let document = document_json(&row);
        assert_eq!(document["type"], "Document", "the object type");
        assert_eq!(
            document["id"], "01934567-89ab-7cde-f012-3456789abcde",
            "the media id the create names in its attachment"
        );
        assert_eq!(
            document["url"],
            json!([{
                "type": "Link",
                "mediaType": "image/png",
                "href": "https://test.example/media/01934567/cat.png",
            }]),
            "the Link Pleroma's upload object carries"
        );
        assert_eq!(document["name"], "a gradient", "the alt text");
        assert_eq!(document["mediaType"], "image/png", "the file type");
        assert_eq!(document["blurhash"], "UBLF5j00Rj", "the computed hash");

        let bare = media_row(None, None);
        let undecorated = document_json(&bare);
        assert_eq!(undecorated["name"], "", "no alt text is the empty name");
        assert!(
            undecorated.get("blurhash").is_none(),
            "and an unhashed upload carries no blurhash key at all"
        );
    }

    /// The media row `document_json` renders, with only the fields the
    /// `Document` reads set.
    fn media_row(
        description: Option<&str>,
        blurhash: Option<&str>,
    ) -> media_attachment::MediaAttachment {
        media_attachment::MediaAttachment {
            id: Uuid::parse_str("01934567-89ab-7cde-f012-3456789abcde").unwrap(),
            user_id: None,
            status_id: None,
            position: 0,
            storage_key: None,
            remote_url: None,
            url: "https://test.example/media/01934567/cat.png".to_owned(),
            preview_url: None,
            content_type: "image/png".to_owned(),
            description: description.map(str::to_owned),
            blurhash: blurhash.map(str::to_owned),
            width: None,
            height: None,
            preview_width: None,
            preview_height: None,
            duration: None,
            focus_x: None,
            focus_y: None,
            file_size: None,
            sensitive_hint: false,
            inserted_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    /// The fresh activity id the withdrawals answer in `Location`:
    /// decision 6's local activity address, and `row_activity_id` keeps a
    /// row's own id when it has one.
    #[test]
    fn withdrawal_locations_are_fresh_activity_ids() {
        let config = Config::for_tests();
        let id = new_activity_id(&config);
        let tail = id
            .strip_prefix("https://test.example/activities/")
            .expect("a local activity id under the instance base URL");
        assert!(
            parse_id(tail).is_some(),
            "and the tail is the minted id itself: {id}"
        );
        assert_eq!(
            row_activity_id(&config, Some("https://peer.example/activities/7")),
            "https://peer.example/activities/7",
            "a row that carries an activity id is answered with it"
        );
        let minted = row_activity_id(&config, None);
        assert!(
            minted.starts_with("https://test.example/activities/"),
            "and one that does not falls back to a fresh id: {minted}"
        );
    }
}
