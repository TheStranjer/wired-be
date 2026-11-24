//! Tests for the small utility endpoints of `src/api/misc.rs` (WBE-166):
//! each endpoint's success shape and disabled branch (the fallback 404),
//! the healthcheck's unhealthy path over a closed pool, the
//! frontend-configurations document, the removed XMPP-server auth
//! endpoints and the removed PWA manifest route's fallback 404.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{config::Toggle, models::instance_setting};

use crate::common;

/// The real router with `CAPTCHA_ENABLED` set to `enabled`.
fn captcha_app(pool: PgPool, enabled: bool) -> Router {
    let mut config = common::test_config();
    config.captcha_enabled = Toggle::new(enabled);
    common::test_app_with_config(pool, config)
}

/// The real router with the healthcheck enabled.
fn healthcheck_app(pool: PgPool) -> Router {
    let mut config = common::test_config();
    config.healthcheck_enabled = Toggle::new(true);
    common::test_app_with_config(pool, config)
}

/// The exact body of the standard fallback 404.
const NOT_FOUND_BODY: &str = r#"{"error":"Not found"}"#;

// ------------------------------------------------- removed auth endpoints

/// The XMPP-server auth endpoints are gone: a real user with the correct
/// password still gets the app-wide fallback 404, exact body and
/// `x-wired-fallback: 1` marker included.
#[tokio::test]
async fn removed_auth_endpoints_are_the_fallback_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_user_with_password(&pool, "alice", "a good password").await;
    let app = common::test_app(pool);
    for path in [
        "/user_exists?user=alice",
        "/check_password?user=alice&pass=a%20good%20password",
    ] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path} status");
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "{path} is JSON"
        );
        assert_eq!(response.text(), NOT_FOUND_BODY, "{path} body");
        assert_eq!(
            response.header("x-wired-fallback"),
            Some("1"),
            "{path} is the fallback's own 404"
        );
    }
}

// ------------------------------------------------------------------ captcha

/// With no captcha configured, `GET /api/v1/pleroma/captcha` answers
/// exactly `{"type":"none"}`, Pleroma's provider-less document.
#[tokio::test]
async fn captcha_reports_none_when_disabled() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = captcha_app(pool, false);
    let response = common::TestRequest::get("/api/v1/pleroma/captcha")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "captcha status");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the answer is JSON"
    );
    assert_eq!(response.text(), r#"{"type":"none"}"#, "the none body");
}

/// With `CAPTCHA_ENABLED` set, the endpoint says so: 501 with the
/// standard error body (no captcha implementation exists to serve).
#[tokio::test]
async fn captcha_is_not_implemented_when_enabled() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = captcha_app(pool, true);
    let response = common::TestRequest::get("/api/v1/pleroma/captcha")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_IMPLEMENTED,
        "configured captcha status"
    );
    assert_eq!(
        response.text(),
        r#"{"error":"Captcha is not implemented"}"#,
        "the 501 error body"
    );
}

// --------------------------------------------------------------- healthcheck

/// With `HEALTHCHECK_ENABLED` unset, the endpoint is the fallback 404
/// (Pleroma answers 503 "Healthcheck disabled"; the ticket routes it out
/// of existence instead).
#[tokio::test]
async fn healthcheck_is_absent_when_disabled() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    // Contrast: an unrouted path under `/api` gets "Not implemented"…
    let typo = common::TestRequest::get("/api/v1/plerma-nothing")
        .send(&app)
        .await;
    assert_eq!(typo.status, StatusCode::NOT_FOUND, "typo path 404");
    assert_eq!(
        typo.text(),
        r#"{"error":"Not implemented"}"#,
        "the API fallback message for an unrouted path"
    );
    // …while the disabled healthcheck answers the plain "Not found".
    let response = common::TestRequest::get("/api/v1/pleroma/healthcheck")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "disabled healthcheck status"
    );
    assert_eq!(response.text(), NOT_FOUND_BODY, "the fallback 404 body");
}

/// The enabled healthcheck over the live leased slot answers 200
/// with the ticket's five keys; the probe succeeded, so `healthy` is
/// `true` and memory is the unreported `0.0`.
#[tokio::test]
async fn healthcheck_reports_a_healthy_database() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = healthcheck_app(pool);
    let response = common::TestRequest::get("/api/v1/pleroma/healthcheck")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "healthy status");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the answer is JSON"
    );
    let body = response.json_value();
    assert_eq!(body["healthy"], json!(true), "the probe succeeded");
    assert_eq!(body["memory_used"], json!(0.0), "memory is not measured");
    for key in ["pool_size", "active", "idle"] {
        assert!(body[key].is_number(), "{key} is a number: {body}");
    }
    // A parsed `Value` map sorts keys, so key order is checked on the raw
    // wire text: the five ticket keys each appear after the one before.
    let text = response.text();
    let ordered = [
        "\"pool_size\":",
        "\"active\":",
        "\"idle\":",
        "\"memory_used\":",
        "\"healthy\":",
    ];
    let positions: Vec<usize> = ordered.iter().filter_map(|key| text.find(key)).collect();
    assert_eq!(
        positions.len(),
        ordered.len(),
        "every ticket key is present: {text}"
    );
    assert!(
        positions
            .windows(2)
            .all(|pair| matches!(pair, [before, after] if before < after)),
        "the keys appear in the ticket's order: {text}"
    );
}

/// The unhealthy path: close the pool underneath the router, and the
/// `SELECT 1` probe fails, so the same body answers 503 with
/// `healthy: false`.
#[tokio::test]
async fn healthcheck_is_unhealthy_over_a_closed_pool() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = healthcheck_app(pool.clone());
    pool.close().await;
    let response = common::TestRequest::get("/api/v1/pleroma/healthcheck")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "closed pool status"
    );
    let body = response.json_value();
    assert_eq!(body["healthy"], json!(false), "the failed probe");
    assert_eq!(body["memory_used"], json!(0.0), "memory is not measured");
    for key in ["pool_size", "active", "idle"] {
        assert!(body[key].is_number(), "{key} is a number: {body}");
    }
}

// --------------------------------------------------- frontend_configurations

/// `GET /api/pleroma/frontend_configurations` answers the empty object
/// while no setting row exists.
#[tokio::test]
async fn frontend_configurations_default_to_the_empty_object() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/api/pleroma/frontend_configurations")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "status");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the answer is JSON"
    );
    assert_eq!(response.text(), "{}", "unset means the empty object");
}

/// The endpoint serves exactly the document stored under the
/// `frontend_configurations` key.
#[tokio::test]
async fn frontend_configurations_serves_the_stored_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let stored = json!({
        "mobile": {"features": {"notifications": true}},
        "enableReactions": ["😄"],
        "theme": "dark"
    });
    instance_setting::set(&pool, "frontend_configurations", &stored)
        .await
        .expect("the setting writes");
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/api/pleroma/frontend_configurations")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "status");
    let body: Value = response.json();
    assert_eq!(body, stored, "the stored document comes back verbatim");
}

// ---------------------------------------------------------------- manifest

/// The backend serves no PWA manifest (the frontend owns it): the path is the
/// app-wide fallback's 404, marked `x-wired-fallback: 1`.
#[tokio::test]
async fn manifest_json_is_the_fallback_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/manifest.json").send(&app).await;
    assert_eq!(response.status, StatusCode::NOT_FOUND, "manifest status");
    assert_eq!(
        response.json_value(),
        json!({"error": "Not found"}),
        "the standard not-found body"
    );
    assert_eq!(
        response.header("x-wired-fallback"),
        Some("1"),
        "the fallback marks its own 404"
    );
}
