//! The unit-test fixtures the search children share: the parsed
//! parameters of a raw query string, the state over a test slot whose
//! client may reach loopback, the stored users, and the mock `WebFinger`
//! peer with its request counters. Test-only.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Router,
    extract::{Path, Query, State},
    http::header::CONTENT_TYPE,
    response::{IntoResponse as _, Response},
    routing::get,
};
use serde_json::json;
use sqlx::PgPool;
use tokio::{net::TcpListener, task::JoinHandle};

use crate::{
    config::{Config, TEST_BASE_URL},
    http_client::HttpClient,
    models::user::{NewUser, User, insert, local_ap_id},
    state::AppState,
    uploads::UploaderBackend,
};

use super::params::SearchParams;
/// The parsed parameters of `query`.
///
/// # Panics
///
/// If the fixed test query fails to parse (a malformed cursor a test
/// names as such never reaches here).
pub(super) fn parse(query: &str) -> SearchParams {
    SearchParams::parse(Some(query)).expect("the fixed test query parses")
}

/// `User-Agent` for the mock-reachable client.
const TEST_UA: &str = "Wired (https://test.example)";

/// Test state over `pool`: the fixed test config with a client whose SSRF
/// guard is off, so the handle lookup may reach the loopback mock.
pub(super) fn test_state(pool: &PgPool) -> AppState {
    let config = Arc::new(Config::for_tests());
    AppState {
        db: pool.clone(),
        hub: crate::streaming::hub::Hub::new(pool),
        config: Arc::clone(&config),
        http_client: HttpClient::new_for_tests(TEST_UA),
        uploader: UploaderBackend::from(config.as_ref()),
        instance_stats_cache: Arc::default(),
        mrf_config_cache: Arc::default(),
        translation_languages_cache: Arc::default(),
        rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
        mailer: Arc::new(crate::mailer::MemoryTransport::new()),
    }
}

/// A local user named `nickname`, inserted straight.
pub(super) async fn local_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
    )
    .await
    .expect("the fixture local user inserts")
}

/// A stored remote mirror of `nickname` on `host`, the shape decision 7
/// keeps: the nickname carries the whole handle.
pub(super) async fn mirror_user(pool: &PgPool, nickname: &str, host: &str) -> User {
    insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@{host}"),
            ap_id: format!("https://{host}/users/{nickname}"),
            domain: Some(host.to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the fixture mirror inserts")
}

/// The mock remote peer: its `http://` origin beside the per-endpoint
/// request counts the tests assert on.
#[derive(Clone, Debug)]
pub(super) struct Mock {
    /// The peer's origin.
    pub(super) base: String,
    /// `WebFinger` requests it received.
    pub(super) webfinger_hits: Arc<AtomicUsize>,
    /// Actor dereferences it received.
    pub(super) actor_hits: Arc<AtomicUsize>,
}

/// Serves `WebFinger` and one actor document per nickname on a loopback
/// port; the returned task is aborted by the test once its assertions are
/// in.
pub(super) async fn mock_peer() -> (Mock, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the mock peer binds a port");
    let mock = Mock {
        base: format!("http://{}", listener.local_addr().expect("a bound address")),
        webfinger_hits: Arc::new(AtomicUsize::new(0)),
        actor_hits: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/.well-known/webfinger", get(mock_webfinger))
        .route("/users/{nick}", get(mock_actor))
        .with_state(mock.clone());
    let server = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("mock peer stopped: {err}");
        }
    });
    (mock, server)
}

/// [`mock_peer`]'s `WebFinger` arm: a `JRD` whose self link points at this
/// peer's own `/users/{nick}`, under a subject that vouches for the
/// `{nick}@mock.example` handle the resolver was asked about.
async fn mock_webfinger(
    State(mock): State<Mock>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let _hits = mock.webfinger_hits.fetch_add(1, Ordering::SeqCst);
    let nickname = params
        .get("resource")
        .and_then(|resource| resource.strip_prefix("acct:"))
        .and_then(|handle| handle.split_once('@').map(|(nick, _)| nick))
        .unwrap_or_else(|| panic!("the mock was fingered for no acct: {params:?}"));
    let jrd = json!({
        "subject": format!("acct:{nickname}@mock.example"),
        "links": [{
            "rel": "self",
            "type": "application/activity+json",
            "href": format!("{}/users/{nickname}", mock.base),
        }],
    });
    ([(CONTENT_TYPE, "application/jrd+json")], jrd.to_string()).into_response()
}

/// [`mock_peer`]'s actor arm: the self-identifying `Person` document
/// [`parse_actor`] requires.
///
/// [`parse_actor`]: crate::federation::remote_actor::parse_actor
async fn mock_actor(State(mock): State<Mock>, Path(nick): Path<String>) -> Response {
    let _hits = mock.actor_hits.fetch_add(1, Ordering::SeqCst);
    let id = format!("{}/users/{nick}", mock.base);
    let document = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Person",
        "id": id,
        "preferredUsername": nick,
        "name": "Mock",
        "inbox": format!("{id}/inbox"),
        "publicKey": {
            "id": format!("{id}#main-key"),
            "owner": id,
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
        },
    });
    (
        [(CONTENT_TYPE, "application/activity+json")],
        document.to_string(),
    )
        .into_response()
}

/// The origin of a port nothing listens on any more: every request to it
/// fails to connect, which is how an unreachable peer reads here without
/// waiting on any network or timer.
pub(super) async fn dead_base() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port is free to bind");
    let base = format!("http://{}", listener.local_addr().expect("a bound address"));
    drop(listener);
    base
}

/// The hits one mock counter has recorded.
pub(super) fn hits(counter: &AtomicUsize) -> usize {
    counter.load(Ordering::SeqCst)
}
