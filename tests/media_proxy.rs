//! End-to-end tests for the media proxy route (`GET
//! /proxy/{sig}/{url}[/{filename}]` and its `/proxy/preview/…` twin):
//! signature validation (403 `{"error":"Invalid signature"}`), streaming
//! of the upstream bytes through a mock upstream on loopback (a local
//! mini-server, reached because the test config disables the SSRF
//! guard), the media-only content-type gate (424), the
//! `MEDIA_PROXY_MAX_BYTES` abort (413), `Range` pass-through (206),
//! `If-None-Match` pass-through (304), the upstream status mapping
//! (404/5xx), and the disabled passthrough (404).

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, StatusCode, header},
    response::Response,
    routing::get,
};
use sqlx::PgPool;
use tokio::net::TcpListener;
use wired_be::{config::Config, services::media_proxy};

/// The fake image bytes the mock upstream serves.
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n-proxy-test";
/// The body `ranged` answers a `bytes=0-1` request with.
const HELLO: &[u8] = b"hello";
/// The `ETag` the revalidating route answers with.
const ETAG: &str = "\"v1\"";
/// The CSP the proxy must stamp on every served response.
const CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; media-src 'self' data:; img-src 'self' data:";

/// Bound the mock upstream, serve it, and return its origin URL.
async fn spawn_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Dropping the handle detaches the task: the mini-server lives for
    // the rest of the test runtime, which is exactly what the proxy
    // needs it to do.
    let _server: tokio::task::JoinHandle<()> = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, upstream_app()).await {
            tracing::debug!("mock upstream stopped: {err}");
        }
    });
    format!("http://{addr}")
}

/// The mock upstream: media, non-media, oversized, missing, failing,
/// range-aware and revalidation-aware routes.
fn upstream_app() -> Router {
    Router::new()
        .route("/cat.png", get(cat_png))
        .route("/clip.webm", get(clip_webm))
        .route("/note.txt", get(note_txt))
        .route("/big.png", get(big_png))
        .route("/missing.png", get(missing))
        .route("/boom.png", get(boom))
        .route("/hello.bin", get(ranged))
        .route("/dog.gif", get(dog_gif))
}

/// Build the upstream response for `status`, `content_type` and `body`.
fn serve(status: StatusCode, content_type: &str, body: &[u8]) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body.to_vec()))
        .unwrap()
}

/// `image/png` media.
async fn cat_png() -> Response {
    serve(StatusCode::OK, "image/png", PNG_BYTES)
}

/// `video/webm` media.
async fn clip_webm() -> Response {
    serve(StatusCode::OK, "video/webm", b"webm-bytes")
}

/// Not media: the proxy must refuse this.
async fn note_txt() -> Response {
    serve(StatusCode::OK, "text/plain", b"not media")
}

/// Media far past any small cap the tests configure.
async fn big_png() -> Response {
    serve(StatusCode::OK, "image/png", &[b'x'; 4096])
}

/// A server-side failure.
async fn boom() -> Response {
    serve(
        StatusCode::INTERNAL_SERVER_ERROR,
        "text/html",
        b"<h1>500</h1>",
    )
}

/// A missing resource.
async fn missing() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// `HELLO` honouring `Range: bytes=0-1` with a `206` and `Content-Range`.
async fn ranged(headers: HeaderMap) -> Response {
    match headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        Some("bytes=0-1") => Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_TYPE, "image/png")
            .header(header::CONTENT_RANGE, "bytes 0-1/5")
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::ETAG, ETAG)
            .body(Body::from(&HELLO[..2]))
            .unwrap(),
        _ => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/png")
            .header(header::ETAG, ETAG)
            .body(Body::from(HELLO))
            .unwrap(),
    }
}

/// A media resource revalidated against `ETag: "v1"`.
async fn dog_gif(headers: HeaderMap) -> Response {
    let revalidated = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(ETAG);
    if revalidated {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, ETAG)
            .body(Body::empty())
            .unwrap();
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/gif")
        .header(header::ETAG, ETAG)
        .body(Body::from(b"gif-bytes".as_slice()))
        .unwrap()
}

/// The proxy path (signed link with the instance origin stripped) for
/// `url`, as `config`'s media proxy would hand it to clients.
fn proxy_path(config: &Config, url: &str) -> String {
    let link = media_proxy::proxy_url(config, url);
    let path = link
        .strip_prefix(wired_be::config::TEST_BASE_URL)
        .unwrap_or_else(|| panic!("the link should hang off the test origin: {link}"));
    path.to_owned()
}

/// The full signed link for `url` with the given proxy prefix.
fn proxy_path_with_prefix(config: &Config, url: &str, prefix: &str) -> String {
    let path = proxy_path(config, url);
    path.replacen("/proxy/", prefix, 1)
}

/// Proxied media arrives with its bytes and the full set of safety
/// headers, and the preview path serves exactly the same bytes.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn proxied_media_serves_bytes_with_safety_headers(pool: PgPool) {
    let upstream = spawn_upstream().await;
    let config = common::test_media_proxy_config(50_000_000);
    let app = common::test_app_with_config(pool, config.clone());
    let path = proxy_path(&config, &format!("{upstream}/cat.png"));

    let response = common::TestRequest::get(&path).send(&app).await;
    assert_eq!(response.status, StatusCode::OK, "proxied image");
    assert_eq!(response.body, PNG_BYTES, "the upstream bytes arrive");
    assert_eq!(response.header("content-type"), Some("image/png"));
    assert_eq!(response.header("content-disposition"), Some("inline"));
    assert_eq!(
        response.header("cache-control"),
        Some("public, max-age=86400"),
        "a day of cache"
    );
    assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(
        response.header("content-security-policy"),
        Some(CSP),
        "the locked-down CSP"
    );

    let preview =
        proxy_path_with_prefix(&config, &format!("{upstream}/cat.png"), "/proxy/preview/");
    let preview_response = common::TestRequest::get(&preview).send(&app).await;
    assert_eq!(preview_response.status, StatusCode::OK, "preview status");
    assert_eq!(
        preview_response.body, PNG_BYTES,
        "preview serves the same bytes"
    );
}

/// Video and audio types pass the media gate as images do.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn video_media_passes_the_content_type_gate(pool: PgPool) {
    let upstream = spawn_upstream().await;
    let config = common::test_media_proxy_config(50_000_000);
    let app = common::test_app_with_config(pool, config.clone());
    let path = proxy_path(&config, &format!("{upstream}/clip.webm"));
    let response = common::TestRequest::get(&path).send(&app).await;
    assert_eq!(response.status, StatusCode::OK, "video passes");
    assert_eq!(response.header("content-type"), Some("video/webm"));
}

/// A tampered signature is refused with 403 and the ticket's JSON body.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn tampered_signature_is_forbidden(pool: PgPool) {
    let upstream = spawn_upstream().await;
    let config = common::test_media_proxy_config(50_000_000);
    let app = common::test_app_with_config(pool, config.clone());
    let path = proxy_path(&config, &format!("{upstream}/cat.png"));
    let (_, tail) = path.split_once("/proxy/").unwrap();
    let mut segments = tail.split('/');
    let sig = segments.next().unwrap().to_owned();
    let encoded = segments.next().unwrap();
    let last = sig.chars().last().unwrap();
    let tampered = format!(
        "{}{}",
        sig.strip_suffix(last).unwrap(),
        if last == 'A' { 'B' } else { 'A' }
    );

    let response = common::TestRequest::get(&format!("/proxy/{tampered}/{encoded}/cat.png"))
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN, "403 status");
    assert_eq!(
        response.json_value()["error"],
        serde_json::json!("Invalid signature"),
        "the ticket's 403 body, and nothing was fetched"
    );

    let unknown = common::TestRequest::get("/proxy/not-base64!!/also-not-base64!!")
        .send(&app)
        .await;
    assert_eq!(
        unknown.status,
        StatusCode::FORBIDDEN,
        "junk segments cannot verify either"
    );
}

/// A non-media upstream type is refused with the ticket's 424 body.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn unsupported_content_type_is_424(pool: PgPool) {
    let upstream = spawn_upstream().await;
    let config = common::test_media_proxy_config(50_000_000);
    let app = common::test_app_with_config(pool, config.clone());
    let path = proxy_path(&config, &format!("{upstream}/note.txt"));

    let response = common::TestRequest::get(&path).send(&app).await;
    assert_eq!(
        response.status,
        StatusCode::FAILED_DEPENDENCY,
        "424 for text/plain"
    );
    assert_eq!(
        response.json_value()["error"],
        serde_json::json!("Unsupported media type"),
        "the ticket's 424 body"
    );
}

/// A body past `MEDIA_PROXY_MAX_BYTES` aborts the fetch (413).
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn oversized_bodies_abort_with_413(pool: PgPool) {
    let upstream = spawn_upstream().await;
    let config = common::test_media_proxy_config(8);
    let app = common::test_media_proxy_app(pool, 8);
    let path = proxy_path(&config, &format!("{upstream}/big.png"));

    let response = common::TestRequest::get(&path).send(&app).await;
    assert_eq!(
        response.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "one 4096-byte body against an 8-byte cap"
    );
    assert_eq!(
        response.json_value()["error"],
        serde_json::json!("Media exceeds the size limit"),
        "the cap names itself"
    );
}

/// `Range` requests reach the upstream and the `206` answer arrives.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn range_requests_pass_through(pool: PgPool) {
    let upstream = spawn_upstream().await;
    let config = common::test_media_proxy_config(50_000_000);
    let app = common::test_app_with_config(pool, config.clone());
    let path = proxy_path(&config, &format!("{upstream}/hello.bin"));

    let partial = common::TestRequest::get(&path)
        .header("range", "bytes=0-1")
        .send(&app)
        .await;
    assert_eq!(partial.status, StatusCode::PARTIAL_CONTENT, "206");
    assert_eq!(partial.body, &HELLO[..2], "only the requested slice");
    assert_eq!(
        partial.header("content-range"),
        Some("bytes 0-1/5"),
        "the range header passes through"
    );
    assert_eq!(partial.header("etag"), Some(ETAG), "validators pass");

    let whole = common::TestRequest::get(&path).send(&app).await;
    assert_eq!(
        whole.status,
        StatusCode::OK,
        "unganged requests get everything"
    );
    assert_eq!(whole.body, HELLO, "the full body");
}

/// `If-None-Match` reaches the upstream and the `304` answer arrives.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn conditional_requests_pass_through_as_304(pool: PgPool) {
    let upstream = spawn_upstream().await;
    let config = common::test_media_proxy_config(50_000_000);
    let app = common::test_app_with_config(pool, config.clone());
    let path = proxy_path(&config, &format!("{upstream}/dog.gif"));

    let fresh = common::TestRequest::get(&path).send(&app).await;
    assert_eq!(fresh.status, StatusCode::OK, "first fetch");
    assert_eq!(fresh.header("etag"), Some(ETAG), "the etag rides along");

    let revalidated = common::TestRequest::get(&path)
        .header("if-none-match", ETAG)
        .send(&app)
        .await;
    assert_eq!(revalidated.status, StatusCode::NOT_MODIFIED, "304");
    assert!(revalidated.body.is_empty(), "a 304 has no body");
    assert_eq!(
        revalidated.header("etag"),
        Some(ETAG),
        "the validated etag comes back"
    );
    assert_eq!(
        revalidated.header("cache-control"),
        Some("public, max-age=86400"),
        "and so do the proxy headers"
    );
}

/// Upstream 404 answers 404 (the "same status" half of the mapping).
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn upstream_404_passes_through(pool: PgPool) {
    let upstream = spawn_upstream().await;
    let config = common::test_media_proxy_config(50_000_000);
    let app = common::test_app_with_config(pool, config.clone());
    let path = proxy_path(&config, &format!("{upstream}/missing.png"));

    let response = common::TestRequest::get(&path).send(&app).await;
    assert_eq!(response.status, StatusCode::NOT_FOUND, "404 stays 404");
    assert_eq!(
        response.json_value()["error"],
        serde_json::json!("Record not found"),
        "the standard JSON error body"
    );
}

/// Upstream 5xx and unreachable upstreams both answer 502 (the "or 502"
/// half of the mapping).
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn upstream_failures_answer_502(pool: PgPool) {
    let upstream = spawn_upstream().await;
    let config = common::test_media_proxy_config(50_000_000);
    let app = common::test_app_with_config(pool, config.clone());
    let boom_path = proxy_path(&config, &format!("{upstream}/boom.png"));
    let response = common::TestRequest::get(&boom_path).send(&app).await;
    assert_eq!(
        response.status,
        StatusCode::BAD_GATEWAY,
        "an upstream 500 becomes a 502"
    );
    assert_eq!(
        response.json_value()["error"],
        serde_json::json!("Upstream media request failed"),
        "the 502 body"
    );

    let gone = proxy_path(&config, "http://127.0.0.1:1/refused.png");
    let refused = common::TestRequest::get(&gone).send(&app).await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_GATEWAY,
        "a refused connection is also a 502"
    );
}

/// With the proxy disabled (the default), no link can verify and every
/// `/proxy` request is a 404 — and entity URLs pass through, which the
/// existing suites' snapshots already prove.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn disabled_proxy_answers_not_found(pool: PgPool) {
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/proxy/anysig/anyencoded.png")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND, "404 while disabled");
    assert_eq!(
        response.json_value()["error"],
        serde_json::json!("Record not found"),
        "the standard JSON 404"
    );
}

/// Sanity check for the harness helpers themselves: the signed path a
/// test builds decodes back to the upstream URL it names.
#[test]
fn generated_paths_decode_back_to_their_urls() {
    let config = common::test_media_proxy_config(1024);
    let url = "https://remote.example/media/cat.gif";
    let path = proxy_path(&config, url);
    let (_, tail) = path.split_once("/proxy/").unwrap();
    let mut segments = tail.split('/');
    let sig = segments.next().unwrap();
    let encoded = segments.next().unwrap();
    assert_eq!(
        media_proxy::decode(&config, sig, encoded).unwrap(),
        url,
        "the harness builds real links"
    );
}
