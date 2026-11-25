//! A signer whose stored key no longer verifies: the one forced refetch that
//! replaces it, and the window that keeps a failing signer from being refetched
//! again.

use super::*;
use crate::models::activitypub_actor::ActivityPubActorWrite;

/// A second key pair, unrelated to [`remote_keys`]: what a signer holds
/// when no key this instance can learn of fits its signature.
fn other_keys() -> &'static KeyPair {
    static KEYS: OnceLock<KeyPair> = OnceLock::new();
    KEYS.get_or_init(|| generate_keypair().expect("test key generation"))
}

/// A counting peer, its origin, and a `GET` of it signed with
/// `private_pem` (the key the peer serves, or another).
async fn counting_peer_signing_with(
    private_pem: &str,
) -> (
    tokio::task::JoinHandle<()>,
    SignedGet,
    Arc<AtomicUsize>,
    String,
) {
    let (server, base, hits) = counting_server().await;
    let get = sign_as(&format!("{base}{TARGET_PATH}#main-key"), private_pem);
    (server, get, hits, base)
}

/// Stores a mirror of the peer's actor holding a key it no longer uses.
async fn seed_stale_mirror(pool: &PgPool, base: &str) {
    let host = base.trim_start_matches("http://");
    let new = NewUser {
        nickname: format!("signer@{host}"),
        ap_id: format!("{base}{TARGET_PATH}"),
        domain: Some(host.to_owned()),
        name: "Signer".to_owned(),
        ..NewUser::default()
    };
    let seeded: User = insert(pool, &new).await.unwrap();
    let _row = activitypub_actor::upsert(
        pool,
        &ActivityPubActorWrite {
            inbox: Some(format!("{base}{TARGET_PATH}/inbox")),
            public_key: Some(STALE_KEY_PEM.to_owned()),
            ..ActivityPubActorWrite::bare(seeded.id)
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_rotated_key_is_refetched_and_the_fetch_passes() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let (server, get, hits, base) = counting_peer_signing_with(&remote_keys().private_pem).await;
    let state = state_with(db.pool().clone(), gate_on());
    seed_stale_mirror(&state.db, &base).await;

    check(&state, &get)
        .await
        .expect("the refetched key verifies the signature");

    assert_eq!(hits.load(Ordering::SeqCst), 1, "after exactly one refetch");
    let mirror = get_by_ap_id(&state.db, &format!("{base}{TARGET_PATH}"))
        .await
        .unwrap()
        .expect("the mirror is still stored");
    let identity = activitypub_actor::get(&state.db, mirror.id)
        .await
        .unwrap()
        .expect("the mirror keeps its identity row");
    assert_eq!(
        identity.public_key.expect("a key").trim(),
        remote_keys().public_pem.trim(),
        "and now carries the rotated key"
    );
    check(&state, &get)
        .await
        .expect("the refreshed mirror answers the next read");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "with no second fetch");
    server.abort();
}

#[tokio::test]
async fn a_refetch_that_still_fails_is_refused() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let (server, get, hits, base) = counting_peer_signing_with(&other_keys().private_pem).await;
    let state = state_with(db.pool().clone(), gate_on());
    seed_stale_mirror(&state.db, &base).await;

    let gate = check(&state, &get).await;
    let signer = ask_signer(&state, &get).await;

    assert!(refused(&gate), "neither the stored nor the fresh key fits");
    assert!(
        matches!(signer, Err(AppError::Unauthorized(_))),
        "the signer read is refused alike"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1, "the one refetch was spent");
    server.abort();
}

#[tokio::test]
async fn a_second_failure_inside_the_window_does_not_refetch_again() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let (server, get, hits, base) = counting_peer_signing_with(&other_keys().private_pem).await;
    let state = state_with(db.pool().clone(), gate_on());
    seed_stale_mirror(&state.db, &base).await;

    let first = check(&state, &get).await;
    assert!(refused(&first), "the first failure is refused");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "after one refetch");

    let second = check(&state, &get).await;
    assert!(refused(&second), "the second failure is refused");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the window is closed: no second fetch"
    );
    server.abort();
}
