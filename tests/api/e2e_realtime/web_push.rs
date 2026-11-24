//! Web Push scenarios: a mock push service, the browser-side key pair and
//! the end-to-end deliveries — payload decryption, alert flags, the
//! `followed` policy, and the `410` and `503` outcomes.

use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use p256::{
    SecretKey,
    elliptic_curve::{Generate as _, sec1::ToSec1Point as _},
};
use rand::RngCore as _;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use uuid::Uuid;
use wired_be::services::{vapid, web_push};

use super::server::{BASE, SCOPES, Server, Verb, call, expect, write_ok};
use super::sockets::{favourite, follow, named_account, post_status};
use crate::common;

/// The fallback push icon: the same default the `Account` entity renders,
/// spelled here because the constant is crate-private.
const DEFAULT_AVATAR: &str = "/media/default/avi.png";

// ──────────────────────────  Scenario 2: Web Push  ──────────────────────────

/// One request a mock push service captured: its headers and body bytes.
type Captured = (HeaderMap, Vec<u8>);

/// What one mock push service remembers: the status it answers with and
/// the requests it captured.
#[derive(Debug)]
struct MockPush {
    /// The status code every answer carries.
    status: u16,
    /// The captured requests, in arrival order.
    seen: Mutex<Vec<Captured>>,
}

impl MockPush {
    /// How many deliveries arrived.
    fn count(&self) -> usize {
        self.seen.lock().expect("an unpoisoned lock").len()
    }

    /// The last captured request, cloned out from under the lock.
    fn last(&self) -> Captured {
        let captured: Captured = {
            let guard = self.seen.lock().expect("an unpoisoned lock");
            guard.last().expect("the mock served a request").clone()
        };
        captured
    }

    /// The named header of the last captured request.
    fn header(&self, name: &str) -> String {
        let (headers, _body) = self.last();
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    /// The last captured request body.
    fn last_body(&self) -> Vec<u8> {
        let (_headers, body) = self.last();
        body
    }
}

/// The mock's handler: record the request, answer with the scripted status.
async fn catch_push(
    State(mock): State<Arc<MockPush>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    mock.seen
        .lock()
        .expect("an unpoisoned lock")
        .push((headers, body.to_vec()));
    StatusCode::from_u16(mock.status).expect("the test statuses are representable")
}

/// Serves a mock push service answering every `POST /push` with `status`,
/// on a fresh loopback port: the endpoint URL, its capture, and the task.
/// A plain-`http` loopback is exactly what the subscription endpoint
/// accepts (the `src/api/push.rs` gate follows Pleroma, which stores any
/// URL the delivery can call), and the instance's own client reaches it
/// because the test config allows private hosts.
async fn mock_push_service(status: u16) -> (String, Arc<MockPush>, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port to serve on");
    let url = format!(
        "http://{}/push",
        listener.local_addr().expect("the bound address")
    );
    let mock = Arc::new(MockPush {
        status,
        seen: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route("/push", post(catch_push))
        .with_state(Arc::clone(&mock));
    let server = tokio::spawn(async move {
        if axum::serve(listener, app).await.is_err() {
            // A dead mock reports itself through the empty capture.
        }
    });
    (url, mock, server)
}

/// The browser side of a subscription: the stored public half beside the
/// private scalar and auth secret the test decrypts with.
#[derive(Clone, Debug)]
struct ClientKeys {
    /// The stored client public point, base64url.
    p256dh: String,
    /// The stored auth secret, base64url.
    auth: String,
    /// The private scalar, test-side only.
    private_key: String,
}

/// A fresh P-256 key pair and 16-octet auth secret, the way a browser
/// generates them for `PushManager.subscribe`.
fn client_keys() -> ClientKeys {
    let secret = SecretKey::try_generate().expect("a fresh P-256 client key");
    let mut auth = [0_u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut auth);
    ClientKeys {
        p256dh: URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_point(false).as_bytes()),
        auth: URL_SAFE_NO_PAD.encode(auth),
        private_key: URL_SAFE_NO_PAD.encode(secret.to_bytes()),
    }
}

/// The pieces one `POST /api/v1/push/subscription` carries.
#[derive(Debug)]
struct PushSpec<'a> {
    /// The `push`-scoped bearer registering.
    token: &'a str,
    /// The push service's callback endpoint.
    endpoint: &'a str,
    /// The generated browser key pair.
    keys: &'a ClientKeys,
    /// The named alert flags.
    alerts: Value,
    /// The stored policy, `None` for the default `all`.
    policy: Option<&'a str>,
}

impl PushSpec<'_> {
    /// The body this spec registers with.
    fn body(&self) -> Value {
        json!({
            "subscription": {
                "endpoint": self.endpoint,
                "keys": {"p256dh": self.keys.p256dh, "auth": self.keys.auth},
            },
            "data": { "alerts": self.alerts.clone(), "policy": self.policy },
        })
    }
}

/// Registers `spec`'s subscription over HTTP, returning the entity.
async fn register_subscription(srv: &Server, spec: &PushSpec<'_>) -> Value {
    write_ok(
        srv,
        Verb::Post,
        "/api/v1/push/subscription",
        spec.token,
        &spec.body(),
    )
    .await
}

/// The decrypted push payload of the mock's last capture, as JSON.
fn delivered_payload(mock: &MockPush, keys: &ClientKeys) -> Value {
    let decrypted = web_push::decrypt_payload(&mock.last_body(), &keys.private_key, &keys.auth)
        .expect("the client's key reads the body");
    serde_json::from_slice(&decrypted).expect("the payload is JSON")
}

/// The pieces of a follow-alert registration for `token` at `endpoint`.
fn follow_spec<'a>(token: &'a str, endpoint: &'a str, keys: &'a ClientKeys) -> PushSpec<'a> {
    PushSpec {
        token,
        endpoint,
        keys,
        alerts: json!({"follow": true}),
        policy: None,
    }
}

/// The delivered document's shape asserted as a whole: no credential (the
/// subscriber's bearer `token` appears nowhere, since the token table keeps
/// only digests), the type, the display-name title, the plain-text body and
/// the locale.
fn assert_wake(payload: &Value, token: &str, kind: &str, title: &str, body: &str) {
    assert!(
        payload.get("access_token").is_none(),
        "the payload carries no access token: {payload}"
    );
    assert!(
        !payload.to_string().contains(token),
        "the subscriber's bearer token never rides in the payload"
    );
    assert_eq!(payload["notification_type"], json!(kind), "the type");
    assert!(
        Uuid::parse_str(
            payload["notification_id"]
                .as_str()
                .expect("a notification id")
        )
        .is_ok(),
        "it names a notification: {payload}"
    );
    assert_eq!(payload["title"], json!(title), "display name + verb");
    assert_eq!(payload["body"], json!(body), "the plain-text body");
    assert_eq!(payload["preferred_locale"], json!("en"), "the locale");
}

/// The wake end to end: a subscription registered over HTTP with a fresh
/// browser key pair, a follow written over HTTP, the delivery job drained
/// against the mock — and the captured body decrypting with that key to
/// the Mastodon-shaped document without its access token (only digests are
/// stored, so there is none to send): the notification id and type, the
/// default-avatar icon, the display-name title, and the transport headers (`aes128gcm`, the four-week `TTL`, the
/// VAPID `Authorization` naming the instance's stored key).
#[tokio::test]
async fn a_follow_over_http_delivers_a_decryptable_push_payload() {
    let srv = Server::start().await;
    let (alice, alice_token) =
        named_account(&srv, "rt226-wp-alice", "Alice", "read write push").await;
    let (_bob, bob_token) = named_account(&srv, "rt226-wp-bob", "Bob", SCOPES).await;
    let (url, mock, server) = mock_push_service(201).await;
    let keys = client_keys();

    let spec = follow_spec(&alice_token, &url, &keys);
    let entity = register_subscription(&srv, &spec).await;
    assert_eq!(entity["endpoint"], json!(url), "the endpoint reads back");
    let server_key = vapid::vapid_public_key(srv.pool())
        .await
        .expect("the instance VAPID pair");
    assert_eq!(
        entity["server_key"],
        json!(server_key),
        "the response advertises the instance key"
    );
    assert_eq!(
        entity["alerts"]["follow"],
        json!(true),
        "the named alert is on"
    );
    assert_eq!(
        entity["alerts"]["mention"],
        json!(false),
        "the unnamed one is off"
    );
    assert_eq!(entity["policy"], json!("all"), "the default policy");

    follow(&srv, &bob_token, alice.id).await;
    let ran = common::run_all_jobs(&srv.state).await;
    server.abort();
    assert!(ran >= 1, "the follow queued its delivery job");

    assert_eq!(mock.count(), 1, "one request reached the push service");
    assert_eq!(mock.header("content-encoding"), "aes128gcm", "the coding");
    assert_eq!(mock.header("ttl"), web_push::TTL_HEADER, "the four weeks");
    assert_eq!(mock.header("urgency"), "normal", "the middle urgency");
    let authorization = mock.header("authorization");
    assert!(
        authorization.starts_with("vapid t=")
            && authorization.ends_with(&format!(", k={server_key}")),
        "the VAPID signature names the instance key: {authorization}"
    );

    let payload = delivered_payload(&mock, &keys);
    assert_wake(&payload, &alice_token, "follow", "Bob followed you", "");
    assert_eq!(
        payload["icon"],
        json!(format!("{BASE}{DEFAULT_AVATAR}")),
        "an avatar-less actor wakes with the instance default"
    );
}

/// The alert flags, written and re-written over HTTP: a subscription
/// created with only the follow alert on stays dark when a status of the
/// subscriber's is favourited, and the `PUT` that names the favourite flag
/// turns the next one through — the second delivery's payload carries the
/// status's plain text as its body.
#[tokio::test]
async fn alert_flags_gate_the_wake_and_the_put_reopens_them() {
    let srv = Server::start().await;
    let (_alert_alice, alice_token) =
        named_account(&srv, "rt226-alert-alice", "Alice", "read write push").await;
    let (_alert_bob, bob_token) = named_account(&srv, "rt226-alert-bob", "Bob", SCOPES).await;
    let (_alert_carol, carol_token) =
        named_account(&srv, "rt226-alert-carol", "Carol", SCOPES).await;
    let (url, mock, server) = mock_push_service(201).await;
    let keys = client_keys();
    let spec = follow_spec(&alice_token, &url, &keys);
    let _registered = register_subscription(&srv, &spec).await;

    let liked = common::id_of(&post_status(&srv, &alice_token, "pushed body text", "public").await);
    favourite(&srv, &bob_token, &liked).await;
    let _ran_dark = common::run_all_jobs(&srv.state).await;
    assert_eq!(mock.count(), 0, "the off favourite alert sends nothing");

    let _updated = write_ok(
        &srv,
        Verb::Put,
        "/api/v1/push/subscription",
        &alice_token,
        &json!({ "data": { "alerts": { "favourite": true } } }),
    )
    .await;
    favourite(&srv, &carol_token, &liked).await;
    let _ran_wake = common::run_all_jobs(&srv.state).await;
    server.abort();

    assert_eq!(mock.count(), 1, "the re-opened alert wakes");
    assert_wake(
        &delivered_payload(&mock, &keys),
        &alice_token,
        "favourite",
        "Carol favourited your post",
        "pushed body text",
    );
}

/// The policy gate, end to end: a `followed` subscription stays dark for
/// an account its owner does not follow, and the follow written over HTTP
/// between the two attempts lets the next telling through.
#[tokio::test]
async fn the_followed_policy_wakes_only_for_accounts_the_owner_follows() {
    let srv = Server::start().await;
    let (_pol_alice, alice_token) =
        named_account(&srv, "rt226-pol-alice", "Alice", "read write push").await;
    let (dave, dave_token) = named_account(&srv, "rt226-pol-dave", "Dave", SCOPES).await;
    let (url, mock, server) = mock_push_service(201).await;
    let keys = client_keys();
    let spec = PushSpec {
        token: &alice_token,
        endpoint: &url,
        keys: &keys,
        alerts: json!({"favourite": true}),
        policy: Some("followed"),
    };
    let _registered = register_subscription(&srv, &spec).await;
    let first = common::id_of(&post_status(&srv, &alice_token, "one", "public").await);
    let second = common::id_of(&post_status(&srv, &alice_token, "two", "public").await);

    favourite(&srv, &dave_token, &first).await;
    let _ran_dark = common::run_all_jobs(&srv.state).await;
    assert_eq!(mock.count(), 0, "a stranger stays behind the policy");

    follow(&srv, &alice_token, dave.id).await;
    favourite(&srv, &dave_token, &second).await;
    let _ran_wake = common::run_all_jobs(&srv.state).await;
    server.abort();

    assert_eq!(
        mock.count(),
        1,
        "the followed actor now wakes the subscription"
    );
    assert_wake(
        &delivered_payload(&mock, &keys),
        &alice_token,
        "favourite",
        "Dave favourited your post",
        "two",
    );
}

/// A `410` from the push service retires the subscription through the
/// job: after the drain, `GET /api/v1/push/subscription` is the 404 — the
/// row is gone, not merely quiet.
#[tokio::test]
async fn a_gone_push_service_retires_the_subscription() {
    let srv = Server::start().await;
    let (alice, alice_token) =
        named_account(&srv, "rt226-gone-alice", "Alice", "read write push").await;
    let (_gone_bob, bob_token) = named_account(&srv, "rt226-gone-bob", "Bob", SCOPES).await;
    let (gone_url, gone_mock, gone_server) = mock_push_service(410).await;
    let gone_keys = client_keys();
    let spec = follow_spec(&alice_token, &gone_url, &gone_keys);
    let _registered = register_subscription(&srv, &spec).await;

    follow(&srv, &bob_token, alice.id).await;
    let _ran = common::run_all_jobs(&srv.state).await;
    gone_server.abort();
    assert_eq!(gone_mock.count(), 1, "the dead service got its one try");

    let retired = call(
        &srv,
        Verb::Get,
        "/api/v1/push/subscription",
        Some(&alice_token),
        None,
    )
    .await;
    expect(&retired, StatusCode::NOT_FOUND, "the 410 retired the row");
}

/// A `503` keeps the subscription: the attempt reaches the service once,
/// the job rides the queue's backoff instead of retrying inside the
/// drain, and the subscription still reads 200.
#[tokio::test]
async fn a_flaky_push_service_keeps_the_subscription_and_retries_later() {
    let srv = Server::start().await;
    let (alice, alice_token) =
        named_account(&srv, "rt226-flaky-alice", "Alice", "read write push").await;
    let (_flaky_actor, actor_token) = named_account(&srv, "rt226-flaky-bob", "Bob", SCOPES).await;
    let (retry_url, retry_mock, retry_server) = mock_push_service(503).await;
    let retry_keys = client_keys();
    let spec = follow_spec(&alice_token, &retry_url, &retry_keys);
    let _registered = register_subscription(&srv, &spec).await;

    follow(&srv, &actor_token, alice.id).await;
    let ran = common::run_all_jobs(&srv.state).await;
    let second_ran = common::run_all_jobs(&srv.state).await;
    retry_server.abort();
    assert_eq!(ran, 1, "the flaky service got its attempt");
    assert_eq!(
        second_ran, 0,
        "and nothing more is due yet — the retry rides the queue's backoff"
    );
    assert_eq!(retry_mock.count(), 1, "the try reached it once, 503");

    let standing = call(
        &srv,
        Verb::Get,
        "/api/v1/push/subscription",
        Some(&alice_token),
        None,
    )
    .await;
    expect(
        &standing,
        StatusCode::OK,
        "a retryable failure keeps the subscription",
    );
}
