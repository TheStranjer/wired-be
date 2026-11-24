//! The fixtures the [`HttpClient`](super::HttpClient) children's unit
//! tests share: the loopback mock-server harness (bind, URL, spawn and
//! header reader), the small request-echoing handlers the routes serve,
//! the constants they assert against, and the helpers that read a
//! peer-observed `Signature` header back apart. Test-only.

use std::time::Duration;

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::Response,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bytes::Bytes;
use serde_json::Value;
use tokio::{net::TcpListener, task::JoinHandle};

use crate::federation::signature::signing_string;

/// `User-Agent` used by clients that only exercise request handling.
pub(super) const TEST_UA: &str = "Wired (https://wired.test)";
/// The key id the signed-request tests present.
pub(super) const TEST_KEY_ID: &str = "https://wired.test/actor#main-key";
/// The `headers` parameter of a signed bodyless GET.
pub(super) const GET_HEADERS_PARAM: &str = "(request-target) host date";
/// The `headers` parameter of a signed POST (with digest).
pub(super) const POST_HEADERS_PARAM: &str = "(request-target) host date digest";
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

/// Reports what a federation peer would see: the request target as
/// `(request-target)` is signed, plus the header values a signed
/// request must carry (absent keys are omitted).
pub(super) async fn echo_ap_request(method: Method, uri: Uri, headers: HeaderMap) -> Json<Value> {
    let target = format!(
        "{} {}",
        method.as_str().to_lowercase(),
        uri.query().map_or_else(
            || uri.path().to_owned(),
            |query| format!("{}?{query}", uri.path()),
        ),
    );
    let mut seen = serde_json::Map::new();
    drop(seen.insert("request-target".to_owned(), Value::String(target)));
    for name in [
        "accept",
        "content-type",
        "host",
        "date",
        "digest",
        "signature",
    ] {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            drop(seen.insert(name.to_owned(), Value::String(value.to_owned())));
        }
    }
    Json(Value::Object(seen))
}

/// The named parameter of a `Signature` header value, unquoted.
fn signature_param(header: &str, name: &str) -> String {
    let prefix = format!("{name}=\"");
    let pair = header
        .split(',')
        .find(|p| p.starts_with(&prefix))
        .unwrap_or_else(|| panic!("signature header carries a {name} parameter: {header}"));
    let value = pair
        .strip_prefix(prefix.as_str())
        .unwrap_or_else(|| panic!("the {name} parameter is quoted: {header}"));
    value.trim_end_matches('"').to_owned()
}

/// The raw signature bytes out of a peer-observed `Signature` header,
/// checking the other three parameters name this test's key, the
/// `rsa-sha256` algorithm and `headers_param`.
pub(super) fn observed_signature(header: &str, headers_param: &str) -> Vec<u8> {
    assert_eq!(
        signature_param(header, "keyId"),
        TEST_KEY_ID,
        "the signature names our key id"
    );
    assert_eq!(
        signature_param(header, "algorithm"),
        "rsa-sha256",
        "the algorithm Mastodon and Pleroma speak"
    );
    assert_eq!(
        signature_param(header, "headers"),
        headers_param,
        "the signed header list"
    );
    BASE64.decode(signature_param(header, "signature")).unwrap()
}

/// The four lines a peer can rebuild for us, as the text the signature
/// must verify over.
pub(super) fn peer_signing_string(seen: &Value) -> String {
    let mut lines = vec![
        ("(request-target)", seen["request-target"].as_str().unwrap()),
        ("host", seen["host"].as_str().unwrap()),
        ("date", seen["date"].as_str().unwrap()),
    ];
    if let Some(digest) = seen.get("digest").and_then(Value::as_str) {
        lines.push(("digest", digest));
    }
    signing_string(&lines)
}
