//! The fixtures the [`HttpClient`](super::HttpClient) children's unit
//! tests share: the loopback mock-server harness (bind, URL, spawn and
//! header reader), the small request-echoing handlers the routes serve
//! and the constants they assert against. Test-only.

use std::time::Duration;

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Method, StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use tokio::{net::TcpListener, task::JoinHandle};

/// `User-Agent` used by clients that only exercise request handling.
pub(super) const TEST_UA: &str = "Wired (https://wired.test)";
/// A 100-byte body served by the `/big` route.
pub(super) const BIG_BODY: [u8; 100] = [b'x'; 100];
/// Total timeout small enough to let a sleeping handler lose the race.
pub(super) const SHORT_TIMEOUT: Duration = Duration::from_millis(150);

/// Binds an unused loopback port.
pub(super) async fn bind() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.unwrap()
}

/// The `path` on `listener` as an absolute `http` URL.
pub(super) fn url_for(listener: &TcpListener, path: &str) -> String {
    format!("http://{}{path}", listener.local_addr().unwrap())
}

/// Serves `app` on `listener` until the test runtime shuts down.
pub(super) fn spawn_server(listener: TcpListener, app: Router) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("test server stopped: {err}");
        }
    })
}

/// The visible text of header `name`, empty when absent or not visible.
fn header_text(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// Answers with the request's own `user-agent` header.
pub(super) async fn echo_user_agent(headers: HeaderMap) -> String {
    header_text(&headers, "user-agent")
}

/// Answers with the request's own `x-marker` header.
pub(super) async fn echo_marker(headers: HeaderMap) -> String {
    header_text(&headers, "x-marker")
}

/// Answers with the fixed 100-byte [`BIG_BODY`].
pub(super) async fn big() -> &'static [u8] {
    &BIG_BODY
}

/// Answers with 2 KiB of body whatever the method.
pub(super) async fn two_kib() -> Vec<u8> {
    vec![b'y'; 2048]
}

/// Sleeps far past [`SHORT_TIMEOUT`] before answering.
pub(super) async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_secs(30)).await;
    "late"
}

/// Echoes the request body back verbatim.
pub(super) async fn echo_body(body: Bytes) -> Bytes {
    body
}

/// Answers with a 301 to the absolute URL carried in state.
pub(super) async fn hop(State(location): State<String>) -> Response {
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header(header::LOCATION, location)
        .body(Body::empty())
        .unwrap()
}

/// The redirect destination: proves the client followed the hop.
pub(super) async fn redirect_target() -> &'static str {
    "followed"
}

/// Answers with the request's own method name.
pub(super) async fn echo_method(method: Method) -> String {
    method.as_str().to_owned()
}
