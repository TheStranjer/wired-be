//! Unit tests for the signed send: the delivered payload and VAPID header,
//! and how the push service's answers end the job.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier as _};
use serde_json::Value;

use super::super::test_support::{
    Setup, account, captured_body, client_keys, header, mock_push_service, note_about, open_pref,
    run, served, setup, state_with_config, subscribe,
};
use super::*;
use crate::{
    config::{Config, TEST_BASE_URL},
    entities::account::DEFAULT_AVATAR_PATH,
    http_client::HttpClient,
    services::media_proxy,
};

/// A signing secret long enough for the media proxy's minimum.
const PROXY_SECRET: &str = "0123456789abcdef0123456789abcdef01234567";

/// The stored avatar every fixture actor below carries.
const STORED_AVATAR: &str = "https://remote.example/avatar.png";

/// The test config with the media proxy switched on.
fn proxy_config() -> Config {
    let mut config = Config::for_tests();
    config.media_proxy.enabled = true;
    config.media_proxy.secret = Some(PROXY_SECRET.to_owned());
    config
}

/// A federated actor `bob@remote.example` with `name` and `avatar`.
async fn remote_actor(pool: &PgPool, name: &str, avatar: Option<&str>) -> user::User {
    user::insert(
        pool,
        &user::NewUser {
            nickname: "bob@remote.example".to_owned(),
            ap_id: "https://remote.example/users/bob".to_owned(),
            domain: Some("remote.example".to_owned()),
            name: name.to_owned(),
            avatar_url: avatar.map(str::to_owned),
            ..user::NewUser::default()
        },
    )
    .await
    .expect("the remote fixture inserts")
}

/// The decrypted payload of one follow push from `actor` to a fresh
/// recipient, delivered under `config`.
async fn follow_payload(pool: &PgPool, config: Config, actor: &user::User) -> Value {
    let recipient = account(pool, "payload-bob").await;
    let (url, seen, server) = mock_push_service(201).await;
    let keys = client_keys();
    let subscription = subscribe(pool, &recipient, &url, &keys, &open_pref()).await;
    let note = note_about(
        pool,
        &recipient.user,
        actor,
        notification::TYPE_FOLLOW,
        None,
    )
    .await;
    run(&state_with_config(pool, config), note.id, subscription.id)
        .await
        .expect("delivered");
    server.abort();
    let decrypted = web_push::decrypt_payload(&captured_body(&seen), &keys.private_key, &keys.auth)
        .expect("the client reads the body");
    serde_json::from_slice(&decrypted).expect("the payload is JSON")
}

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
    assert!(
        payload.get("access_token").is_none(),
        "the payload carries no credential: only digests are stored"
    );
    assert_eq!(payload["notification_id"], s.note.id.to_string());
    assert_eq!(payload["notification_type"], notification::TYPE_FOLLOW);
    assert_eq!(payload["preferred_locale"], "en", "the fixed locale");
    assert_eq!(
        payload["icon"],
        format!("{TEST_BASE_URL}/media{DEFAULT_AVATAR_PATH}"),
        "an actor with no avatar wakes with the instance default"
    );
    assert_eq!(
        payload["title"], "Alice followed you",
        "display name + verb"
    );
    assert_eq!(payload["body"], "", "a status-less telling carries no text");
    assert_eq!(s.actor.name, "Alice", "the fixture actor's name");
}

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

/// A subscription whose endpoint the SSRF guard blocks (a loopback mock
/// under the production client) completes the job and is deleted, instead
/// of retrying a delivery that can never pass.
#[tokio::test]
async fn a_guard_blocked_endpoint_retires_the_subscription_and_completes() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let s = setup(&pool, 201).await;
    let guarded = AppState {
        http_client: HttpClient::new("Wired (https://wired.test)"),
        ..s.state.clone()
    };

    let outcome = run(&guarded, s.note.id, s.subscription.id).await;

    assert_eq!(outcome, Ok(()), "a blocked endpoint is not a retry");
    assert!(!served(&s.seen), "the guard stopped the POST");
    assert!(
        web_push_subscription::find_by_id(&pool, s.subscription.id)
            .await
            .expect("the lookup runs")
            .is_none(),
        "the blocked subscription is deleted"
    );
    server_gone_stays_gone(&pool, &s).await;
    s.server.abort();
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

#[tokio::test]
async fn a_remote_actors_icon_is_the_signed_proxy_link_when_the_proxy_is_on() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let config = proxy_config();
    let actor = remote_actor(&pool, "Bob", Some(STORED_AVATAR)).await;

    let payload = follow_payload(&pool, config.clone(), &actor).await;

    let icon = payload["icon"].as_str().expect("the icon is a string");
    assert!(
        icon.starts_with(&format!("{TEST_BASE_URL}/proxy/")) && icon.ends_with("/avatar.png"),
        "the push service never sees the remote host: {icon}"
    );
    let (_, tail) = icon.split_once("/proxy/").expect("a proxy link");
    let mut segments = tail.split('/');
    let (sig, encoded) = (
        segments.next().expect("a signature"),
        segments.next().expect("an encoded url"),
    );
    assert_eq!(
        media_proxy::decode(&config, sig, encoded).expect("the signature verifies"),
        STORED_AVATAR,
        "the link decodes back to the stored avatar"
    );
}

#[tokio::test]
async fn a_remote_actors_icon_is_the_stored_url_when_the_proxy_is_off() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let actor = remote_actor(&pool, "Bob", Some(STORED_AVATAR)).await;

    let payload = follow_payload(&pool, Config::for_tests(), &actor).await;

    assert_eq!(
        payload["icon"], STORED_AVATAR,
        "a disabled proxy leaves the stored url"
    );
}

#[tokio::test]
async fn a_local_actors_icon_is_the_stored_url_even_with_the_proxy_on() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let actor = user::insert(
        &pool,
        &user::NewUser {
            name: "Alice".to_owned(),
            avatar_url: Some(STORED_AVATAR.to_owned()),
            ..user::NewUser::local("carol", &user::local_ap_id(TEST_BASE_URL, "carol"))
        },
    )
    .await
    .expect("the local fixture inserts");

    let payload = follow_payload(&pool, proxy_config(), &actor).await;

    assert_eq!(
        payload["icon"], STORED_AVATAR,
        "only remote actors' avatars are proxied"
    );
}

/// The fallback icon for an avatar-less actor hangs under
/// `MEDIA_BASE_URL`, where the backend serves the file, not under
/// `FRONTEND_BASE_URL`;
/// [`a_follow_wakes_the_subscription_with_a_signed_encrypted_payload`]
/// pins the URL for the default config.
#[tokio::test]
async fn the_default_icon_hangs_off_the_media_base_url() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let mut config = Config::for_tests();
    config.media_base_url = "https://cdn.example/files".to_owned();
    config.frontend_base_url = "https://fe.example".to_owned();
    let actor = remote_actor(&pool, "Bob", None).await;

    let payload = follow_payload(&pool, config, &actor).await;

    assert_eq!(
        payload["icon"],
        format!("https://cdn.example/files{DEFAULT_AVATAR_PATH}"),
        "the fallback icon is the default avatar under the media base URL"
    );
}

#[tokio::test]
async fn a_remote_actors_markup_never_reaches_the_title() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let actor = remote_actor(&pool, "<b>Bob</b>", Some(STORED_AVATAR)).await;

    let payload = follow_payload(&pool, Config::for_tests(), &actor).await;

    let title = payload["title"].as_str().expect("the title is a string");
    assert!(
        title.starts_with("Bob"),
        "the name leads the title: {title}"
    );
    assert!(!title.contains('<'), "no markup survives: {title}");
}
