//! Sample HTTP integration test: the pattern every later endpoint ticket
//! copies (`docs/ARCHITECTURE.md` decision 13 and the “Writing tests”
//! section). Checks `/health` and the JSON fallback, then exercises the
//! `tests/common/mod.rs` harness itself against a local echo route.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Json, Router, extract::Request, http::StatusCode, routing::any};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::config::{BASE_URL_VAR, DATABASE_URL_VAR, TEST_BASE_URL, TEST_DATABASE_URL};

/// `/health` answers 204 No Content with an empty body.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn health_returns_no_content(pool: PgPool) {
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/health").send(&app).await;
    assert_eq!(response.status, StatusCode::NO_CONTENT, "health status");
    assert!(response.body.is_empty(), "health body is empty");
}

/// A path no route matches gets the standard JSON error response
/// (decision 8), not an empty body.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn unknown_route_returns_json_not_found(pool: PgPool) {
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/nope-not-a-route")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND, "404 status");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "errors answer with JSON"
    );
    let error = response
        .json_value()
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_owned);
    assert_eq!(
        error.as_deref(),
        Some("Not found"),
        "unmatched paths are the router-level 404 (see tests/api_skeleton.rs)"
    );
}

/// A router-level verb mismatch is a 405 from axum, even with the JSON
/// fallback installed for unknown paths.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn wrong_verb_on_health_gets_method_not_allowed(pool: PgPool) {
    let app = common::test_app(pool);
    let post_response = common::TestRequest::post("/health").send(&app).await;
    assert_eq!(
        post_response.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "POST is not routed on /health"
    );
    let delete_response = common::TestRequest::delete("/health").send(&app).await;
    assert_eq!(
        delete_response.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "DELETE is not routed on /health"
    );
    assert_eq!(delete_response.text(), "", "the 405 body is empty");
}

/// `test_app` answers the health check with the middleware stack active,
/// proving it is the real `wired_be::app`.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn test_app_serves_the_middleware_stack(pool: PgPool) {
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/health").send(&app).await;
    assert_eq!(response.status, StatusCode::NO_CONTENT, "health status");
    assert!(
        response.header("x-request-id").is_some(),
        "the middleware stack adds the request id"
    );
}

/// Echo route for the harness self-check: reports the request method, the
/// headers the builders set, and the body back as JSON.
async fn echo(request: Request) -> Json<Value> {
    let method = request.method().clone();
    let headers = request.headers().clone();
    let body = axum::body::to_bytes(request.into_body(), 1024)
        .await
        .unwrap_or_default();
    let header_text = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    Json(json!({
        "method": method.as_str(),
        "content-type": header_text("content-type"),
        "authorization": header_text("authorization"),
        "x-probe": header_text("x-probe"),
        "body": String::from_utf8_lossy(&body),
    }))
}

/// A router over the echo handler, for testing the harness without a
/// database.
fn echo_app() -> Router {
    Router::new().route("/echo", any(echo))
}

/// Typed view of the echo response, exercising [`TestResponse::json`].
#[derive(Debug, Deserialize)]
struct EchoSeen {
    /// The method the handler was called with.
    method: String,
    /// The received `content-type` header, or empty.
    #[serde(rename = "content-type")]
    content_type: String,
    /// The received `authorization` header, or empty.
    authorization: String,
    /// The received `x-probe` header, or empty.
    #[serde(rename = "x-probe")]
    probe: String,
    /// The request body, decoded as text.
    body: String,
}

/// `put` + `json` + `bearer` + `header`, checked at the handler through the
/// `get`/typed `json` response accessors.
#[tokio::test]
async fn json_body_and_headers_reach_the_handler() {
    let app = echo_app();
    let response = common::TestRequest::put("/echo")
        .json(&json!({"page": 2}))
        .bearer("tok3n")
        .header("x-probe", "yes")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "echo accepted PUT");
    let seen: EchoSeen = response.json();
    assert_eq!(seen.method, "PUT", "the verb builder reached the handler");
    assert_eq!(
        seen.content_type, "application/json",
        "json() sets the content type"
    );
    assert_eq!(
        seen.authorization, "Bearer tok3n",
        "bearer() adds the authorization header"
    );
    assert_eq!(seen.probe, "yes", "header() adds custom headers");
    assert_eq!(seen.body, r#"{"page":2}"#, "json() serializes the fixture");
}

/// `patch` + `form`: the pairs arrive percent-encoded.
#[tokio::test]
async fn form_body_reaches_the_handler() {
    let app = echo_app();
    let response = common::TestRequest::patch("/echo")
        .form(&[("query", "hello world&more"), ("lang", "en")])
        .send(&app)
        .await;
    let seen: EchoSeen = response.json();
    assert_eq!(
        seen.method, "PATCH",
        "the patch() builder reached the handler"
    );
    assert_eq!(
        seen.content_type, "application/x-www-form-urlencoded",
        "form() sets the content type"
    );
    assert_eq!(
        seen.body, "query=hello+world%26more&lang=en",
        "form() percent-encodes the pairs"
    );
}

/// A bare `get` sends no body and no custom headers.
#[tokio::test]
async fn empty_get_carries_no_body_or_headers() {
    let app = echo_app();
    let response = common::TestRequest::get("/echo").send(&app).await;
    assert_eq!(response.status, StatusCode::OK, "echo accepted GET");
    let seen: EchoSeen = response.json();
    assert_eq!(seen.method, "GET", "the default GET verb is sent");
    assert_eq!(seen.body, "", "no body is sent");
    assert_eq!(seen.content_type, "", "json() was not called");
    assert_eq!(seen.authorization, "", "bearer() was not called");
    assert_eq!(seen.probe, "", "header() was not called");
}

/// [`test_config`](common::test_config) is the documented fixed
/// configuration, built without any environment access.
#[test]
fn test_config_is_the_fixed_configuration() {
    let config = common::test_config();
    let expected = wired_be::config::Config::from_lookup(|key| match key {
        DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
        BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
        _ => None,
    })
    .expect("the fixed test values are valid");
    assert_eq!(config, expected, "test_config equals the fixed lookup");
    assert_eq!(config.domain(), "test.example", "test domain");
}
