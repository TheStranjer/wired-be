//! The `password` grant's scope rules and the token endpoint's
//! grant-type refusals: the 400 "Invalid scope" for a request outside
//! the app's registration, sub-scopes granted verbatim, the `read`
//! default (and its refusal by a write-only app), the email `username`,
//! the deactivated account's 403 `account_is_disabled`, the 400
//! `unsupported_grant_type` and the "Missing parameter: `grant_type`"
//! refusal.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::oauth_token;

use super::support::{PASSWORD, assert_no_store, error_text, fixtures, grant_body, post_json};
use crate::common;

/// A scope outside the app's registration is the 400 "Invalid scope".
#[tokio::test]
async fn a_scope_outside_the_registration_is_invalid_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "grace", "read").await;
    let http = common::test_app(pool.clone());
    let response = post_json(&http, &grant_body(&app, "grace", "read write")).await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "unregistered `write` is refused"
    );
    assert_eq!(error_text(&response), "Invalid scope", "Pleroma's message");
    assert_no_store(&response);
    let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_tokens")
        .fetch_one(&pool)
        .await
        .expect("the count reaches the database");
    assert_eq!(rows.0, 0, "the refused grant stored no token");
}

/// Sub-scopes fit inside a broader registration and come back verbatim.
#[tokio::test]
async fn requested_subscopes_are_granted_verbatim() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "heidi", "read write").await;
    let http = common::test_app(pool.clone());
    let response = post_json(
        &http,
        &grant_body(&app, "heidi", "read:accounts write:statuses"),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "sub-scopes of registered trees pass"
    );
    let body = response.json_value();
    assert_eq!(
        body["scope"], "read:accounts write:statuses",
        "the narrower grant is exactly what was asked for"
    );
    let access = body["access_token"].as_str().expect("a token string");
    let (stored, _) = oauth_token::get_with_app(&pool, access)
        .await
        .expect("the lookup reaches the database")
        .expect("the token row was stored");
    assert_eq!(
        stored.scopes, "read:accounts write:statuses",
        "and that is what got stored"
    );
}

/// Without a `scope` parameter the token gets the default, `read`.
#[tokio::test]
async fn the_default_scope_is_read() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "ivan", "read write").await;
    let http = common::test_app(pool.clone());
    let mut body = grant_body(&app, "ivan", "read write");
    body["scope"] = json!(null);
    let response = post_json(&http, &body).await;
    assert_eq!(response.status, StatusCode::OK, "an absent scope defaults");
    assert_eq!(
        response.json_value()["scope"],
        "read",
        "to Pleroma's default"
    );

    // And `read` must be registered for the default to pass at all.
    let write_only = common::create_app(&pool, "write").await;
    let mut write_only_body = grant_body(&write_only, "ivan", "read write");
    write_only_body["scope"] = json!(null);
    let refused = post_json(&http, &write_only_body).await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "a `write`-only app cannot receive the default `read`"
    );
    assert_eq!(error_text(&refused), "Invalid scope", "as Invalid scope");
}

/// `username` is a nickname or an email, in any case (citext).
#[tokio::test]
async fn an_email_works_as_username() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, app) = fixtures(&pool, "judy", "read").await;
    let http = common::test_app(pool.clone());
    assert_eq!(
        user.email.as_deref(),
        Some("judy@example.org"),
        "the fixture carries the email"
    );
    let response = post_json(&http, &grant_body(&app, "JUDY@example.org", "read")).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the email logs in, case-insensitively"
    );
    assert_eq!(response.json_value()["me"], user.ap_id, "same user");
}

/// A deactivated account with the right password gets Pleroma's 403
/// `account_is_disabled`, not the invalid-credentials 400.
#[tokio::test]
async fn a_deactivated_account_is_refused_with_an_identifier() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = common::create_deactivated_user_with_password(&pool, "kate", PASSWORD).await;
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool.clone());
    let response = post_json(&http, &grant_body(&app, "kate", "read")).await;

    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "a deactivated account is a 403 after authenticating"
    );
    assert_no_store(&response);
    let body = response.json_value();
    assert_eq!(
        body["error"], "Your account is currently disabled",
        "Pleroma's message"
    );
    assert_eq!(
        body["identifier"], "account_is_disabled",
        "and its machine-readable reason code"
    );
    assert_eq!(
        body.as_object().expect("an object").len(),
        2,
        "exactly the two keys"
    );
    assert!(
        !body["error"].as_str().unwrap().contains(&user.nickname),
        "the body leaks nothing beyond the fixed message"
    );

    let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_tokens")
        .fetch_one(&pool)
        .await
        .expect("the count reaches the database");
    assert_eq!(rows.0, 0, "the refusal minted no token");
}

/// A grant type the endpoint does not speak is the 400
/// `unsupported_grant_type`.
#[tokio::test]
async fn an_unknown_grant_type_is_unsupported() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "luna", "read").await;
    let http = common::test_app(pool.clone());
    for grant in ["urn:ietf:params:oauth:grant-type:device_code", "bogus"] {
        let mut body = grant_body(&app, "luna", "read");
        body["grant_type"] = json!(grant);
        let response = post_json(&http, &body).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "`{grant}` is a 400 here"
        );
        assert_eq!(
            error_text(&response),
            "unsupported_grant_type",
            "for the named reason"
        );
        assert_no_store(&response);
    }
}

/// A request without `grant_type` at all is refused by name, matching
/// Pleroma's message.
#[tokio::test]
async fn a_missing_grant_type_is_named() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "mike", "read").await;
    let http = common::test_app(pool.clone());
    let mut body = grant_body(&app, "mike", "read");
    body["grant_type"] = json!(null);
    let response = post_json(&http, &body).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST, "a 400");
    assert_eq!(
        error_text(&response),
        "Missing parameter: grant_type",
        "naming the missing parameter"
    );
    assert_no_store(&response);

    // Nothing at all in the body is still "missing grant_type", not a 422.
    let form_answer = common::TestRequest::post("/oauth/token")
        .form(&[("username", "mike")])
        .send(&http)
        .await;
    assert_eq!(
        form_answer.status,
        StatusCode::BAD_REQUEST,
        "an unrelated form body"
    );
    assert_eq!(
        error_text(&form_answer),
        "Missing parameter: grant_type",
        "gets the same answer"
    );
}
