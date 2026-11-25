//! The lookup by `ActivityPub` id: [`get_or_fetch_by_ap_id`], which
//! consults the local mirror first, and [`serve_or_refresh`], the
//! freshness policy it runs a stored row through.

use chrono::{DateTime, Utc};
use tracing::debug;

use crate::{
    federation::fetch_actor::{
        FetchError, error::server_error, fetch::fetch_and_store, handle::Naming,
    },
    models::{
        activitypub_actor,
        user::{User, get_by_ap_id},
    },
    state::AppState,
};

use super::{fresh_since, is_local_ap_id};

/// Looks an actor up by `ActivityPub` id, fetching and mirroring it when
/// this instance does not have it (or has a stale mirror of it).
///
/// The database comes first: a local row answers as stored (this instance
/// owns its actors), a remote mirror younger than 24 hours answers as
/// stored too, and an older one is refetched and upserted. Any refresh
/// failure except [`FetchError::Gone`] falls back to the stale mirror —
/// one flaky peer must not hide an account this instance already knows.
/// An unknown *local* actor id (one on this instance's own origin) is
/// [`FetchError::NotFound`] without touching the network: its document
/// would be served by this very process.
///
/// # Errors
///
/// [`FetchError::NotFound`] for an unknown local id; otherwise, on a first
/// fetch, whatever
/// [`fetch_actor_json`](super::super::fetch_actor_json),
/// [`parse_actor`](crate::federation::remote_actor::parse_actor) or the
/// mirror write reports. On a refresh those failures only log and the
/// stale mirror answers.
pub async fn get_or_fetch_by_ap_id(state: &AppState, ap_id: &str) -> Result<User, FetchError> {
    get_or_fetch(state, ap_id, Naming::Confirm).await
}

/// [`get_or_fetch_by_ap_id`] for an actor id just taken from a `WebFinger`
/// answer.
///
/// The caller settles the handle from that answer itself (as
/// [`get_or_fetch_by_acct`](super::get_or_fetch_by_acct) does), so a fetch
/// here asks `WebFinger` nothing more: the mirror is written
/// under the parser's derived nickname, or under the stored one when its
/// local part still matches, so a split-domain handle survives a refetch.
/// One handle lookup is one `WebFinger` request.
///
/// # Errors
///
/// As [`get_or_fetch_by_ap_id`].
pub async fn get_or_fetch_by_fingered_ap_id(
    state: &AppState,
    ap_id: &str,
) -> Result<User, FetchError> {
    get_or_fetch(state, ap_id, Naming::Caller).await
}

/// The lookup behind [`get_or_fetch_by_ap_id`] and
/// [`get_or_fetch_by_fingered_ap_id`], with `naming` saying who settles a
/// fetched actor's nickname.
async fn get_or_fetch(state: &AppState, ap_id: &str, naming: Naming) -> Result<User, FetchError> {
    if let Some(stored) = get_by_ap_id(&state.db, ap_id)
        .await
        .map_err(|err| server_error(&err))?
    {
        return serve_or_refresh(state, stored, ap_id, naming).await;
    }
    if is_local_ap_id(&state.config, ap_id) {
        return Err(FetchError::NotFound);
    }
    fetch_and_store(state, ap_id, naming).await
}

/// When the mirror `stored` was last fetched, from its `activitypub_actors`
/// row.
///
/// A mirror with no row, like one never stamped, has no fetch to date, so
/// it counts as stale and is refetched.
async fn last_fetched_at(
    state: &AppState,
    stored: &User,
) -> Result<Option<DateTime<Utc>>, FetchError> {
    let actor = activitypub_actor::get(&state.db, stored.id)
        .await
        .map_err(|err| server_error(&err))?;
    Ok(actor.and_then(|row| row.last_fetched_at))
}

/// Answers from the stored mirror when it can (a local row, or a remote
/// one inside the refresh window), refetches otherwise — and on a failed
/// refetch keeps serving the stale mirror, `Gone` excepted.
async fn serve_or_refresh(
    state: &AppState,
    stored: User,
    ap_id: &str,
    naming: Naming,
) -> Result<User, FetchError> {
    if stored.local.get() || fresh_since(last_fetched_at(state, &stored).await?) {
        return Ok(stored);
    }
    match fetch_and_store(state, ap_id, naming).await {
        Ok(user) => Ok(user),
        // Gone states that the actor is permanently gone; every other
        // failure only says the peer is unreachable or wrong at this
        // moment, so the mirror this instance already has keeps serving.
        Err(FetchError::Gone) => Err(FetchError::Gone),
        Err(err) => {
            debug!(%err, "remote actor refresh failed; serving the stale mirror");
            Ok(stored)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering};

    use axum::{Router, http::StatusCode, routing::get};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        federation::{
            fetch_actor::test_support::*,
            remote_actor::{parse_actor, upsert_remote_user},
        },
        models::user::{NewUser, insert, local_ap_id},
    };
    #[tokio::test]
    async fn get_or_fetch_by_ap_id_inserts_then_serves_from_the_database() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(versioned_actor))
                .with_state(peer),
        );
        let state = test_state(pool);

        let first = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        assert_eq!(
            first.nickname,
            format!("alice@{}", authority_of(&base)),
            "preferredUsername@host of the id, port included (decision 7)"
        );
        assert_eq!(first.ap_id, ap_id, "the id is stored as fetched");
        assert_eq!(
            first.domain.as_deref(),
            Some(authority_of(&base)),
            "the id's host and port"
        );
        assert!(!first.local.get(), "the row is a remote mirror");
        assert_eq!(first.name, "First", "the document landed");
        let identity = activitypub_actor::get(&state.db, first.id)
            .await
            .unwrap()
            .expect("the fetch wrote the actor row");
        assert_eq!(
            identity.inbox.as_deref(),
            Some(format!("{ap_id}/inbox").as_str()),
            "the inbox endpoint"
        );
        assert!(identity.last_fetched_at.is_some(), "the fetch is stamped");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "one fetch");

        let second = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        assert_eq!(second.id, first.id, "the same mirror row");
        assert_eq!(second.name, "First", "the database answer, not a refetch");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the second call did not fetch"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_stale_mirror_is_refetched_and_updated() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(versioned_actor))
                .with_state(peer),
        );
        let state = test_state(pool);

        let first = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        make_stale(&state.db, &ap_id).await;

        let refreshed = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        assert_eq!(refreshed.id, first.id, "the existing row is updated");
        assert_eq!(refreshed.name, "Second", "the refetched document landed");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "a stale mirror triggers one fetch"
        );

        let again = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        assert_eq!(again.name, "Second", "the refreshed stamp is fresh again");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "no third fetch");
        server.abort();
    }

    /// Only the `activitypub_actors` stamp ages: `users` still says the
    /// mirror is fresh, so a refetch proves the stamp is read from the actor row.
    #[tokio::test]
    async fn the_fetch_stamp_is_read_from_the_actor_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(versioned_actor))
                .with_state(peer),
        );
        let state = test_state(pool);
        let first = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1, "the first lookup fetched");

        let aged = sqlx::query(
            "UPDATE activitypub_actors SET last_fetched_at = now() - interval '25 hours'
              WHERE user_id = $1",
        )
        .bind(first.id)
        .execute(&state.db)
        .await
        .unwrap()
        .rows_affected();
        assert_eq!(aged, 1, "the mirror has one actor row to age");

        let refreshed = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        assert_eq!(refreshed.id, first.id, "the same mirror row");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "the aged actor row alone made the mirror stale"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_dead_peer_falls_back_to_the_stale_mirror() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let dead = format!("http://{}/users/alice", listener.local_addr().unwrap());
        drop(listener);
        let stored = upsert_remote_user(&pool, parse_actor(&actor_document(&dead)).unwrap())
            .await
            .unwrap();
        make_stale(&pool, &dead).await;
        let state = test_state(pool);

        let served = get_or_fetch_by_ap_id(&state, &dead).await.unwrap();

        assert_eq!(served.id, stored.id, "the stale mirror is served");
        assert_eq!(served.name, "First", "the failed refresh overwrote nothing");
    }

    #[tokio::test]
    async fn a_gone_refresh_survives_the_stale_fallback() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let stored = upsert_remote_user(&pool, parse_actor(&actor_document(&ap_id)).unwrap())
            .await
            .unwrap();
        make_stale(&pool, &ap_id).await;
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(status_only))
                .with_state(StatusCode::GONE),
        );
        let state = test_state(pool);

        let error = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap_err();

        assert!(
            matches!(error, FetchError::Gone),
            "a 410 says the actor is gone; the mirror must not hide it: {error:?}"
        );
        let still = get_by_ap_id(&state.db, &ap_id)
            .await
            .unwrap()
            .expect("the mirror row stands");
        assert_eq!(still.id, stored.id, "and it was left untouched");
        server.abort();
    }

    #[tokio::test]
    async fn an_unknown_local_ap_id_is_not_found_without_network() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(pool);
        let ap_id = local_ap_id(TEST_BASE_URL, "nobody");

        let error = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap_err();

        assert!(
            matches!(error, FetchError::NotFound),
            "a local id this database does not have names nobody: {error:?}"
        );
    }

    #[tokio::test]
    async fn a_local_row_answers_as_stored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(
            &pool,
            &NewUser::local("alice", &local_ap_id(TEST_BASE_URL, "alice")),
        )
        .await
        .unwrap();
        let state = test_state(pool);

        let found = get_or_fetch_by_ap_id(&state, &alice.ap_id).await.unwrap();

        assert_eq!(found.id, alice.id, "the local row is returned, not fetched");
        assert!(found.local.get(), "and it stays a local row");
    }

    #[tokio::test]
    async fn a_first_fetch_of_a_document_that_is_not_an_actor_is_invalid() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(not_an_actor))
                .with_state(peer_at(&base)),
        );
        let state = test_state(pool);

        let error = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap_err();

        assert!(
            matches!(error, FetchError::Invalid(_)),
            "a document the parser rejects never becomes a mirror: {error:?}"
        );
        assert!(
            get_by_ap_id(&state.db, &ap_id).await.unwrap().is_none(),
            "and nothing was stored"
        );
        server.abort();
    }
}
