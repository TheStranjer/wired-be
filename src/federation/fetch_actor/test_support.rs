//! The mock-peer fixtures the fetch children's tests build on: the test
//! states and loopback clients, the recording peer and the actor handlers
//! it serves, and the status, redirect and staleness helpers. Test-only.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse as _, Response},
};
use reqwest::header::CONTENT_TYPE;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpListener, task::JoinHandle};

use super::ACTOR_ACCEPT;
use crate::{config::Config, http_client::HttpClient, state::AppState};

/// `User-Agent` for the test clients.
const TEST_UA: &str = "Wired (https://test.example)";
/// The content type a federation peer answers an actor document with.
pub(super) const ACTOR_CONTENT_TYPE: &str = "application/activity+json";

/// The state under test: `pool` on the default test config.
pub(super) fn test_state(pool: PgPool) -> AppState {
    test_state_with_config(pool, Config::for_tests())
}

/// The state under test: `pool`, `config`, and a client whose SSRF
/// guard is off, so it may reach the loopback mocks.
pub(super) fn test_state_with_config(pool: PgPool, config: Config) -> AppState {
    AppState {
        http_client: HttpClient::new_for_tests(TEST_UA),
        ..AppState::for_tests(pool, config)
    }
}

/// The test-mode client (guard off, so `127.0.0.1` mocks are allowed).
pub(super) fn client() -> HttpClient {
    HttpClient::new_for_tests(TEST_UA)
}

/// Binds an unused loopback port.
pub(super) async fn bind() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

/// The `http://{addr}` origin a listener answers on.
pub(super) fn origin(listener: &TcpListener) -> String {
    format!("http://{}", listener.local_addr().unwrap())
}

/// The `host:port` authority of an `http://{addr}` origin — what the
/// mirror's `user@host` nickname and `domain` column now carry, port
/// included (decision 7, and the handle a `WebFinger` round trip on a
/// loopback peer resolves to).
pub(super) fn authority_of(base: &str) -> &str {
    base.split_once("://").map_or(base, |(_scheme, rest)| rest)
}

/// Serves `app` on `listener` until the test aborts the task.
pub(super) fn spawn(listener: TcpListener, app: Router) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("test server stopped: {err}");
        }
    })
}

/// A minimal actor document for `id`, the way a Mastodon peer serves
/// one: everything [`parse_actor`](crate::federation::remote_actor::parse_actor) requires is present.
pub(super) fn actor_document(id: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Person",
        "id": id,
        "preferredUsername": "alice",
        "name": "First",
        "inbox": format!("{id}/inbox"),
        "publicKey": {
            "id": format!("{id}#main-key"),
            "owner": id,
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
        },
    })
}

/// A body labelled [`ACTOR_CONTENT_TYPE`].
fn json_response(document: &Value) -> Response {
    ([(CONTENT_TYPE, ACTOR_CONTENT_TYPE)], document.to_string()).into_response()
}

/// The state every mock-peer handler runs with: its own origin plus
/// the counters and captures the test asserts on.
#[derive(Clone, Debug)]
pub(super) struct Peer {
    /// The `http://{addr}` origin of this mock server.
    pub(super) base: String,
    /// Requests this peer answered, across all its routes.
    pub(super) hits: Arc<AtomicUsize>,
    /// The headers of the most recent request, when one was recorded.
    pub(super) captured: Arc<Mutex<Option<HeaderMap>>>,
}

/// A mock peer at `base` with zeroed counters.
pub(super) fn peer_at(base: &str) -> Peer {
    Peer {
        base: base.to_owned(),
        hits: Arc::new(AtomicUsize::new(0)),
        captured: Arc::new(Mutex::new(None)),
    }
}

/// Counts the hit and answers the self-identifying actor document.
pub(super) async fn self_actor(State(peer): State<Peer>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    let id = format!("{}/users/alice", peer.base);
    json_response(&actor_document(&id))
}

/// Like [`self_actor`], but every answer after the first carries a
/// changed display name, so tests can tell a refetch from a mirror.
pub(super) async fn versioned_actor(State(peer): State<Peer>) -> Response {
    let hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    let id = format!("{}/users/alice", peer.base);
    let mut document = actor_document(&id);
    if hits > 0 {
        document["name"] = json!("Second");
    }
    json_response(&document)
}

/// Like [`self_actor`], but first recording the request headers for
/// the signature assertions.
pub(super) async fn self_actor_recording(State(peer): State<Peer>, headers: HeaderMap) -> Response {
    let _previous = peer.captured.lock().unwrap().replace(headers.clone());
    self_actor(State(peer)).await
}

/// Answers the self-identifying document only when the request asked
/// with the exact [`ACTOR_ACCEPT`], so reaching the document proves
/// the header.
pub(super) async fn self_actor_checking_accept(
    State(peer): State<Peer>,
    headers: HeaderMap,
) -> Response {
    if headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept == ACTOR_ACCEPT)
    {
        return self_actor(State(peer)).await;
    }
    StatusCode::NOT_ACCEPTABLE.into_response()
}

/// Answer with the configured status.
pub(super) async fn status_only(State(status): State<StatusCode>) -> StatusCode {
    status
}

/// The actor document's `id` moved to `{base}/moved/alice`.
pub(super) async fn moved_actor(State(peer): State<Peer>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    let id = format!("{}/moved/alice", peer.base);
    json_response(&actor_document(&id))
}

/// The document at `{base}/moved/alice` naming a third id: the claim
/// keeps moving and containment must reject it.
pub(super) async fn moving_on_actor(State(peer): State<Peer>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    let id = format!("{}/elsewhere/alice", peer.base);
    json_response(&actor_document(&id))
}

/// The document at `{base}/moved/alice` that names itself: the
/// refetched anchor a moved document must land on.
pub(super) async fn moved_home_actor(State(peer): State<Peer>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    let id = format!("{}/moved/alice", peer.base);
    json_response(&actor_document(&id))
}

/// The `/users/alice` document a `/actor` request is redirected to,
/// naming the URL the client asked for originally.
pub(super) async fn redirect_target_actor(State(peer): State<Peer>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    let id = format!("{}/actor", peer.base);
    json_response(&actor_document(&id))
}

/// A 301 to this peer's own `/users/alice`.
pub(super) async fn hop(State(peer): State<Peer>) -> Response {
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header(header::LOCATION, format!("{}/users/alice", peer.base))
        .body(axum::body::Body::empty())
        .unwrap()
}

/// A 301 to the `Location` held in the route's state.
pub(super) async fn redirect_to(State(location): State<String>) -> Response {
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header(header::LOCATION, location)
        .body(axum::body::Body::empty())
        .unwrap()
}

/// Moves a stored mirror's fetch stamp outside the refresh window.
pub(super) async fn make_stale(pool: &PgPool, ap_id: &str) {
    sqlx::query("UPDATE users SET last_fetched_at = now() - interval '25 hours' WHERE ap_id = $1")
        .bind(ap_id)
        .execute(pool)
        .await
        .map(drop)
        .unwrap();
}

/// A peer serving an actor document that carries the profile images
/// the removal lists are about: an `icon` and an `image`.
pub(super) async fn profiled_actor(State(peer): State<Peer>) -> Response {
    let id = format!("{}/users/alice", peer.base);
    let mut document = actor_document(&id);
    let _icon = document
        .as_object_mut()
        .expect("the fixture document is an object")
        .insert(
            "icon".to_owned(),
            json!({"type": "Image", "url": format!("{id}/avatar.png")}),
        );
    let _image = document
        .as_object_mut()
        .expect("the fixture document is an object")
        .insert(
            "image".to_owned(),
            json!({"type": "Image", "url": format!("{id}/banner.png")}),
        );
    json_response(&document)
}

/// The self-identifying document with the required username removed.
pub(super) async fn not_an_actor(State(peer): State<Peer>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    let mut document = actor_document(&format!("{}/users/alice", peer.base));
    drop(
        document
            .as_object_mut()
            .unwrap()
            .remove("preferredUsername"),
    );
    json_response(&document)
}

/// The JRD pointing at this peer's actor path.
pub(super) async fn webfinger(State(body): State<String>) -> Response {
    ([(CONTENT_TYPE, "application/jrd+json")], body).into_response()
}
