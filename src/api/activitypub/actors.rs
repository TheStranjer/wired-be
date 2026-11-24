//! The actor `GET`s of the `ActivityPub` router: the actor document
//! [`get_actor`] serves and the four collections beside it
//! ([`get_outbox`], [`get_followers`], [`get_following`] and
//! [`get_featured`]), the instance actor document `GET /internal/fetch`,
//! the three relay documents beside it, and the static JSON-LD schema
//! `GET /schemas/litepub-0.1.jsonld`. The actor-keyed reads share the
//! front half [`gate_actor_read`](super::gate_actor_read) runs; the
//! relay and instance reads negotiate with
//! [`wants_activity_json`](super::negotiation::wants_activity_json) and
//! answer through the shared `Vary: Accept` ending.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, Uri},
    response::{IntoResponse as _, Response},
};
use serde_json::Value;

use crate::{
    error::{AppError, AppResult},
    federation::{
        collections::{self, FollowSide},
        note::litepub_context_json,
    },
    services::instance_actor,
    state::AppState,
};

use super::{
    ORDERED_COLLECTION, RELAY_PATH, Reader, gate_actor_read,
    negotiation::{actor_response, document, wants_activity_json, with_vary},
};

/// `GET /users/{nickname}` — the local actor named `nickname`, as an
/// `ActivityPub` document. No OAuth: federation fetches actors with
/// anonymous requests — or, once `AUTHORIZED_FETCH` is on, with HTTP
/// Signatures, which the gate above checks before the account is read.
///
/// `Vary: Accept` is added to whatever the route sends, so the
/// negotiation itself never has to remember the header.
pub(super) async fn get_actor(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(nickname): Path<String>,
) -> Response {
    let outcome = gate_actor_read(&state, &nickname, &headers, &uri).await;
    let response = match outcome {
        Ok(Reader::Account(user)) => actor_response(&state, &user).await,
        Ok(Reader::Redirect(redirect)) => Ok(redirect),
        Err(err) => Err(err),
    };
    with_vary(response)
}

/// `GET /users/{nickname}/outbox` — the account's public activity
/// stream: the collection root, or with `?page=true` a newest-first keyset
/// page (up to 20 per page, cursor `max_id`) of the `Create` and `Announce`
/// activities the account's publicly visible statuses federate as.
pub(super) async fn get_outbox(
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
pub(super) async fn get_followers(
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
pub(super) async fn get_following(
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
pub(super) async fn get_featured(
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

/// `GET /internal/fetch` — the instance actor document, served by the
/// same builder as a user's and typed `Application` by its stored
/// `actor_type`. Provisioned lazily here, so the route answers from the
/// first request even if startup never got to it; not gated by
/// `AUTHORIZED_FETCH` (see the module docs — this is the document a peer
/// needs in order to be able to sign).
pub(super) async fn get_instance_actor(State(state): State<AppState>) -> Response {
    match instance_actor::ensure_instance_actor(&state.db, &state.config).await {
        Ok(actor) => actor_response(&state, &actor)
            .await
            .unwrap_or_else(AppError::into_response),
        Err(err) => err.into_response(),
    }
}

/// `GET /relay` — this instance's relay-facing actor document. Serving
/// relays to others is not supported (the ticket): where Pleroma answers
/// this address with a dedicated `relay` service actor, here the answer
/// is the very JSON `GET /internal/fetch` serves — the same actor the
/// relay `Follow`s this instance sends are signed by, so a peer that
/// dereferences it and a peer that received its follow are looking at
/// one identity. The usual federation negotiation: an `Accept` naming an
/// `ActivityPub` media type gets the document, anything else the plain
/// 404. Like `/internal/fetch` the route is not gated by
/// `AUTHORIZED_FETCH`, and for the same reason — it is the document a
/// peer reads to learn the key it has to sign with.
pub(super) async fn get_relay(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let outcome = relay_actor_document(&state, &headers).await;
    with_vary(outcome)
}

/// The body of [`get_relay`]: the negotiation, then the instance actor,
/// provisioned lazily exactly as `GET /internal/fetch` provisions it.
async fn relay_actor_document(state: &AppState, headers: &HeaderMap) -> AppResult<Response> {
    if !wants_activity_json(headers) {
        return Err(AppError::RouteNotFound);
    }
    let actor = instance_actor::ensure_instance_actor(&state.db, &state.config).await?;
    actor_response(state, &actor).await
}

/// `GET /relay/following` — whom this instance follows in the relay role:
/// the empty `OrderedCollection`. It follows no subscribers as a relay
/// (that feature is not supported), and Pleroma's `relay_following` —
/// which renders the relay actor's own, ordinarily empty, following
/// collection — is answered here with the bare root, no account lookup
/// and no `AUTHORIZED_FETCH` gate.
pub(super) async fn get_relay_following(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    relay_collection(&state, &headers, "following")
}

/// `GET /relay/followers` — who follows this instance's relay actor:
/// [`get_relay_following`]'s twin. Nobody subscribes to an instance that
/// does not serve relays, so the collection is the same empty root at the
/// `/followers` address.
pub(super) async fn get_relay_followers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    relay_collection(&state, &headers, "followers")
}

/// The two relay collections' shared answer: the negotiation, then one
/// empty `OrderedCollection` root at `{BASE_URL}/relay/{side}`.
fn relay_collection(state: &AppState, headers: &HeaderMap, side: &str) -> Response {
    let outcome = if wants_activity_json(headers) {
        let id = format!("{}{RELAY_PATH}/{side}", state.config.base_url);
        Ok(collections::collection_root(
            &id,
            ORDERED_COLLECTION,
            Some(0),
            &state.config,
        ))
    } else {
        Err(AppError::RouteNotFound)
    };
    with_vary(document(outcome))
}

/// `GET /schemas/litepub-0.1.jsonld` — the JSON-LD context document every
/// `Note`/`Question` and activity this instance serves references from its
/// `@context`, the counterpart of Pleroma's static
/// `priv/static/schemas/litepub-0.1.jsonld`. Content negotiation, the
/// `AUTHORIZED_FETCH` gate and the account read are all irrelevant here:
/// it is one fixed document, so peers can expand our vocabulary before
/// they have ever authenticated to us.
pub(super) async fn litepub_schema() -> Json<Value> {
    Json(litepub_context_json())
}
