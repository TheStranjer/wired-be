//! Pleroma's bare `admin` scope over HTTP: Soapbox registers its app and
//! asks for tokens with `read write follow push admin`. The registration
//! echoes the string, and a password grant yields a token whose
//! `admin:read` reach still ends at the account's own role: an admin
//! lists users, a non-admin gets the usual refusal.

use axum::http::StatusCode;
use serde_json::json;

use super::support::{PASSWORD, grant_body, post_json};
use crate::common;

/// The scope string Soapbox sends for every Pleroma backend.
const SOAPBOX_SCOPES: &str = "read write follow push admin";
/// The admin user listing, gated on `admin:read` and an admin account.
const USERS: &str = "/api/v1/pleroma/admin/users";

/// Registering an app with the bare `admin` scope succeeds and echoes it.
#[tokio::test]
async fn an_app_registers_with_the_bare_admin_scope() {
    let db = common::test_db().await;
    let http = common::test_app(db.pool().clone());
    let response = common::TestRequest::post("/api/v1/apps")
        .json(&json!({
            "client_name": "Soapbox FE",
            "redirect_uris": "https://soapbox.example/login",
            "scopes": SOAPBOX_SCOPES,
        }))
        .send(&http)
        .await;
    assert_eq!(response.status, StatusCode::OK, "`admin` is accepted");
    assert_eq!(
        response.json_value()["scopes"],
        json!(["read", "write", "follow", "push", "admin"]),
        "the scopes echo the request"
    );
}

/// Registering with `admin:foo` stays refused.
#[tokio::test]
async fn an_unknown_admin_branch_is_still_refused_at_registration() {
    let db = common::test_db().await;
    let http = common::test_app(db.pool().clone());
    let response = common::TestRequest::post("/api/v1/apps")
        .json(&json!({
            "client_name": "Soapbox FE",
            "redirect_uris": "https://soapbox.example/login",
            "scopes": "read admin:foo",
        }))
        .send(&http)
        .await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "`admin:foo` is not a scope"
    );
}

/// An admin's `admin` token reaches the admin user listing.
#[tokio::test]
async fn an_admin_account_with_an_admin_token_lists_users() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _admin = common::create_admin_with_password(&pool, "root", PASSWORD).await;
    let app = common::create_app(&pool, SOAPBOX_SCOPES).await;
    let http = common::test_app(pool.clone());
    let grant = post_json(&http, &grant_body(&app, "root", SOAPBOX_SCOPES)).await;
    assert_eq!(grant.status, StatusCode::OK, "the password grant succeeds");
    let body = grant.json_value();
    assert_eq!(body["scope"], SOAPBOX_SCOPES, "the token carries `admin`");
    let token = body["access_token"].as_str().expect("a token string");
    let listing = common::TestRequest::get(USERS)
        .bearer(token)
        .send(&http)
        .await;
    assert_eq!(
        listing.status,
        StatusCode::OK,
        "`admin` covers `admin:read` for an admin account"
    );
}

/// A non-admin's `admin` token is refused by the role check.
#[tokio::test]
async fn a_non_admin_account_with_an_admin_token_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _user = common::create_user_with_password(&pool, "plain", PASSWORD).await;
    let app = common::create_app(&pool, SOAPBOX_SCOPES).await;
    let http = common::test_app(pool.clone());
    let grant = post_json(&http, &grant_body(&app, "plain", SOAPBOX_SCOPES)).await;
    assert_eq!(grant.status, StatusCode::OK, "the password grant succeeds");
    let body = grant.json_value();
    let token = body["access_token"].as_str().expect("a token string");
    let listing = common::TestRequest::get(USERS)
        .bearer(token)
        .send(&http)
        .await;
    assert_eq!(
        listing.status,
        StatusCode::FORBIDDEN,
        "the scope never replaces the admin role"
    );
    assert_eq!(
        listing.error_text(),
        "User is not an admin.",
        "the role gate"
    );
}

/// An app registered with only `admin:read admin:write` cannot be granted
/// the bare `admin`.
#[tokio::test]
async fn the_bare_admin_is_not_covered_by_the_split_registration() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _admin = common::create_admin_with_password(&pool, "root", PASSWORD).await;
    let app = common::create_app(&pool, "admin:read admin:write").await;
    let http = common::test_app(pool.clone());
    let grant = post_json(&http, &grant_body(&app, "root", "admin")).await;
    assert_eq!(
        grant.status,
        StatusCode::BAD_REQUEST,
        "`admin` is broader than its two halves"
    );
    assert_eq!(grant.error_text(), "Invalid scope", "Pleroma's message");
}
