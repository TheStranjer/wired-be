//! Unit tests for the signed send: the delivered payload and VAPID header,
//! and how the push service's answers end the job.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier as _};
use serde_json::Value;

use super::super::test_support::{Setup, captured_body, header, run, setup};
use super::*;
use crate::{config::TEST_BASE_URL, entities::account::DEFAULT_AVATAR_PATH};

#[tokio::test]
async fn a_follow_wakes_the_subscription_with_a_signed_encrypted_payload() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let s = setup(&pool, 201).await;

    let outcome = run(&s.state, s.note.id, s.subscription.id).await;

    assert_eq!(outcome, Ok(()), "the mock accepted the delivery");
    s.server.abort();
    assert_eq!(
        header(&s.seen, "content-encoding"),
        "aes128gcm",
        "the coding is announced"
    );
    assert_eq!(
        header(&s.seen, "content-type"),
        "application/octet-stream",
        "the body is opaque to the service"
    );
    assert_eq!(header(&s.seen, "ttl"), web_push::TTL_HEADER, "four weeks");
    assert_eq!(header(&s.seen, "urgency"), "normal", "the middle urgency");
    let authorization = header(&s.seen, "authorization");
    let stored_public = vapid::vapid_public_key(&pool)
        .await
        .expect("the instance key pair");
    assert!(
        authorization.starts_with("vapid t=")
            && authorization.ends_with(&format!(", k={stored_public}")),
        "the signed header advertises the instance key: {authorization}"
    );
    let decrypted =
        web_push::decrypt_payload(&captured_body(&s.seen), &s.keys.private_key, &s.keys.auth)
            .expect("the body is a record the client's keys read");
    let payload: Value = serde_json::from_slice(&decrypted).expect("the payload is JSON");
    assert_eq!(
        payload["access_token"], s.recipient.token.token,
        "the wake token is the subscription's own"
    );
    assert_eq!(payload["notification_id"], s.note.id.to_string());
    assert_eq!(payload["notification_type"], notification::TYPE_FOLLOW);
    assert_eq!(payload["preferred_locale"], "en", "the fixed locale");
    assert_eq!(
        payload["icon"],
        format!("{TEST_BASE_URL}{DEFAULT_AVATAR_PATH}"),
        "an actor with no avatar wakes with the instance default"
    );
    assert_eq!(
        payload["title"], "Alice followed you",
        "display name + verb"
    );
    assert_eq!(payload["body"], "", "a status-less telling carries no text");
    assert_eq!(s.actor.name, "Alice", "the fixture actor's name");
}

/// The fallback icon for an avatar-less actor hangs under
/// `FRONTEND_BASE_URL`, where the frontend ships the file, not under
/// the API's own origin; [`a_follow_wakes_the_subscription_with_a_signed_encrypted_payload`]
/// above keeps the unchanged URL pinned for the default config, where
/// the variable is unset and the origins are equal.
#[tokio::test]
async fn the_delivered_vapid_jwt_verifies_and_names_the_endpoint_origin() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let s = setup(&pool, 201).await;
    run(&s.state, s.note.id, s.subscription.id)
        .await
        .expect("delivered");
    s.server.abort();

    let authorization = header(&s.seen, "authorization");
    let jwt = authorization
        .strip_prefix("vapid t=")
        .and_then(|rest| rest.split_once(", k="))
        .expect("the captured header is a vapid pair")
        .0
        .to_owned();
    let parts: Vec<&str> = jwt.split('.').collect();
    let claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(parts[1])
            .expect("the claims are base64url"),
    )
    .expect("the claims are JSON");
    let origin = s.url.strip_suffix("/push").expect("the mock url shape");
    assert_eq!(claims["aud"], origin, "aud is the endpoint origin");
    assert_eq!(
        claims["sub"], TEST_BASE_URL,
        "with no CONTACT_EMAIL configured the instance origin is the subject"
    );
    let now = Utc::now().timestamp();
    let exp = claims["exp"].as_i64().expect("exp is numeric");
    assert!(
        exp > now + 43_000 && exp < now + 44_000,
        "exp is twelve hours out: {exp} against now {now}"
    );
    let pair = vapid::vapid_public_key(&pool).await.expect("the key pair");
    let verifying = VerifyingKey::from_sec1_bytes(
        &URL_SAFE_NO_PAD
            .decode(pair)
            .expect("the public half decodes"),
    )
    .expect("the instance point parses");
    let signature = Signature::from_slice(
        &URL_SAFE_NO_PAD
            .decode(parts[2])
            .expect("the signature is base64url"),
    )
    .expect("the JWT signature is the fixed r||s form");
    verifying
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .expect("the delivered JWT verifies against the stored public key");
}

#[tokio::test]
async fn a_410_deletes_the_subscription_and_completes() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let s = setup(&pool, 410).await;

    let outcome = run(&s.state, s.note.id, s.subscription.id).await;

    assert_eq!(
        outcome,
        Ok(()),
        "a retired subscription is not a failed job"
    );
    assert!(
        web_push_subscription::find_by_id(&pool, s.subscription.id)
            .await
            .expect("the lookup runs")
            .is_none(),
        "the 410 took the row with it"
    );
    server_gone_stays_gone(&pool, &s).await;
    s.server.abort();
}

/// The 410's delete is final for the token: the token's lookup reads
/// nothing back either (the job deleted by `token_id`, the row's
/// unique key).
async fn server_gone_stays_gone(pool: &PgPool, s: &Setup) {
    assert!(
        web_push_subscription::find(pool, s.recipient.token.id)
            .await
            .expect("the lookup runs")
            .is_none(),
        "the delete went through the token, the row's unique key"
    );
}

#[tokio::test]
async fn a_server_error_retries_the_job_and_keeps_the_subscription() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let s = setup(&pool, 503).await;

    let outcome = run(&s.state, s.note.id, s.subscription.id).await;

    match outcome {
        Err(JobError::Retry(detail)) => assert!(
            detail.contains("503"),
            "the retry names the peer's answer: {detail}"
        ),
        other => panic!("a 503 must retry the job, got {other:?}"),
    }
    assert!(
        web_push_subscription::find_by_id(&pool, s.subscription.id)
            .await
            .expect("the lookup runs")
            .is_some(),
        "a retryable failure keeps the subscription"
    );
    s.server.abort();
}
