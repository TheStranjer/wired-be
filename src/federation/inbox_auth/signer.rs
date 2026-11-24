//! Resolving a `keyId` to the signer.
//!
//! The actor lookup behind the extractor's step 5 — the mirror, the
//! on-the-fly fetch, the standalone key object's `owner`, the gone actor's
//! self-`Delete` — plus the once-per-window key-refetch throttle its step 6
//! retries through.

use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard, PoisonError},
    time::Instant,
};

use serde_json::Value;
use tracing::debug;

use super::{DELETE_TYPE, InboxAuthError, KEY_REFETCH_WINDOW};
use crate::{
    federation::{
        fetch_actor::{FetchError, fetch_actor_json, get_or_fetch_by_ap_id},
        json::{as_array, value_or_id},
        signature::verify::key_id_actor,
    },
    models::user::{User, get_by_ap_id, get_by_public_key_id},
    state::AppState,
};

/// Actor URLs whose key was refetched within [`KEY_REFETCH_WINDOW`].
/// Process-wide by nature: the throttle protects the remote actor, not one
/// worker's state. A poisoned lock keeps its map (entries are plain data),
/// the same recovery as [`crate::util::cache::TimedCache`].
static KEY_REFETCH_STAMPS: Mutex<BTreeMap<String, Instant>> = Mutex::new(BTreeMap::new());

/// Resolves the `keyId` to the actor whose key must verify the signature.
///
/// This is step 5 of the [module docs](super): the mirror row that stored the
/// full `keyId` as its `publicKey.id` answers first (the path-style keys of
/// `GoToSocial`, which no fragment strip reaches); then the mirror of the
/// fragment-stripped URL; a miss fetches that actor; a fetch that fails on a
/// document that is not an actor retries the `keyId` itself as a key object,
/// following its `owner`.
///
/// # Errors
///
/// [`InboxAuthError::Discarded`] for a `410 Gone` answer to a self-`Delete`;
/// 401 `Unknown key` for anything else unresolved; database failures arrive
/// as [`InboxAuthError::Rejected`].
pub async fn resolve_signer(
    state: &AppState,
    key_id: &str,
    json: &Value,
) -> Result<User, InboxAuthError> {
    if let Some(stored) = get_by_public_key_id(&state.db, key_id).await? {
        return Ok(stored);
    }
    let actor_url = key_id_actor(key_id);
    if let Some(stored) = get_by_ap_id(&state.db, actor_url).await? {
        return Ok(stored);
    }
    match get_or_fetch_by_ap_id(state, actor_url).await {
        Ok(fetched) => Ok(fetched),
        Err(err) => resolve_key_fallback(state, &err, key_id, actor_url, json).await,
    }
}

/// The fallbacks of a failed actor resolution, told apart by *why* the
/// fetch failed: `Gone` + self-`Delete` is the accepted-and-dropped
/// account deletion; an actor document that never parsed may be a
/// standalone key object whose `owner` names the real actor.
async fn resolve_key_fallback(
    state: &AppState,
    err: &FetchError,
    key_id: &str,
    actor_url: &str,
    json: &Value,
) -> Result<User, InboxAuthError> {
    if matches!(err, FetchError::Gone) && is_self_delete(json, actor_url) {
        debug!(actor = %actor_url, "inbox Delete of a gone actor: accepted and dropped");
        return Err(InboxAuthError::Discarded);
    }
    if matches!(err, FetchError::Invalid(_))
        && let Some(owner) = key_object_owner(state, key_id).await
    {
        return fetch_key_owner(state, &owner, json).await;
    }
    debug!(key_id, %err, "inbox signature keyId resolves to no actor");
    Err(InboxAuthError::unknown_key())
}

/// Dereferences `key_id` as a standalone key document and reads its
/// `owner`: the actor the key belongs to. Any failure reads as "no
/// owner", because every outcome ends in the same 401.
async fn key_object_owner(state: &AppState, key_id: &str) -> Option<String> {
    let document = fetch_actor_json(&state.http_client, key_id, None)
        .await
        .ok()?;
    document.get("owner").and_then(value_or_id)
}

/// Loads the actor a key document's `owner` names; a `410 Gone` for a
/// self-`Delete` of that owner is dropped like the direct path's.
async fn fetch_key_owner(
    state: &AppState,
    owner: &str,
    json: &Value,
) -> Result<User, InboxAuthError> {
    match get_or_fetch_by_ap_id(state, owner).await {
        Ok(fetched) => Ok(fetched),
        Err(FetchError::Gone) if is_self_delete(json, owner) => Err(InboxAuthError::Discarded),
        Err(_err) => Err(InboxAuthError::unknown_key()),
    }
}

/// Whether `json` is a `Delete` by `actor_url` of that same actor: the
/// activity's `actor` and `object` (string or object id) both name it.
/// This is the one activity accepted while its signer's key is
/// unreachable — the account is gone and nothing is done with it.
pub(super) fn is_self_delete(json: &Value, actor_url: &str) -> bool {
    let names_actor = |key: &str| {
        json.get(key)
            .and_then(value_or_id)
            .is_some_and(|named| named == actor_url)
    };
    json.get("type").is_some_and(|types| {
        as_array(types)
            .iter()
            .any(|entry| entry.as_str() == Some(DELETE_TYPE))
    }) && names_actor("actor")
        && names_actor("object")
}

/// Whether `ap_id`'s key may be refetched now: records the attempt and
/// answers `false` while an earlier one is inside the window. Stale
/// stamps are pruned on every call, which bounds the map to the actors
/// with failures in the last [`KEY_REFETCH_WINDOW`].
pub(super) fn key_refetch_allowed(ap_id: &str) -> bool {
    let now = Instant::now();
    let mut stamps = refetch_stamps();
    stamps.retain(|_actor, then| now.saturating_duration_since(*then) < KEY_REFETCH_WINDOW);
    if stamps.contains_key(ap_id) {
        return false;
    }
    let _previous: Option<Instant> = stamps.insert(ap_id.to_owned(), now);
    true
}

/// The shared refetch-stamp map; a poisoned lock keeps its intact entries
/// (plain owned data, never half-updated).
fn refetch_stamps() -> MutexGuard<'static, BTreeMap<String, Instant>> {
    KEY_REFETCH_STAMPS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering};

    use axum::http::StatusCode;
    use serde_json::json;

    use super::*;
    use crate::{
        federation::inbox_auth::{
            UNKNOWN_KEY,
            test_support::{
                activity, bind, deliver, inbox_app, origin, peer, peer_app, self_delete, send,
                spawn, test_state,
            },
        },
        models::user::get_by_ap_id,
        util::keys::{FIXTURE_PRIVATE_PKCS1_PEM, FIXTURE_PUBLIC_PKCS1_PEM},
    };

    #[test]
    fn a_self_delete_names_the_deleted_actor_in_both_places() {
        let plain: Value =
            serde_json::from_slice(&self_delete("https://p.example/users/a")).unwrap();
        assert!(
            is_self_delete(&plain, "https://p.example/users/a"),
            "Delete by the actor of itself"
        );
        let embedded = json!({
            "type": ["Delete"],
            "actor": { "id": "https://p.example/users/a" },
            "object": { "id": "https://p.example/users/a" },
        });
        assert!(
            is_self_delete(&embedded, "https://p.example/users/a"),
            "array type and object spellings read too"
        );
        let note = json!({
            "type": "Delete",
            "actor": "https://p.example/users/a",
            "object": "https://p.example/objects/note",
        });
        assert!(
            !is_self_delete(&note, "https://p.example/users/a"),
            "deleting a Note is not deleting the actor"
        );
        assert!(
            !is_self_delete(&note, "https://p.example/objects/note"),
            "and the signer must be the actor, not the object"
        );
    }

    #[test]
    fn the_refetch_throttle_opens_once_per_actor_per_window() {
        assert!(
            key_refetch_allowed("https://throttle.example/users/a"),
            "a first failure may refetch"
        );
        assert!(
            !key_refetch_allowed("https://throttle.example/users/a"),
            "a second inside the window may not"
        );
        assert!(
            key_refetch_allowed("https://throttle.example/users/b"),
            "another actor has its own budget"
        );
    }

    /// A `keyId` that names no actor — and no key object — is unknown.
    #[tokio::test]
    async fn an_unknown_key_is_rejected() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
        let server = spawn(listener, peer_app(peer));
        let app = inbox_app(test_state(pool.clone()));

        let body = activity(
            &format!("{base}/users/ghost"),
            "https://test.example/users/bob",
        );
        let answer = send(
            &app,
            &deliver(
                &format!("{base}/missing#main-key"),
                FIXTURE_PRIVATE_PKCS1_PEM,
                &body,
            ),
        )
        .await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "unknown key");
        assert_eq!(answer.error, UNKNOWN_KEY, "resolution failure wording");
        server.abort();
    }

    /// A `keyId` whose URL is a standalone key object resolves through
    /// its `owner` to the actor document.
    #[tokio::test]
    async fn a_standalone_key_object_follows_its_owner() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
        let hits = Arc::clone(&peer.hits);
        let server = spawn(listener, peer_app(peer));
        let app = inbox_app(test_state(pool.clone()));

        let body = activity(&ap_id, "https://test.example/users/bob");
        let answer = send(
            &app,
            &deliver(&format!("{base}/keys/1"), FIXTURE_PRIVATE_PKCS1_PEM, &body),
        )
        .await;
        assert_eq!(
            answer.status,
            StatusCode::OK,
            "the owner's key verifies: {}",
            answer.error
        );
        assert_eq!(
            answer.json.get("signer").and_then(Value::as_str),
            Some(format!("alice@{}", base.strip_prefix("http://").expect("http base")).as_str()),
            "the owner is the signer"
        );
        assert!(
            hits.load(Ordering::SeqCst) >= 2,
            "the key document and the actor were both dereferenced"
        );
        let stored = get_by_ap_id(&pool, &ap_id).await.unwrap();
        assert!(stored.is_some(), "the owner landed in the mirror table");
        server.abort();
    }

    /// The `Delete` of an actor that answers `410 Gone` is accepted and
    /// dropped when it deletes itself — and only then.
    #[tokio::test]
    async fn a_gone_actors_self_delete_is_dropped_with_202() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let gone = format!("{base}/gone");
        let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
        let server = spawn(listener, peer_app(peer));
        let app = inbox_app(test_state(pool.clone()));

        let delete_reply = send(
            &app,
            &deliver(
                &format!("{gone}#main-key"),
                FIXTURE_PRIVATE_PKCS1_PEM,
                &self_delete(&gone),
            ),
        )
        .await;
        assert_eq!(
            delete_reply.status,
            StatusCode::ACCEPTED,
            "the self-Delete is accepted"
        );
        assert!(delete_reply.json.is_null(), "and dropped without a body");

        // A live-looking actor deleting something else while gone is not
        // the exception: a Note cannot be dropped as an account deletion.
        let note = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "type": "Delete",
            "actor": gone,
            "object": format!("{gone}/objects/1"),
        })
        .to_string()
        .into_bytes();
        let note_reply = send(
            &app,
            &deliver(
                &format!("{gone}#main-key"),
                FIXTURE_PRIVATE_PKCS1_PEM,
                &note,
            ),
        )
        .await;
        assert_eq!(
            note_reply.status,
            StatusCode::UNAUTHORIZED,
            "deleting an object is not self-deletion"
        );
        assert_eq!(note_reply.error, UNKNOWN_KEY, "and the key stays unknown");
        server.abort();
    }
}
