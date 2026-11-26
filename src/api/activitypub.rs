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
//! `GET /objects/{uuid}/replies` (and `GET /contexts/{uuid}`, the thread
//! collection the notes name as their `context`), built by
//! [`crate::federation::collections`]
//! from the same controller's `outbox/2`, `followers/2`, `following/2`,
//! `pinned/2` and `object_replies/2` actions — and the four inbox
//! `POST`s of the actors those documents name (`inbox/2` feeding
//! `Federator.incoming_ap_doc/1`): the shared `/inbox`, the instance
//! actor's `/internal/fetch/inbox`, the relay actor's `/relay/inbox` and
//! the personal `/users/{nickname}/inbox`. The three relay documents
//! beside the shared inbox — `GET /relay` (an alias of the instance
//! actor's `/internal/fetch` JSON: this instance does not *serve* relays)
//! and the empty `/relay/following` and `/relay/followers` collections —
//! complete the same controller's `relay`, `relay_following` and
//! `relay_followers` actions.
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
//! ([`tombstone_json`](crate::federation::note::tombstone_json)) instead of the `{"error":"Gone"}` [`AppError::Gone`]
//! would render; a `local`-visibility status is 404 always, a `private` or
//! `direct` one is served only to a request whose HTTP Signature
//! [`fetch_signer`](crate::federation::authorized_fetch::fetch_signer) proves and
//! [`status_visibility::can_view`](crate::models::status_visibility::can_view) admits, and
//! everyone else — anonymous, unaddressed signer, blocked viewer — gets
//! the same 404. What remains is served as
//! [`note_json`](crate::federation::note::note_json)'s `Note` or `Question`.
//!
//! The activity reads serve the documents whose ids decision 6 mints as
//! `{BASE_URL}/activities/{uuid}` — Pleroma's `activity/2` and
//! `notice_activity` actions. `GET /activities/{uuid}` runs
//! [`find_local_by_id`](crate::models::activity::find_local_by_id), which
//! keys on the activity row's own `UUIDv7` and answers everything the
//! builders need, and renders [`like_json`](crate::federation::activities::like_json),
//! [`emoji_react_json`](crate::federation::activities::emoji_react_json),
//! [`follow_json`](crate::federation::activities::follow_json) or
//! [`block_json`](crate::federation::activities::block_json) for a relation row, or the status'
//! [`note_json`](crate::federation::note::note_json) wrapped in
//! [`create_activity_json`](crate::federation::note::create_activity_json) /
//! [`announce_activity_json`](crate::federation::note::announce_activity_json) for a status row.
//! Both routes negotiate like
//! the object reads — an `Accept` naming no `ActivityPub` media type is the
//! plain 404, no HTML branch on either. A `Create`/`Announce` follows its
//! status' visibility exactly as the object core does (`local` is 404,
//! `public`/`unlisted` open, `private`/`direct` only to an addressed
//! signer); a soft-deleted status resolves to no arm at all, so it is 404
//! here, never a `Tombstone`. A `Like`, `EmojiReact`, `Follow` or `Block`
//! is served only to a signed fetch
//! [`fetch_signer`](crate::federation::authorized_fetch::fetch_signer) proves whose
//! `users.id` is either the activity's actor or its target; everyone else —
//! unsigned, unrelated — gets the 404.
//! `GET /users/{nickname}/statuses/{uuid}/activity` serves the `Create` of
//! that live, non-reblog, local status: the object core's checks run first
//! (the nickname must name the author), and unknown id, remote row, reblog
//! row, tombstone, `local`, a mismatched nickname and an unsigned private
//! read are all the same 404 JSON.
//!
//! The quote consent document, `GET /quote_authorizations/{uuid}`, is the
//! `QuoteAuthorization` an `Accept` of a `QuoteRequest` names as its
//! `result` (FEP-044f): 404 for an id that names no grant, 410 once the
//! grant was revoked or the quoted status deleted.
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
    Router,
    http::{HeaderMap, Method, Uri},
    response::Response,
    routing::{get, post},
};

use crate::{
    config::Config,
    error::{AppError, AppResult},
    extract::multipart::upload_body_limit,
    federation::{authorized_fetch::fetch_allowed, note::LITEPUB_SCHEMA_PATH},
    models::user::{User, require_local_active},
    state::AppState,
};

use self::{
    activities::{get_activity, get_notice_activity},
    actors::{
        get_actor, get_featured, get_featured_tags, get_followers, get_following,
        get_instance_actor, get_instance_followers, get_instance_following, get_instance_outbox,
        get_outbox, get_relay, get_relay_followers, get_relay_following, litepub_schema,
    },
    c2s_misc::{ap_upload_media, whoami},
    inbox::{inbox, read_inbox, user_inbox},
    negotiation::{frontend_redirect, wants_activity_json, wants_html},
    objects::{get_context, get_notice, get_object, get_replies, get_scoped_object},
    outbox_create::update_outbox,
    quote_authorizations::get_quote_authorization,
};

/// The activity reads: `GET /activities/{uuid}` and the `Create` of a
/// notice at `GET /users/{nickname}/statuses/{uuid}/activity`.
mod activities;
/// The actor `GET`s: the actor document, the four collections beside it,
/// the instance actor, the three relay documents and the JSON-LD schema.
mod actors;
/// The remaining client-to-server routes — `whoami` and `upload_media` —
/// and the reference-resolution and id-minting helpers the outbox folds
/// its activities through.
mod c2s_misc;
/// The four inbox `POST`s and the owner's C2S inbox read.
mod inbox;
/// The content negotiation (`Accept` decides document, redirect or 404)
/// and the shared response endings every federation route answers
/// through.
mod negotiation;
/// The object reads: `GET /objects/{uuid}`, its nickname-scoped twin,
/// `GET /notice/{id}` and an object's replies collection.
mod objects;
/// The C2S outbox write: `POST /users/{nickname}/outbox`, its dispatch,
/// and the `Create` and `Delete` arms.
mod outbox_create;
/// The C2S outbox's relationship arms: the `Follow`, `Like`, `Announce`,
/// `Block` and `Undo` activities it folds.
mod outbox_relations;
/// The `QuoteAuthorization` read: `GET /quote_authorizations/{uuid}`.
mod quote_authorizations;
/// The scrobble documents: the `Audio` object and the `Listen` activity
/// the object and activity routes fall back to for a scrobble id.
mod scrobbles;
/// The fixtures shared by this module tree's unit tests.
#[cfg(test)]
mod test_support;

/// The text appended to [`ACTIVITY_JSON`](crate::federation::vocab::ACTIVITY_JSON)
/// to make the `Content-Type` of an actor document: the charset is spelled
/// out because axum's `Json` type would send it without one.
pub(super) const CHARSET_SUFFIX: &str = "; charset=utf-8";
/// The `Vary` value of every response: the `Accept` header picks the
/// document, the redirect or the 404.
pub(super) const VARY_ACCEPT: &str = "Accept";
/// The media type whose presence in `Accept` means "this is a browser":
/// Pleroma's HTML clients are sent to the frontend, and so here.
pub(super) const HTML_MEDIA_TYPE: &str = "text/html";
/// The `ActivityPub` address this instance's relay-facing actor document
/// is served at (Pleroma's `relay` route, `/relay`): here an alias of
/// [`INSTANCE_ACTOR_PATH`](crate::services::instance_actor::INSTANCE_ACTOR_PATH)'s
/// document, with the two empty subscription collections and the shared
/// inbox handler beside it.
pub(super) const RELAY_PATH: &str = "/relay";
/// 400 body when an authenticated activity carries no string `id`, no
/// string `type` or no `actor` (see
/// [`require_activity_fields`](crate::api::activitypub::inbox::require_activity_fields)).
pub(super) const INVALID_ACTIVITY: &str = "Invalid activity";
/// 403 body of every inbox route while `FEDERATION_ENABLED` is off.
pub(super) const FEDERATION_DISABLED: &str = "Federation is disabled";
/// Prefix of the incoming jobs' deduplication key: `incoming:{activity
/// id}`, the counterpart of the delivery queue's `deliver:…` key.
pub(super) const INCOMING_UNIQUE_PREFIX: &str = "incoming";
/// Attempts an incoming-activity job gets before it is given up on: the
/// ticket's five, the queue's own column default spelled out at the call.
pub(super) const INCOMING_MAX_ATTEMPTS: i32 = 5;
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
pub(super) const FORBIDDEN: &str = "Forbidden";
/// 400 body of the C2S outbox for an activity whose type it does not
/// fold — and for a `Create` whose object is not a `Note`.
pub(super) const UNSUPPORTED_ACTIVITY_TYPE: &str = "Unsupported activity type";
/// The `type` a posted `Create`'s object must wear to be folded: the
/// outbox takes the plain post and nothing else (a poll's `Question`, a
/// `Tombstone`, an actor — all unsupported).
pub(super) const OBJECT_TYPE_NOTE: &str = "Note";
/// The collection `type` of the C2S inbox root.
pub(super) const ORDERED_COLLECTION: &str = "OrderedCollection";
/// How many activities one C2S inbox page holds — the outbox page's size,
/// the ticket's twenty.
pub(super) const INBOX_PAGE_SIZE: i64 = 20;

/// The top-level `ActivityPub` router, merged by [`crate::app`].
///
/// It serves the actor documents peers fetch, the object reads
/// (`get_object`, `get_scoped_object` and `get_notice`), the activity
/// reads (`get_activity` and `get_notice_activity`), the activity
/// collections beside the actor (the account's outbox, followers,
/// following and featured list, an object's replies), and the four inbox
/// routes that accept their deliveries (`POST /inbox`,
/// `POST /users/{nickname}/inbox`, `POST /internal/fetch/inbox` and
/// `POST /relay/inbox`), beside the three relay `GET`s — the actor
/// document `/relay` and the empty `/relay/following` and
/// `/relay/followers` collections.
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
        .route("/quote_authorizations/{uuid}", get(get_quote_authorization))
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
        .route("/users/{nickname}/collections/tags", get(get_featured_tags))
        .route("/objects/{uuid}/replies", get(get_replies))
        .route("/contexts/{uuid}", get(get_context))
        .route("/internal/fetch", get(get_instance_actor))
        .route("/internal/fetch/outbox", get(get_instance_outbox))
        .route("/internal/fetch/followers", get(get_instance_followers))
        .route("/internal/fetch/following", get(get_instance_following))
        .route("/relay", get(get_relay))
        .route("/relay/following", get(get_relay_following))
        .route("/relay/followers", get(get_relay_followers))
        .route(LITEPUB_SCHEMA_PATH, get(litepub_schema))
        .route("/inbox", post(inbox))
        .route("/internal/fetch/inbox", post(inbox))
        .route("/relay/inbox", post(inbox))
        .route("/api/ap/whoami", get(whoami))
        .route(
            "/api/ap/upload_media",
            post(ap_upload_media).layer(upload_body_limit(config)),
        )
}

/// What the negotiation and the account lookup settled on for a
/// user-keyed `ActivityPub` `GET`.
pub(super) enum Reader {
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
pub(super) async fn gate_actor_read(
    state: &AppState,
    nickname: &str,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<Reader> {
    if !wants_activity_json(headers) {
        if wants_html(headers) {
            let redirect = frontend_redirect(&state.config, nickname)?;
            return Ok(Reader::Redirect(redirect));
        }
        return Err(AppError::RouteNotFound);
    }
    fetch_allowed(state, &Method::GET, uri, headers).await?;
    let user = require_local_active(&state.db, nickname).await?;
    Ok(Reader::Account(Box::new(user)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_inbox_routes_belong_to_the_ap_routes_bucket() {
        assert_eq!(
            RATE_LIMIT_BUCKET, "ap_routes",
            "Pleroma's bucket name, off by default there and waiting for \
             this server's first rate limiter"
        );
    }
}
