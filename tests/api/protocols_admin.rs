//! The admin split under each `PROTOCOLS` value: managing local accounts and
//! the instance is core and answers with only `fixture` enabled, while the
//! status, report, relay, remote-instance, chat and forced-follow routes are
//! `ActivityPub`-backed and answer the app-wide fallback there.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::json;
use sqlx::PgPool;
use wired_be::{
    config::{
        BASE_URL_VAR, Config, DATABASE_URL_VAR, PROTOCOLS_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
    },
    models::user::{Flag, NewUser},
};

use crate::common;

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

/// The Pleroma admin API prefix.
const ADMIN: &str = "/api/v1/pleroma/admin";
/// The scopes of the admin token every test here mints.
const ADMIN_SCOPES: &str = "admin:read admin:write";

/// Creates an admin account named `root` and a token for it.
async fn admin_token(pool: &PgPool) -> String {
    let root = common::create_local_user_with(pool, "root", |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    common::create_app_and_token(pool, &root, ADMIN_SCOPES).await
}

/// Whether `response` came from the app-wide fallback rather than a route.
fn is_fallback(response: &common::TestResponse) -> bool {
    response.header("x-wired-fallback") == Some("1")
}

/// Sends `method` to `path` as the admin, with `{}` as the JSON body.
async fn send_as_admin(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
) -> common::TestResponse {
    let request = match method {
        "POST" => common::TestRequest::post(path),
        "PATCH" => common::TestRequest::patch(path),
        "DELETE" => common::TestRequest::delete(path),
        _ => common::TestRequest::get(path),
    };
    request.bearer(token).json(&json!({})).send(app).await
}

/// The `ActivityPub`-backed admin routes, as a method and a path each.
///
/// The forced follow pair is not here: with `ActivityPub` off its paths still
/// match the core `/users/{nickname}` route, so they answer 405 rather than
/// the fallback 404. [`FORCED_FOLLOWS`] covers them.
const ACTIVITYPUB_BACKED: [(&str, &str); 12] = [
    ("GET", "/api/v1/pleroma/admin/reports"),
    ("GET", "/api/v1/pleroma/admin/reports/some-id"),
    ("GET", "/api/v1/pleroma/admin/relay"),
    ("POST", "/api/v1/pleroma/admin/relay"),
    ("DELETE", "/api/v1/pleroma/admin/relay"),
    ("DELETE", "/api/v1/pleroma/admin/instances/remote.example"),
    ("GET", "/api/v1/pleroma/admin/statuses"),
    ("GET", "/api/v1/pleroma/admin/users/sleepy/statuses"),
    ("GET", "/api/v1/pleroma/admin/users/sleepy/chats"),
    ("GET", "/api/v1/pleroma/admin/stats"),
    ("GET", "/api/v1/admin/reports"),
    ("POST", "/api/v1/admin/reports/some-id/resolve"),
];

/// The `ActivityPub`-backed forced follow and unfollow.
const FORCED_FOLLOWS: [(&str, &str); 2] = [
    ("POST", "/api/v1/pleroma/admin/users/follow"),
    ("POST", "/api/v1/pleroma/admin/users/unfollow"),
];

/// With only `fixture`, an admin lists local users and deactivates one.
#[tokio::test]
async fn a_fixture_only_instance_lets_an_admin_list_and_deactivate_local_users() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let token = admin_token(&pool).await;
    let _sleepy = common::create_local_user(&pool, "sleepy").await;
    let app = common::test_app_with_config(pool, config_with_protocols("fixture"));

    let listed = common::TestRequest::get(&format!("{ADMIN}/users"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&listed, StatusCode::OK, "the user listing is core");
    let body = listed.json_value();
    let nicknames: Vec<&str> = body["users"]
        .as_array()
        .expect("the users array")
        .iter()
        .filter_map(|found| found["nickname"].as_str())
        .collect();
    assert!(
        nicknames.contains(&"sleepy"),
        "the local account is listed, got {nicknames:?}"
    );

    let deactivated = common::TestRequest::patch(&format!("{ADMIN}/users/deactivate"))
        .bearer(&token)
        .json(&json!({"nicknames": ["sleepy"]}))
        .send(&app)
        .await;
    common::assert_answer(&deactivated, StatusCode::OK, "the deactivation is core");
    assert_eq!(
        deactivated.json_value()["users"][0]["is_active"],
        json!(false),
        "the answered account is now deactivated"
    );
}

/// With only `fixture`, the instance-management admin routes answer.
#[tokio::test]
async fn a_fixture_only_instance_keeps_the_instance_management_admin_routes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let token = admin_token(&pool).await;
    let _sleepy = common::create_local_user(&pool, "sleepy").await;
    let app = common::test_app_with_config(pool, config_with_protocols("fixture"));
    let core = [
        format!("{ADMIN}/moderation_log"),
        format!("{ADMIN}/users/sleepy"),
        format!("{ADMIN}/users/sleepy/credentials"),
        format!("{ADMIN}/users/invites"),
        format!("{ADMIN}/config"),
        format!("{ADMIN}/rules"),
        format!("{ADMIN}/webhooks"),
        format!("{ADMIN}/oauth_app"),
        "/api/v1/admin/accounts".to_owned(),
        "/api/v2/admin/accounts".to_owned(),
    ];

    for path in &core {
        let answer = common::TestRequest::get(path)
            .bearer(&token)
            .send(&app)
            .await;
        assert!(!is_fallback(&answer), "{path} is core, not the fallback");
        assert_eq!(answer.status, StatusCode::OK, "{path} answers 200");
    }
}

/// With only `fixture`, the `ActivityPub`-backed admin routes are the fallback 404.
#[tokio::test]
async fn a_fixture_only_instance_drops_the_activitypub_backed_admin_routes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let token = admin_token(&pool).await;
    let app = common::test_app_with_config(pool, config_with_protocols("fixture"));

    for (method, path) in ACTIVITYPUB_BACKED {
        let answer = send_as_admin(&app, &token, method, path).await;
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

/// With only `fixture`, the forced follow pair never reaches a handler.
#[tokio::test]
async fn a_fixture_only_instance_does_not_serve_the_forced_follows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let token = admin_token(&pool).await;
    let app = common::test_app_with_config(pool, config_with_protocols("fixture"));

    for (method, path) in FORCED_FOLLOWS {
        let answer = send_as_admin(&app, &token, method, path).await;
        assert_eq!(
            answer.status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path} only matches the core show route, which is GET-only"
        );
    }
}

/// The default config mounts every `ActivityPub`-backed admin route.
#[tokio::test]
async fn the_default_config_mounts_the_activitypub_backed_admin_routes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let token = admin_token(&pool).await;
    let app = common::test_app(pool);

    for (method, path) in ACTIVITYPUB_BACKED.into_iter().chain(FORCED_FOLLOWS) {
        let answer = send_as_admin(&app, &token, method, path).await;
        assert!(
            !is_fallback(&answer),
            "{method} {path} is a route with ActivityPub"
        );
    }
}
