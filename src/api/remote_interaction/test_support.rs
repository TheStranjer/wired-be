//! The unit-test fixtures the remote-interaction children share: the
//! `WebFinger` documents the mock peers answer with, the loopback server
//! that serves them, the subscribe lookup driven against it, the test
//! state whose client may reach loopback, the stored local actor and the
//! response reader. Test-only.

use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse as _, Response},
    routing::get,
};
use sqlx::PgPool;
use tokio::{net::TcpListener, task::JoinHandle};

use crate::{
    config::{Config, TEST_BASE_URL},
    http_client::HttpClient,
    models::user::{NewUser, User, insert, local_ap_id},
    state::AppState,
    uploads::UploaderBackend,
};

use super::interaction::subscribe_url_for;

/// `User-Agent` for the test client.
pub(super) const TEST_UA: &str = "Wired (https://test.example)";
/// The local actor URL the tests expand into the templates.
pub(super) const AP_ID: &str = "https://test.example/users/alice";
/// [`AP_ID`] as [`expand_subscribe_url`] encodes it.
///
/// [`expand_subscribe_url`]: crate::api::remote_interaction::interaction::expand_subscribe_url
pub(super) const AP_ID_ENCODED: &str = "https%3A%2F%2Ftest.example%2Fusers%2Falice";
/// The remote handle every mock answers about.
pub(super) const PROFILE: &str = "bob@remote.example";
/// The interaction URL of the [`JRD_SUBSCRIBE`] mock.
pub(super) const WANT_URL: &str =
    "https://remote.example/ostatus_subscribe?acct=https%3A%2F%2Ftest.example%2Fusers%2Falice";
/// A JRD answer advertising a subscribe template for [`PROFILE`].
pub(super) const JRD_SUBSCRIBE: &str = r#"{"subject": "acct:bob@remote.example", "links": [
    {"rel": "self", "type": "application/activity+json",
     "href": "https://remote.example/users/bob"},
    {"rel": "http://ostatus.org/schema/1.0/subscribe",
     "template": "https://remote.example/ostatus_subscribe?acct={uri}"}
]}"#;
/// The same answer with the percent-encoded macro spelling.
pub(super) const JRD_ENCODED_MACRO: &str = r#"{"links": [
    {"rel": "http://ostatus.org/schema/1.0/subscribe",
     "template": "https://remote.example/follow?uri=%7Buri%7D"}
]}"#;
/// An XRD answer advertising a subscribe template.
pub(super) const XRD_SUBSCRIBE: &str = r#"<XRD xmlns="http://docs.oasis-open.org/ns/xri/xrd-1.0">
    <Subject>acct:bob@remote.example</Subject>
    <Link rel="http://ostatus.org/schema/1.0/subscribe"
          template="https://remote.example/ostatus_subscribe?acct={uri}"/>
</XRD>"#;
/// An answer whose subscribe link carries no template.
pub(super) const JRD_NO_TEMPLATE: &str = r#"{"links": [
    {"rel": "http://ostatus.org/schema/1.0/subscribe"}
]}"#;
/// An answer with a subscribe link whose template has no macro.
pub(super) const JRD_NO_MACRO: &str = r#"{"links": [
    {"rel": "http://ostatus.org/schema/1.0/subscribe",
     "template": "https://remote.example/follow"}
]}"#;
/// An answer with no subscribe link at all.
pub(super) const JRD_NO_SUBSCRIBE: &str = r#"{"links": [
    {"rel": "self", "type": "application/activity+json",
     "href": "https://remote.example/users/bob"}
]}"#;

/// The client under test: the SSRF guard off, so loopback mocks are
/// reachable (the mode `HTTP_ALLOW_PRIVATE_HOSTS` selects).
pub(super) fn client() -> HttpClient {
    HttpClient::new_for_tests(TEST_UA)
}

/// A router answering `/.well-known/webfinger` with `body`, labelled
/// `content_type`.
pub(super) fn webfinger_route(content_type: &'static str, body: &'static str) -> Router {
    /// Answer with the configured document.
    async fn handler(State((content_type, body)): State<(String, String)>) -> Response {
        ([(header::CONTENT_TYPE, content_type)], body).into_response()
    }
    Router::new()
        .route("/.well-known/webfinger", get(handler))
        .with_state((content_type.to_owned(), body.to_owned()))
}

/// A router answering every path with `status`.
pub(super) fn status_route(status: StatusCode) -> Router {
    /// Reply with the configured status.
    async fn handler(State(status): State<StatusCode>) -> StatusCode {
        status
    }
    Router::new()
        .route("/{*rest}", get(handler))
        .with_state(status)
}

/// Serves `app` on a loopback port; returns its task handle (to abort
/// when the test is done) and the `http://` base URL to hand the
/// lookup as the scheme-and-authority override.
pub(super) async fn serve(app: Router) -> (JoinHandle<()>, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("test server stopped: {err}");
        }
    });
    (server, base)
}

/// The subscribe URL `target` expands to against `app`'s answer for
/// [`PROFILE`], served on loopback.
pub(super) async fn url_against(app: Router, target: &str) -> Option<String> {
    let (server, base) = serve(app).await;
    let url = subscribe_url_for(&client(), target, PROFILE, Some(&base)).await;
    server.abort();
    url
}

/// Test state over `pool`: the fixed test config with a client whose
/// SSRF guard is off, so the subscribe lookup may reach the loopback
/// mock (the shape [`crate::api::search`]'s tests build).
pub(super) fn test_state(pool: &PgPool) -> AppState {
    let config = std::sync::Arc::new(Config::for_tests());
    AppState {
        db: pool.clone(),
        hub: crate::streaming::hub::Hub::new(pool),
        config: std::sync::Arc::clone(&config),
        http_client: HttpClient::new_for_tests(TEST_UA),
        uploader: UploaderBackend::from(config.as_ref()),
        instance_stats_cache: std::sync::Arc::default(),
        mrf_config_cache: std::sync::Arc::default(),
        translation_languages_cache: std::sync::Arc::default(),
        rate_limiter: std::sync::Arc::new(crate::rate_limit::RateLimiter::from_config(
            config.as_ref(),
        )),
        mailer: std::sync::Arc::new(crate::mailer::MemoryTransport::new()),
    }
}

/// A state over a pool that never connects: the JSON endpoint reads
/// only the HTTP client, so no slot is spent on it.
pub(super) fn client_state() -> AppState {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused")
        .unwrap();
    test_state(&pool)
}

/// The local `alice` whose actor id is [`AP_ID`], inserted straight.
pub(super) async fn local_alice(pool: &PgPool) -> User {
    insert(
        pool,
        &NewUser::local("alice", &local_ap_id(TEST_BASE_URL, "alice")),
    )
    .await
    .expect("the fixture local user inserts")
}

/// The full text of a response body.
pub(super) async fn body_of(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}
