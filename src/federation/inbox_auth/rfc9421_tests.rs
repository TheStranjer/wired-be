//! Inbox authentication of RFC 9421 `Signature-Input` / `Signature`
//! deliveries, alone and beside a draft-cavage `Signature`.

use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use chrono::Utc;

use super::{
    ACTOR_MISMATCH, INVALID_DIGEST, INVALID_SIGNATURE, MISSING_SIGNATURE,
    test_support::{
        Delivery, INBOX_URL, ROTATED_PRIVATE_PEM, activity, bind, deliver, drop_header, inbox_app,
        origin, peer, peer_app, replace_header, seed_remote_user, send, spawn, test_state,
    },
};
use crate::{
    federation::signature::verify::rfc9421_test_support::{Rfc9421Key, content_digest, sign},
    util::keys::{FIXTURE_PRIVATE_PKCS1_PEM, FIXTURE_PUBLIC_PKCS1_PEM},
};

/// The `created` parameter of a signature made right now.
fn created_now() -> i64 {
    Utc::now().timestamp()
}

/// A delivery of `body` signed only the RFC 9421 way.
fn deliver_rfc9421(key: &Rfc9421Key<'_>, body: &[u8]) -> Delivery {
    let mut headers = vec![("content-type", "application/activity+json".to_owned())];
    headers.extend(sign("POST", INBOX_URL, Some(body), key));
    Delivery {
        body: body.to_vec(),
        headers,
    }
}

/// A delivery signed both ways: the cavage signature of `cavage_pem` moved
/// under `Authorization` (the bare `Signature` header is the RFC 9421
/// one) and the RFC 9421 signature of `rfc9421_pem` beside it.
fn deliver_both(key_id: &str, cavage_pem: &str, rfc9421_pem: &str, body: &[u8]) -> Delivery {
    let mut delivery = deliver(key_id, cavage_pem, body);
    let cavage = delivery
        .headers
        .iter()
        .find(|(name, _)| *name == "signature")
        .map(|(_, value)| value.clone())
        .unwrap();
    drop_header(&mut delivery, "signature");
    delivery
        .headers
        .push(("authorization", format!("Signature {cavage}")));
    let key = Rfc9421Key {
        key_id,
        private_pem: rfc9421_pem,
        created: created_now(),
    };
    // The cavage delivery already carries the one `host` header.
    delivery.headers.extend(
        sign("POST", INBOX_URL, Some(body), &key)
            .into_iter()
            .filter(|(name, _)| *name != "host"),
    );
    delivery
}

#[tokio::test]
async fn a_valid_rfc9421_delivery_authenticates_the_signer() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let listener = bind().await;
    let base = origin(&listener);
    let ap_id = format!("{base}/users/alice");
    let peer = peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned());
    let hits = std::sync::Arc::clone(&peer.hits);
    let server = spawn(listener, peer_app(peer));
    let app = inbox_app(test_state(pool.clone()));

    let body = activity(&ap_id, "https://test.example/users/bob");
    let key = Rfc9421Key {
        key_id: &format!("{ap_id}#main-key"),
        private_pem: FIXTURE_PRIVATE_PKCS1_PEM,
        created: created_now(),
    };
    let answer = send(&app, &deliver_rfc9421(&key, &body)).await;
    assert_eq!(
        answer.status,
        StatusCode::OK,
        "the RFC 9421 delivery: {}",
        answer.error
    );
    assert_eq!(
        answer
            .json
            .get("signer")
            .and_then(serde_json::Value::as_str),
        Some(format!("alice@{}", base.strip_prefix("http://").expect("http base")).as_str()),
        "the keyid resolved to the actor exactly as a cavage keyId does"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1, "one fetch resolved the key");
    server.abort();
}

#[tokio::test]
async fn a_tampered_rfc9421_delivery_is_refused() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let listener = bind().await;
    let base = origin(&listener);
    let ap_id = format!("{base}/users/alice");
    let server = spawn(
        listener,
        peer_app(peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned())),
    );
    let app = inbox_app(test_state(pool.clone()));
    let body = activity(&ap_id, "https://test.example/users/bob");
    let key = Rfc9421Key {
        key_id: &format!("{ap_id}#main-key"),
        private_pem: FIXTURE_PRIVATE_PKCS1_PEM,
        created: created_now(),
    };

    let mut moved = deliver_rfc9421(&key, &body);
    replace_header(&mut moved, "host", "evil.example".to_owned());
    let moved_reply = send(&app, &moved).await;
    assert_eq!(
        moved_reply.status,
        StatusCode::UNAUTHORIZED,
        "another authority"
    );
    assert_eq!(
        moved_reply.error, INVALID_SIGNATURE,
        "the signed URL differs"
    );

    let mut other_body = deliver_rfc9421(&key, &body);
    other_body.body = activity(&ap_id, "https://evil.example/steal");
    let body_reply = send(&app, &other_body).await;
    assert_eq!(body_reply.status, StatusCode::UNAUTHORIZED, "another body");
    assert_eq!(
        body_reply.error, INVALID_DIGEST,
        "the digest no longer fits"
    );

    let mut wrong_digest = deliver_rfc9421(&key, &body);
    replace_header(
        &mut wrong_digest,
        "content-digest",
        content_digest(b"other"),
    );
    let digest_reply = send(&app, &wrong_digest).await;
    assert_eq!(
        digest_reply.status,
        StatusCode::UNAUTHORIZED,
        "another digest"
    );
    assert_eq!(
        digest_reply.error, INVALID_DIGEST,
        "a Content-Digest of other bytes"
    );
    server.abort();
}

#[tokio::test]
async fn an_expired_or_unsigned_rfc9421_delivery_is_refused() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let listener = bind().await;
    let base = origin(&listener);
    let ap_id = format!("{base}/users/alice");
    let server = spawn(
        listener,
        peer_app(peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned())),
    );
    let app = inbox_app(test_state(pool.clone()));
    let body = activity(&ap_id, "https://test.example/users/bob");
    let key_id = format!("{ap_id}#main-key");

    let stale = Rfc9421Key {
        key_id: &key_id,
        private_pem: FIXTURE_PRIVATE_PKCS1_PEM,
        created: created_now() - 13 * 3600,
    };
    let stale_reply = send(&app, &deliver_rfc9421(&stale, &body)).await;
    assert_eq!(stale_reply.status, StatusCode::UNAUTHORIZED, "13 hours old");
    assert_eq!(stale_reply.error, INVALID_SIGNATURE, "outside the window");

    let fresh = Rfc9421Key {
        created: created_now(),
        ..stale
    };
    let mut headless = deliver_rfc9421(&fresh, &body);
    drop_header(&mut headless, "signature");
    let headless_reply = send(&app, &headless).await;
    assert_eq!(
        headless_reply.status,
        StatusCode::UNAUTHORIZED,
        "no Signature"
    );
    assert_eq!(
        headless_reply.error, MISSING_SIGNATURE,
        "Signature-Input alone"
    );
    server.abort();
}

#[tokio::test]
async fn another_actors_key_id_is_refused() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let listener = bind().await;
    let base = origin(&listener);
    let alice = format!("{base}/users/alice");
    let server = spawn(
        listener,
        peer_app(peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned())),
    );
    let bob = "https://remote.example/users/bob";
    seed_remote_user(&pool, bob, FIXTURE_PUBLIC_PKCS1_PEM).await;
    let app = inbox_app(test_state(pool.clone()));
    let body = activity(&alice, "https://test.example/users/carol");

    // Bob signs, validly, an activity that claims to be alice's.
    let as_bob = Rfc9421Key {
        key_id: &format!("{bob}#main-key"),
        private_pem: FIXTURE_PRIVATE_PKCS1_PEM,
        created: created_now(),
    };
    let bob_reply = send(&app, &deliver_rfc9421(&as_bob, &body)).await;
    assert_eq!(
        bob_reply.status,
        StatusCode::UNAUTHORIZED,
        "a stranger's keyid"
    );
    assert_eq!(
        bob_reply.error, ACTOR_MISMATCH,
        "contained to its own actor"
    );

    // Alice's keyid, signed with a key that is not hers.
    let forged = Rfc9421Key {
        key_id: &format!("{alice}#main-key"),
        private_pem: ROTATED_PRIVATE_PEM,
        created: created_now(),
    };
    let forged_reply = send(&app, &deliver_rfc9421(&forged, &body)).await;
    assert_eq!(
        forged_reply.status,
        StatusCode::UNAUTHORIZED,
        "alice's keyid"
    );
    assert_eq!(forged_reply.error, INVALID_SIGNATURE, "under the wrong key");
    server.abort();
}

#[tokio::test]
async fn either_valid_signature_carries_an_invalid_one_of_the_other_scheme() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let listener = bind().await;
    let base = origin(&listener);
    let ap_id = format!("{base}/users/alice");
    let server = spawn(
        listener,
        peer_app(peer(base.clone(), FIXTURE_PUBLIC_PKCS1_PEM.to_owned())),
    );
    let app = inbox_app(test_state(pool.clone()));
    let body = activity(&ap_id, "https://test.example/users/bob");
    let key_id = format!("{ap_id}#main-key");

    let valid_both = deliver_both(
        &key_id,
        FIXTURE_PRIVATE_PKCS1_PEM,
        FIXTURE_PRIVATE_PKCS1_PEM,
        &body,
    );
    let both_reply = send(&app, &valid_both).await;
    assert_eq!(
        both_reply.status,
        StatusCode::OK,
        "both valid: {}",
        both_reply.error
    );

    let bad_9421 = deliver_both(
        &key_id,
        FIXTURE_PRIVATE_PKCS1_PEM,
        ROTATED_PRIVATE_PEM,
        &body,
    );
    let cavage_reply = send(&app, &bad_9421).await;
    assert_eq!(
        cavage_reply.status,
        StatusCode::OK,
        "a valid cavage header beside an invalid RFC 9421 one: {}",
        cavage_reply.error
    );

    let bad_cavage = deliver_both(
        &key_id,
        ROTATED_PRIVATE_PEM,
        FIXTURE_PRIVATE_PKCS1_PEM,
        &body,
    );
    let rfc9421_reply = send(&app, &bad_cavage).await;
    assert_eq!(
        rfc9421_reply.status,
        StatusCode::OK,
        "a valid RFC 9421 header beside an invalid cavage one: {}",
        rfc9421_reply.error
    );

    let neither = deliver_both(&key_id, ROTATED_PRIVATE_PEM, ROTATED_PRIVATE_PEM, &body);
    let neither_reply = send(&app, &neither).await;
    assert_eq!(
        neither_reply.status,
        StatusCode::UNAUTHORIZED,
        "both invalid"
    );
    assert_eq!(
        neither_reply.error, INVALID_SIGNATURE,
        "the first failure is reported"
    );
    server.abort();
}
