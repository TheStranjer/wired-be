//! Tests for the `MongooseIM` auth endpoints and the small utility
//! endpoints of `src/api/misc.rs` (WBE-166): each endpoint's success
//! shape, its disabled branch (the fallback 404), the credential
//! failures of `check_password`, the healthcheck's unhealthy path over a
//! closed pool, the frontend-configurations document, and the exact PWA
//! manifest body.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    config::Toggle,
    models::{
        instance_setting,
        user::{Flag, NewUser},
    },
};

/// The real router with the `MongooseIM` auth endpoints enabled.
fn mongooseim_app(pool: PgPool) -> Router {
    let mut config = common::test_config();
    config.mongooseim_enabled = Toggle::new(true);
    common::test_app_with_config(pool, config)
}

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

// ------------------------------------------------------------ user_exists

/// With `MONGOOSEIM_ENABLED` unset (the default), both auth endpoints are
/// the fallback 404, exact body included.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn mongooseim_endpoints_are_absent_when_disabled(pool: PgPool) {
    let app = common::test_app(pool);
    for path in [
        "/user_exists?user=alice",
        "/check_password?user=alice&pass=x",
    ] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path} status");
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "{path} is JSON"
        );
        assert_eq!(response.text(), NOT_FOUND_BODY, "{path} body");
    }
}

/// `GET /user_exists?user=<nick>` answers an active local user 200 with
/// JSON `true`, and the citext nickname answers case-insensitively:
/// create the fixture, then mount the enabled router, then ask both
/// spellings.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn user_exists_round_trip(pool: PgPool) {
    let _alice = common::create_local_user(&pool, "alice").await;
    let app = mongooseim_app(pool);
    for nickname in ["alice", "ALICE"] {
        let response = common::TestRequest::get(&format!("/user_exists?user={nickname}"))
            .send(&app)
            .await;
        assert_eq!(response.status, StatusCode::OK, "{nickname} exists");
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "the answer is JSON"
        );
        assert_eq!(response.text(), "true", "{nickname} body");
    }
}

/// Everything that is not an active local user gets 404 with JSON
/// `false`: an unknown nickname, a deactivated account, a remote mirror
/// and a request without the `user` parameter.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn user_exists_false_cases(pool: PgPool) {
    let _suspended = common::create_local_user_with(&pool, "suspended", |user| NewUser {
        is_active: Flag::new(false),
        ..user
    })
    .await;
    let _robo = common::create_remote_user(&pool, "robo", "remote.example").await;
    let app = mongooseim_app(pool);
    let cases = [
        ("/user_exists?user=nobody", "unknown nickname"),
        ("/user_exists?user=suspended", "deactivated account"),
        ("/user_exists?user=robo@remote.example", "remote mirror"),
        ("/user_exists", "missing parameter"),
    ];
    for (path, why) in cases {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{why} status");
        assert_eq!(response.text(), "false", "{why} body");
    }
}

// ---------------------------------------------------------- check_password

/// `GET /check_password?user=<nick>&pass=<password>` answers the right
/// credential for an active local user 200 with JSON `true`.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn check_password_true_for_valid_credentials(pool: PgPool) {
    let _gina = common::create_user_with_password(&pool, "gina", "a good password").await;
    let app = mongooseim_app(pool);
    let response = common::TestRequest::get("/check_password?user=gina&pass=a%20good%20password")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "valid credentials");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the answer is JSON"
    );
    assert_eq!(response.text(), "true", "the true answer");
}

/// Every failed credential is one uniform denial: 403 with JSON `false`
/// — the wrong password, an unknown user, a deactivated account with its
/// right password, an account with no password at all, the email instead
/// of the nickname (`MongooseIM` authenticates handles), and either
/// parameter missing. The disabled branch is covered by
/// [`mongooseim_endpoints_are_absent_when_disabled`].
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn check_password_denies_every_bad_credential(pool: PgPool) {
    let _gina = common::create_user_with_password(&pool, "gina", "a good password").await;
    let _gone =
        common::create_deactivated_user_with_password(&pool, "gone", "still-my-password").await;
    let _passless = common::create_local_user(&pool, "passless").await;
    let app = mongooseim_app(pool);
    let cases = [
        (
            "/check_password?user=gina&pass=wrong%20password",
            "wrong password",
        ),
        ("/check_password?user=nobody&pass=x", "unknown user"),
        (
            "/check_password?user=gone&pass=still-my-password",
            "deactivated account",
        ),
        (
            "/check_password?user=passless&pass=whatever",
            "no stored hash",
        ),
        (
            "/check_password?user=gina@example.org&pass=a%20good%20password",
            "email instead of nickname",
        ),
        ("/check_password?user=gina", "missing pass"),
        ("/check_password?pass=x", "missing user"),
        ("/check_password", "missing both"),
    ];
    for (path, why) in cases {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{why} status");
        assert_eq!(response.text(), "false", "{why} body");
    }
}

// ------------------------------------------------------------------ captcha

/// With no captcha configured, `GET /api/v1/pleroma/captcha` answers
/// exactly `{"type":"none"}`, Pleroma's provider-less document.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn captcha_reports_none_when_disabled(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn captcha_is_not_implemented_when_enabled(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn healthcheck_is_absent_when_disabled(pool: PgPool) {
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

/// The enabled healthcheck over the live `#[sqlx::test]` pool answers 200
/// with the ticket's five keys; the probe succeeded, so `healthy` is
/// `true` and memory is the unreported `0.0`.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn healthcheck_reports_a_healthy_database(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn healthcheck_is_unhealthy_over_a_closed_pool(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn frontend_configurations_default_to_the_empty_object(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn frontend_configurations_serves_the_stored_document(pool: PgPool) {
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

/// The exact manifest body for the default test configuration, content
/// type included.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn manifest_is_the_ticket_document(pool: PgPool) {
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/manifest.json").send(&app).await;
    assert_eq!(response.status, StatusCode::OK, "manifest status");
    assert_eq!(
        response.header("content-type"),
        Some("application/manifest+json"),
        "the manifest media type"
    );
    assert_eq!(
        response.text(),
        r##"{"name":"Wired","short_name":"Wired","description":"A Pleroma-compatible instance","display":"standalone","scope":"/","start_url":"/","icons":[{"src":"/static/logo.svg","sizes":"any","type":"image/svg+xml"}],"theme_color":"#282c37","background_color":"#191b22"}"##,
        "the exact manifest body"
    );
}

/// The manifest's `name`, `short_name` and `description` follow the
/// instance configuration.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn manifest_follows_the_instance_config(pool: PgPool) {
    let mut config = common::test_config();
    config.instance_name = "Snatter".to_owned();
    config.instance_description = "Where we all chatter".to_owned();
    let app = common::test_app_with_config(pool, config);
    let response = common::TestRequest::get("/manifest.json").send(&app).await;
    assert_eq!(response.status, StatusCode::OK, "manifest status");
    let body = response.json_value();
    assert_eq!(body["name"], json!("Snatter"), "name follows config");
    assert_eq!(
        body["short_name"],
        json!("Snatter"),
        "short_name follows config"
    );
    assert_eq!(
        body["description"],
        json!("Where we all chatter"),
        "description follows config"
    );
    assert_eq!(body["display"], json!("standalone"), "fixed display");
    assert_eq!(
        body["icons"],
        json!([{"src": "/static/logo.svg", "sizes": "any", "type": "image/svg+xml"}]),
        "the one logo icon"
    );
}
