//! The account-settings and misc split under each `PROTOCOLS` value: the
//! credential and departure routes and the instance utilities are core and
//! answer with only `fixture` enabled, while the aliases and move routes are
//! `ActivityPub`-backed and answer the app-wide fallback there.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::json;
use wired_be::{
    config::{
        BASE_URL_VAR, Config, DATABASE_URL_VAR, PROTOCOLS_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
    },
    util::password::verify_password,
};

use crate::common;

/// Password the test account logs in with.
const PASSWORD: &str = "correct horse battery staple";
/// The password `change_password` sets (past the default minimum length).
const NEW_PASSWORD: &str = "a-brand-new-secret";
/// The scopes of the token every test here mints.
const SCOPES: &str = "read:accounts write:accounts";

/// The test configuration with `PROTOCOLS` set to `protocols`.
///
/// # Panics
///
/// If `protocols` is not a valid `PROTOCOLS` value; the tests pass fixed ones.
fn config_with_protocols(protocols: &str) -> Config {
    Config::from_lookup(|key| match key {
        DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
        BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
        PROTOCOLS_VAR => Some(protocols.to_owned()),
        _ => None,
    })
    .expect("the fixed protocols test config is valid")
}

/// Whether `response` came from the app-wide fallback rather than a route.
fn is_fallback(response: &common::TestResponse) -> bool {
    response.header("x-wired-fallback") == Some("1")
}

/// The `ActivityPub`-backed routes of the area, as a method and a path each.
const ACTIVITYPUB_BACKED: [(&str, &str); 5] = [
    ("GET", "/api/pleroma/aliases"),
    ("PUT", "/api/pleroma/aliases"),
    ("DELETE", "/api/pleroma/aliases"),
    ("POST", "/api/pleroma/move_account"),
    ("POST", "/api/v1/accounts/move"),
];

/// Sends `method` to `path` with `token` and `{}` as the JSON body.
async fn send(app: &Router, token: &str, method: &str, path: &str) -> common::TestResponse {
    let request = match method {
        "POST" => common::TestRequest::post(path),
        "PUT" => common::TestRequest::put(path),
        "DELETE" => common::TestRequest::delete(path),
        _ => common::TestRequest::get(path),
    };
    request.bearer(token).json(&json!({})).send(app).await
}

/// With only `fixture`, `change_password` stores the new password.
#[tokio::test]
async fn a_fixture_only_instance_changes_the_password() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_user_with_password(&pool, "alice", PASSWORD).await;
    let token = common::create_app_and_token(&pool, &alice, SCOPES).await;
    let app = common::test_app_with_config(pool.clone(), config_with_protocols("fixture"));

    let changed = common::TestRequest::post("/api/pleroma/change_password")
        .bearer(&token)
        .json(&json!({
            "password": PASSWORD,
            "new_password": NEW_PASSWORD,
            "new_password_confirmation": NEW_PASSWORD,
        }))
        .send(&app)
        .await;

    common::assert_answer(&changed, StatusCode::OK, "change_password is core");
    assert_eq!(
        changed.json_value(),
        json!({"status": "success"}),
        "the password change succeeded"
    );
    let stored = common::reload_user(&pool, alice.id).await;
    let hash = stored.password_hash.expect("the account keeps a hash");
    assert!(
        verify_password(NEW_PASSWORD, &hash),
        "the new password is stored"
    );
}

/// With only `fixture`, `disable_account` deactivates the account.
#[tokio::test]
async fn a_fixture_only_instance_disables_the_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_user_with_password(&pool, "alice", PASSWORD).await;
    let token = common::create_app_and_token(&pool, &alice, SCOPES).await;
    let app = common::test_app_with_config(pool.clone(), config_with_protocols("fixture"));

    let disabled = common::TestRequest::post("/api/pleroma/disable_account")
        .bearer(&token)
        .json(&json!({"password": PASSWORD}))
        .send(&app)
        .await;

    common::assert_answer(&disabled, StatusCode::OK, "disable_account is core");
    assert!(
        !common::reload_user(&pool, alice.id).await.is_active.get(),
        "the account is deactivated"
    );
}

/// With only `fixture`, `change_email` and `delete_account` are routes, not the fallback.
#[tokio::test]
async fn a_fixture_only_instance_keeps_the_email_and_deletion_routes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_user_with_password(&pool, "alice", PASSWORD).await;
    let token = common::create_app_and_token(&pool, &alice, SCOPES).await;
    let app = common::test_app_with_config(pool, config_with_protocols("fixture"));

    for path in ["/api/pleroma/change_email", "/api/pleroma/delete_account"] {
        let answer = send(&app, &token, "POST", path).await;
        assert!(!is_fallback(&answer), "{path} is core, not the fallback");
    }
}

/// With only `fixture`, the instance utilities of `misc` answer.
#[tokio::test]
async fn a_fixture_only_instance_keeps_the_misc_utilities() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(pool, config_with_protocols("fixture"));

    for path in [
        "/api/pleroma/frontend_configurations",
        "/api/v1/pleroma/captcha",
    ] {
        let answer = common::TestRequest::get(path).send(&app).await;
        common::assert_answer(&answer, StatusCode::OK, "the misc utility is core");
        assert!(!is_fallback(&answer), "{path} is core, not the fallback");
    }
}

/// With only `fixture`, the aliases and move routes are the fallback 404.
#[tokio::test]
async fn a_fixture_only_instance_drops_the_aliases_and_moves() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_user_with_password(&pool, "alice", PASSWORD).await;
    let token = common::create_app_and_token(&pool, &alice, SCOPES).await;
    let app = common::test_app_with_config(pool, config_with_protocols("fixture"));

    for (method, path) in ACTIVITYPUB_BACKED {
        let answer = send(&app, &token, method, path).await;
        assert_eq!(
            answer.status,
            StatusCode::NOT_FOUND,
            "{method} {path} is not found without ActivityPub"
        );
        assert!(
            is_fallback(&answer),
            "{method} {path} is the fallback without ActivityPub"
        );
    }
}

/// The default config mounts the aliases and move routes.
#[tokio::test]
async fn the_default_config_mounts_the_aliases_and_moves() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_user_with_password(&pool, "alice", PASSWORD).await;
    let token = common::create_app_and_token(&pool, &alice, SCOPES).await;
    let app = common::test_app(pool);

    for (method, path) in ACTIVITYPUB_BACKED {
        let answer = send(&app, &token, method, path).await;
        assert!(
            !is_fallback(&answer),
            "{method} {path} is a route with ActivityPub"
        );
    }
}
