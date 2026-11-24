//! The fixtures the inbox-authentication tests share: the test state and
//! the extractor-gated router under test, the loopback mock peer with its
//! actor, key-object, `410` and `404` routes, the signed-delivery builder
//! and its tampering helpers, the mirror and policy seeding, and the
//! `oneshot` drive that reads the answer. Test-only.

use std::{
    str::FromStr as _,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Json, Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt as _;

use super::SignedActivity;
pub(super) use crate::test_server::{bind, origin, spawn};
use crate::{
    config::Config,
    federation::signature::{SigningKey, sign_request},
    models::user::{NewUser, User, insert},
    services::runtime_config,
    state::AppState,
};

/// The origin test requests are addressed to and sign their `Host` as:
/// the [`Config::for_tests`] base url, matching what a real peer signs.
pub(super) const INBOX_URL: &str = "https://test.example/inbox";
/// The host every loopback mock is served from: the domain of the peer
/// a delivery here is signed as, and the one a reject list names.
pub(super) const LOOPBACK_HOST: &str = "127.0.0.1";
/// A five-byte body, for the size-cap test with a four-byte limit.
pub(super) const BODY: &[u8] = b"hello";
/// The `publicKeyPem` a mirror row seeds with when it must not verify.
pub(super) const STALE_KEY_PEM: &str =
    "-----BEGIN PUBLIC KEY-----\nSTALE\n-----END PUBLIC KEY-----\n";

/// The second pair's private key, PKCS#8 PEM, generated offline like
/// [`FIXTURE_PRIVATE_PKCS1_PEM`](crate::util::keys::FIXTURE_PRIVATE_PKCS1_PEM): the fixture key must not accept it.
pub(super) const ROTATED_PRIVATE_PEM: &str = "
-----BEGIN PRIVATE KEY-----
MIICdQIBADANBgkqhkiG9w0BAQEFAASCAl8wggJbAgEAAoGBALDiE29QVeI6UuQV
hAwkdU7CFiDpvovtvIDw9a7hj/csyzFMqOops5OdsyELlWVJDYVbIWsu9YIdOoLf
P8ZDNT2UXbZV355Mb9vNX6WdWdxyw2KNQKwVVTDxNzIC9UQns76BYFUuAk27/0uJ
TKHbq7uoQfVVsfroyNDlV1zZ7TQZAgMBAAECgYBpzNATJNo6Fui9xKDXIneIRE84
IJf42pqGBZrun7yc/A4Nzuy6I3dmNE2Q1nzluGx996RlmBEBYro8zYw36rtMu1Hv
ljOplbJPRzcghTJXT7viCocv/FSaVDb5eFCb+dQSu4cXIrdK0DTkxjILKs2vYAhY
DoekP5AVBCx60fvqAQJBAN0L4kj6rDAI+S8C0Ad/yMS6ctDD6IYo24CpBtsjBZyd
CcfT7UlR0OXP3bxto3rHPJ8xM2jgBG9+KoR1kqrV6N0CQQDM2m4D4xD6kvKLGD0N
5Fb1K+3QR7gK2/AG6Fu/eYEjv3q8TjeOE9+ikbilpJMxSrmrSabRPKD0P3CJ1Qxw
xWZtAkA9Fsuy35Q2skQreduENwyFUgHpMDtS2HfRVuVOKp+iaMQLNcaePL4+xPPC
34oOpIFcfXx3v3F6XqPhd5zRGc0BAkAIUVIoPD8DPZY1iZ1pA/135Pm5/D9/yWBc
l3CqPJ++uNad8LsWvL3W1v3dU+paFQOm7c2lL0YFxNGn9wuTRKCdAkB/CQsvdoEM
Xtk2GaHvtK4bFI0BVjRZConB8iruDzYNaVu0GB4KpK5jiT+4pdRCdoEADMAAJghz
abbY4/vMuKDE
-----END PRIVATE KEY-----
";

/// The public half of [`ROTATED_PRIVATE_PEM`], legacy PKCS#1 PEM: what a
/// peer serves as its `publicKeyPem`.
pub(super) const ROTATED_PUBLIC_PEM: &str = "
-----BEGIN RSA PUBLIC KEY-----
MIGJAoGBALDiE29QVeI6UuQVhAwkdU7CFiDpvovtvIDw9a7hj/csyzFMqOops5Od
syELlWVJDYVbIWsu9YIdOoLfP8ZDNT2UXbZV355Mb9vNX6WdWdxyw2KNQKwVVTDx
NzIC9UQns76BYFUuAk27/0uJTKHbq7uoQfVVsfroyNDlV1zZ7TQZAgMBAAE=
-----END RSA PUBLIC KEY-----
";

/// The state under test: `pool`, the test config and an SSRF-guard-off
/// client that may reach the loopback mocks.
pub(super) fn test_state(pool: PgPool) -> AppState {
    test_state_with_config(pool, Config::for_tests())
}

/// The state under test over an explicit config (the size-cap test
/// needs a small `inbox_max_bytes`).
pub(super) fn test_state_with_config(pool: PgPool, config: Config) -> AppState {
    AppState::for_tests(pool, config)
}

/// The router under test: one `POST /inbox` gated by the extractor.
pub(super) fn inbox_app(state: AppState) -> Router {
    Router::new()
        .route("/inbox", post(echo_activity))
        .with_state(state)
}

/// The success arm: 200 JSON naming the signer and echoing the body,
/// proving the extractor delivered all three fields.
async fn echo_activity(activity: SignedActivity) -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "signer": activity.signer.nickname,
            "actor": activity.json.get("actor"),
            "type": activity.json.get("type"),
            "bytes": activity.body.len(),
        })),
    )
        .into_response()
}

/// The `Person` actor document a peer serves for `id`, vouching for
/// `public_pem` as its signing key.
fn actor_document(id: &str, public_pem: &str, key_id: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Person",
        "id": id,
        "preferredUsername": "alice",
        "name": "Alice",
        "inbox": format!("{id}/inbox"),
        "publicKey": {
            "id": key_id,
            "owner": id,
            "publicKeyPem": public_pem,
        },
    })
}

/// The `ActivityPub` JSON answer of a mock peer.
fn actor_response(document: &Value) -> Response {
    (
        [(header::CONTENT_TYPE, "application/activity+json")],
        document.to_string(),
    )
        .into_response()
}

/// Mock peer state: its origin, the fetch counter every route bumps,
/// and the public key the served actor carries.
#[derive(Clone, Debug)]
pub(super) struct Peer {
    /// The `http://{addr}` origin of this mock server.
    pub(super) base: String,
    /// Requests answered, across all routes.
    pub(super) hits: Arc<AtomicUsize>,
    /// The public key the actor document carries.
    pub(super) public_pem: String,
    /// Whether the actor names its key path-style (`{id}/main-key`, as
    /// `GoToSocial` does) instead of with a `#main-key` fragment.
    pub(super) path_style_key: bool,
}

/// A peer at `base` serving `public_pem`.
pub(super) fn peer(base: String, public_pem: String) -> Peer {
    Peer {
        base,
        hits: Arc::new(AtomicUsize::new(0)),
        public_pem,
        path_style_key: false,
    }
}

/// A peer at `base` whose actor names its key `{id}/main-key`, the
/// `GoToSocial` shape no fragment strip maps back to the actor.
pub(super) fn path_key_peer(base: String, public_pem: String) -> Peer {
    Peer {
        path_style_key: true,
        ..peer(base, public_pem)
    }
}

/// Counts the hit and answers the self-identifying actor document.
///
/// Also answers at the path-style key URL, as `GoToSocial` does: the document
/// names its own `id`, so the fetcher follows it.
async fn serve_actor(State(peer): State<Peer>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    let id = format!("{}/users/alice", peer.base);
    let key_id = if peer.path_style_key {
        format!("{id}/main-key")
    } else {
        format!("{id}#main-key")
    };
    actor_response(&actor_document(&id, &peer.public_pem, &key_id))
}

/// Counts the hit and answers the standalone key object whose `owner`
/// names this peer's actor: the `keyId` shape that is not an actor.
async fn serve_key_object(State(peer): State<Peer>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    actor_response(&json!({
        "@context": "https://w3id.org/security/v1",
        "id": format!("{}/keys/1", peer.base),
        "type": "Key",
        "owner": format!("{}/users/alice", peer.base),
        "publicKeyPem": peer.public_pem,
    }))
}

/// Answers the configured status and nothing else (410, 404).
async fn status_only(State(status): State<StatusCode>) -> StatusCode {
    status
}

/// A mock peer answering `/users/alice` (actor), `/keys/1` (key
/// object), `/gone` (410) and `/missing` (404). Routes with different
/// state types get their state erased and the pieces merged, the
/// pattern the [`crate::federation::fetch_actor`] tests use.
pub(super) fn peer_app(peer: Peer) -> Router {
    let actor_peer = peer.clone();
    Router::new()
        .route("/users/alice", get(serve_actor))
        .route("/users/alice/main-key", get(serve_actor))
        .with_state(actor_peer)
        .merge(
            Router::new()
                .route("/keys/1", get(serve_key_object))
                .with_state(peer),
        )
        .merge(
            Router::new()
                .route("/gone", get(status_only))
                .with_state(StatusCode::GONE),
        )
        .merge(
            Router::new()
                .route("/missing", get(status_only))
                .with_state(StatusCode::NOT_FOUND),
        )
}

/// Inserts a `reject` policy row directly (this table has no writer
/// until the admin tickets, like its own tests).
pub(super) async fn block_domain(pool: &PgPool, domain: &str) {
    let inserted = sqlx::query(
        "INSERT INTO instance_policy_domains (domain, action, reason) VALUES ($1, 'reject', '')",
    )
    .bind(domain)
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(inserted.rows_affected(), 1, "the policy row is there");
}

/// Enables the `simple` policy with the domain lists `lists` configure:
/// the row the pipeline reads as its policy, beside the table the
/// extractor also consults.
pub(super) async fn set_mrf_lists(pool: &PgPool, lists: &Value) {
    runtime_config::write(
        pool,
        ":pleroma",
        ":mrf",
        &json!({"policies": ["simple"], "simple": lists}),
    )
    .await
    .unwrap();
}

/// Seeds a remote mirror row for actor `ap_id` holding `public_pem`:
/// the stale-mirror state the rotation and throttle tests start from.
pub(super) async fn seed_remote_user(pool: &PgPool, ap_id: &str, public_pem: &str) {
    let new = NewUser {
        nickname: "alice@127.0.0.1".to_owned(),
        ap_id: ap_id.to_owned(),
        domain: Some("127.0.0.1".to_owned()),
        name: "Alice".to_owned(),
        inbox: Some(format!("{ap_id}/inbox")),
        public_key: Some(public_pem.to_owned()),
        ..NewUser::default()
    };
    let _seeded: User = insert(pool, &new).await.unwrap();
}

/// A delivering peer's `POST`: the body and every header the fediverse
/// signs, built by [`sign_request`] so the extractor sees the real
/// shape. Tests tamper with the fields before [`send`]-ing them.
#[derive(Debug)]
pub(super) struct Delivery {
    /// The body bytes, as sent (not necessarily as signed).
    pub(super) body: Vec<u8>,
    /// The headers, in send order: content-type, host, date, digest,
    /// signature.
    pub(super) headers: Vec<(&'static str, String)>,
}

/// Signs `body` as a `POST` to [`INBOX_URL`] with `key_id`.
pub(super) fn deliver(key_id: &str, private_pem: &str, body: &[u8]) -> Delivery {
    let signed = sign_request(
        "POST",
        INBOX_URL,
        Some(body),
        &SigningKey {
            key_id,
            private_pem,
        },
        Utc::now(),
    )
    .unwrap();
    let digest = signed.digest.clone().unwrap();
    Delivery {
        body: body.to_vec(),
        headers: vec![
            ("content-type", "application/activity+json".to_owned()),
            ("host", signed.host.clone()),
            ("date", signed.date.clone()),
            ("digest", digest),
            ("signature", signed.signature),
        ],
    }
}

/// Swaps one header value of a [`Delivery`] (the tampering tests).
pub(super) fn replace_header(delivery: &mut Delivery, name: &str, value: String) {
    let slot = delivery
        .headers
        .iter_mut()
        .find(|(existing, _)| *existing == name)
        .unwrap_or_else(|| panic!("the test delivery carries a {name} header"));
    slot.1 = value;
}

/// Drops one header from a [`Delivery`].
pub(super) fn drop_header(delivery: &mut Delivery, name: &str) {
    let index = delivery
        .headers
        .iter()
        .position(|(existing, _)| *existing == name)
        .unwrap_or_else(|| panic!("the test delivery carries a {name} header"));
    let _removed = delivery.headers.remove(index);
}

/// The value of one header of a [`Delivery`].
pub(super) fn header_value(delivery: &Delivery, name: &str) -> String {
    for (existing, value) in &delivery.headers {
        if *existing == name {
            return value.clone();
        }
    }
    panic!("the test delivery carries a {name} header");
}

/// The Follow activity `actor` posts of `object`.
pub(super) fn activity(actor: &str, object: &str) -> Vec<u8> {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Follow",
        "actor": actor,
        "object": object,
    })
    .to_string()
    .into_bytes()
}

/// The `Delete` by `actor` of that same actor.
pub(super) fn self_delete(actor: &str) -> Vec<u8> {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Delete",
        "actor": actor,
        "object": actor,
    })
    .to_string()
    .into_bytes()
}

/// The extractor's answer, fully buffered.
#[derive(Debug)]
pub(super) struct Answer {
    /// The HTTP status.
    pub(super) status: StatusCode,
    /// The `{"error": …}` body message; empty unless there is one.
    pub(super) error: String,
    /// The whole decoded body, `Null` when it is not JSON.
    pub(super) json: Value,
}

/// Drives one [`Delivery`] through the real router over `oneshot`.
pub(super) async fn send(app: &Router, delivery: &Delivery) -> Answer {
    let mut builder = Request::post("/inbox");
    for (name, value) in &delivery.headers {
        let parsed = HeaderName::from_str(name).unwrap();
        builder = builder.header(parsed, HeaderValue::from_str(value).unwrap());
    }
    let request = builder.body(Body::from(delivery.body.clone())).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let error = json
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Answer {
        status,
        error,
        json,
    }
}
