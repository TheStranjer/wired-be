//! `GET /api/v1/preferences` (WBE-826): the posting and reading defaults a
//! Mastodon client reads right after login.
//!
//! The tests pin the Mastodon default object for a fresh user, the posting
//! keys following what `update_credentials` stored and staying equal to
//! `verify_credentials.source`, and the `AuthUser` + `read:accounts` gates.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};

use crate::common;

/// The path under test.
const ROUTE: &str = "/api/v1/preferences";
/// The profile write the stored defaults come from.
const UPDATE: &str = "/api/v1/accounts/update_credentials";
/// The self view the keys must agree with.
const VERIFY: &str = "/api/v1/accounts/verify_credentials";

/// `GET /api/v1/preferences` as `token`.
async fn preferences(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get(ROUTE)
        .bearer(token)
        .send(app)
        .await
}

/// A fresh user answers Mastodon's default object.
#[tokio::test]
async fn a_fresh_user_gets_the_default_preferences() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = common::test_app(pool);
    let response = preferences(&app, &token).await;
    common::assert_answer(&response, StatusCode::OK, "the owner's read token");
    assert_eq!(
        response.json_value(),
        json!({
            "posting:default:visibility": "public",
            "posting:default:sensitive": false,
            "posting:default:language": null,
            "posting:default:quote_policy": "public",
            "reading:expand:media": "default",
            "reading:expand:spoilers": false,
            "reading:autoplay:gifs": false,
        })
    );
}

/// What `update_credentials` stored comes back, in step with `source`.
#[tokio::test]
async fn stored_defaults_follow_update_credentials() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read write").await;
    let app = common::test_app(pool);
    let update = common::TestRequest::patch(UPDATE)
        .bearer(&token)
        .json(&json!({"source": {"privacy": "unlisted", "sensitive": true, "language": "eo"}}))
        .send(&app)
        .await;
    common::assert_answer(&update, StatusCode::OK, "the profile write");

    let response = preferences(&app, &token).await;
    common::assert_answer(&response, StatusCode::OK, "the preferences read");
    let body = response.json_value();
    assert_eq!(body["posting:default:visibility"], "unlisted");
    assert_eq!(body["posting:default:sensitive"], true);
    assert_eq!(body["posting:default:language"], "eo");

    let verify = common::TestRequest::get(VERIFY)
        .bearer(&token)
        .send(&app)
        .await;
    let source: Value = verify.json_value()["source"].clone();
    assert_eq!(body["posting:default:visibility"], source["privacy"]);
    assert_eq!(body["posting:default:sensitive"], source["sensitive"]);
    assert_eq!(body["posting:default:language"], source["language"]);

    let again = common::TestRequest::patch(UPDATE)
        .bearer(&token)
        .json(&json!({"source": {"privacy": "private"}}))
        .send(&app)
        .await;
    common::assert_answer(&again, StatusCode::OK, "the second profile write");
    assert_eq!(
        preferences(&app, &token).await.json_value()["posting:default:visibility"],
        "private",
        "changing source[privacy] changes the default visibility"
    );
}

/// No token is the 401, an app-only token the user-required 403, and a token
/// without `read:accounts` the 403 naming it.
#[tokio::test]
async fn the_route_is_gated_by_auth_and_the_read_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let wrong = common::create_app_and_token(&pool, &alice, "write:accounts read:statuses").await;
    let broad = common::create_app_and_token(&pool, &alice, "read").await;
    let app_only = common::create_app_only_token(&pool, "read").await;
    let app = common::test_app(pool);

    let anonymous = common::TestRequest::get(ROUTE).send(&app).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
    common::assert_error(&anonymous, "Invalid credentials", "no token");

    let userless = preferences(&app, &app_only).await;
    common::assert_answer(&userless, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(
        &userless,
        "This API requires an authenticated user",
        "an app-only token",
    );

    let refused = preferences(&app, &wrong).await;
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "a token without the scope");
    common::assert_error(
        &refused,
        "Insufficient permissions: read:accounts.",
        "a token without the scope",
    );

    let granted = preferences(&app, &broad).await;
    common::assert_answer(&granted, StatusCode::OK, "the `read` parent grants it");
}
