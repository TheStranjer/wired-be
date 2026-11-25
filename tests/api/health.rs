//! Sample HTTP integration test: the pattern every later endpoint ticket
//! copies (`docs/ARCHITECTURE.md` decision 13 and the “Writing tests”
//! section). Checks `/health` and the JSON fallback, then exercises the
//! `tests/api/common.rs` harness itself against a local echo route.

#![cfg(test)]

use axum::{Json, Router, extract::Request, http::StatusCode, routing::any};
use serde::Deserialize;
use serde_json::{Value, json};
use wired_be::config::{
    BASE_URL_VAR, DATABASE_URL_VAR, HTTP_ALLOW_PRIVATE_HOSTS_VAR, RATE_LIMITS_ENABLED_VAR,
    REGISTRATIONS_OPEN_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
};

use crate::common;

/// `/health` answers 204 No Content with an empty body.
#[tokio::test]
async fn health_returns_no_content() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/health").send(&app).await;
    assert_eq!(response.status, StatusCode::NO_CONTENT, "health status");
    assert!(response.body.is_empty(), "health body is empty");
}

/// A path no route matches gets the standard JSON error response
/// (decision 8), not an empty body.
#[tokio::test]
async fn unknown_route_returns_json_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
        "unmatched paths are the router-level 404 (see tests/api/api_skeleton.rs)"
    );
}

/// `GET /mailer/unsubscribe/{token}` is not routed (the page was removed
/// with `SECRET_KEY_BASE`): the fallback alone answers 404, and no page
/// is served.
#[tokio::test]
async fn the_mailer_unsubscribe_page_is_answered_by_the_fallback() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/mailer/unsubscribe/abc.def")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "the unsubscribe page is gone"
    );
    assert_eq!(
        response.header("x-wired-fallback"),
        Some("1"),
        "only the fallback answers the path"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the answer is the JSON error, not an HTML page"
    );
}

/// The retired status embed card and player are not routed either: the
/// fallback answers 404 for both.
#[tokio::test]
async fn the_retired_embed_pages_are_answered_by_the_fallback() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    for path in ["/embed/1", "/notice/1/embed_player?url=x"] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path} is gone");
        assert_eq!(
            response.header("x-wired-fallback"),
            Some("1"),
            "only the fallback answers {path}"
        );
        assert_eq!(
            response.header("x-frame-options"),
            None,
            "no framable page is served at {path}"
        );
    }
}

/// A router-level verb mismatch is a 405 from axum, even with the JSON
/// fallback installed for unknown paths.
#[tokio::test]
async fn wrong_verb_on_health_gets_method_not_allowed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn test_app_serves_the_middleware_stack() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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

/// `multipart`: the boundary is announced by the `content-type` and every part
/// is framed the way a `FormData` body frames it — text fields without a file
/// name, file parts with a `filename` and their own `content-type`.
#[tokio::test]
async fn multipart_body_reaches_the_handler() {
    let app = echo_app();
    let parts = [
        common::MultipartPart::text("description", "a cat"),
        common::MultipartPart::file(
            "file",
            "cat.png",
            "image/png",
            axum::body::Bytes::from_static(b"magic-bytes"),
        ),
    ];
    let response = common::TestRequest::post("/echo")
        .multipart("wired-echo-boundary", &parts)
        .send(&app)
        .await;
    let seen: EchoSeen = response.json();
    assert_eq!(seen.method, "POST", "the verb builder reached the handler");
    assert_eq!(
        seen.content_type, "multipart/form-data; boundary=wired-echo-boundary",
        "multipart() announces the body and its boundary"
    );
    assert_eq!(
        seen.body,
        "--wired-echo-boundary\r\n\
         Content-Disposition: form-data; name=\"description\"\r\n\
         \r\n\
         a cat\r\n\
         --wired-echo-boundary\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"cat.png\"\r\n\
         Content-Type: image/png\r\n\
         \r\n\
         magic-bytes\r\n\
         --wired-echo-boundary--\r\n",
        "the parts are framed with their headers and closed off"
    );
}

/// `multipart()` with no parts is a body that holds only the closing boundary,
/// which is how an empty form arrives.
#[tokio::test]
async fn multipart_without_parts_sends_an_empty_form() {
    let app = echo_app();
    let response = common::TestRequest::post("/echo")
        .multipart("empty-boundary", &[])
        .send(&app)
        .await;
    let seen: EchoSeen = response.json();
    assert_eq!(
        seen.body, "--empty-boundary--\r\n",
        "the body is the closed-off form alone"
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
        HTTP_ALLOW_PRIVATE_HOSTS_VAR | REGISTRATIONS_OPEN_VAR => Some("true".to_owned()),
        RATE_LIMITS_ENABLED_VAR => Some("false".to_owned()),
        _ => None,
    })
    .expect("the fixed test values are valid");
    assert_eq!(config, expected, "test_config equals the fixed lookup");
    assert!(
        config.fetch.http_allow_private_hosts,
        "the harness disables the SSRF guard so integration tests can reach local servers"
    );
    assert!(
        !config.rate_limit.enabled.get(),
        "the harness runs unthrottled so a suite never trips a limiter of its own"
    );
    assert_eq!(config.domain(), "test.example", "test domain");
}

/// [`create_local_user`](common::create_local_user) inserts a local,
/// passwordless row through the model layer, with the test instance's
/// actor URL, and the row is readable back case-insensitively.
#[tokio::test]
async fn create_local_user_fixture_inserts_a_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = common::create_local_user(&pool, "tess").await;
    assert_eq!(user.nickname, "tess", "the nickname is stored as given");
    assert!(user.local.get(), "the fixture builds a local account");
    assert!(user.password_hash.is_none(), "the fixture sets no password");
    assert_eq!(
        user.ap_id,
        format!("{TEST_BASE_URL}/users/tess"),
        "the actor id is local_ap_id on the test instance"
    );

    let fetched = wired_be::models::user::get_by_nickname(&pool, "TESS")
        .await
        .expect("the lookup reaches the database")
        .expect("the fixture row exists");
    assert_eq!(
        fetched.id, user.id,
        "citext finds the fixture case-insensitively"
    );
}

/// [`create_user_with_password`](common::create_user_with_password) inserts
/// a local user whose Argon2id hash and derived email let
/// [`authenticate`](wired_be::models::user::register::authenticate) find it
/// by either identifier, and only by the right password.
#[tokio::test]
async fn create_user_with_password_fixture_authenticates() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = common::create_user_with_password(&pool, "gina", "a good password").await;
    assert!(user.local.get(), "the fixture builds a local account");
    assert_eq!(
        user.email.as_deref(),
        Some("gina@example.org"),
        "the derived email"
    );
    let hash = user
        .password_hash
        .clone()
        .expect("the fixture sets a password");
    assert!(hash.starts_with("$argon2id$"), "the hash is Argon2id");
    assert_eq!(
        user.ap_id,
        format!("{TEST_BASE_URL}/users/gina"),
        "the actor id is local_ap_id on the test instance"
    );

    let logged_in =
        wired_be::models::user::register::authenticate(&pool, "gina", "a good password")
            .await
            .expect("the nickname logs the fixture in");
    assert_eq!(logged_in.id, user.id, "the row itself");
    assert!(
        wired_be::models::user::register::authenticate(
            &pool,
            "GINA@EXAMPLE.ORG",
            "a good password"
        )
        .await
        .is_some(),
        "the citext email logs the fixture in too"
    );
    assert!(
        wired_be::models::user::register::authenticate(&pool, "gina", "wrong password")
            .await
            .is_none(),
        "the wrong password never logs in"
    );
}
