//! The `client_credentials` grant over `POST /oauth/token`: the app-only
//! JSON answer (no `refresh_token`, no `me`, the stored `user_id = NULL`
//! row), the same grant through HTTP Basic with the default scope, its
//! credential and scope refusals, and the granted token's `AppAuth`
//! acceptance and `AuthUser` refusal.

use axum::http::StatusCode;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::json;
use uuid::Uuid;
use wired_be::{config::DEFAULT_OAUTH_TOKEN_TTL_SECS, models::oauth_token};

use super::support::{PASSWORD, assert_no_store, client_credentials_body, fixtures, post_json};
use crate::common;

/// The full `client_credentials` grant over JSON with the credentials in
/// the body: the app-only token — no `refresh_token`, no `me` — and the
/// `user_id = NULL` row behind it.
#[tokio::test]
async fn client_credentials_grant_returns_an_app_only_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::create_app(&pool, "read write").await;
    let http = common::test_app(pool.clone());
    let response = post_json(&http, &client_credentials_body(&app, "read write")).await;

    assert_eq!(response.status, StatusCode::OK, "the grant succeeds");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the answer is JSON"
    );
    assert_no_store(&response);

    let body = response.json_value();
    assert_eq!(body["token_type"], "Bearer", "bearer tokens");
    let access = body["access_token"]
        .as_str()
        .expect("access_token is a string");
    assert_eq!(access.len(), 43, "the 43-char Pleroma token shape");
    assert_eq!(body["scope"], "read write", "the requested subset");
    let created = body["created_at"]
        .as_i64()
        .expect("created_at is unix seconds");
    let now = chrono::Utc::now().timestamp();
    assert!(
        (now - 5..=now + 5).contains(&created),
        "created_at is issue time, got {created} vs {now}"
    );
    let ttl = i64::try_from(DEFAULT_OAUTH_TOKEN_TTL_SECS).expect("the default fits in i64");
    let expires_in = body["expires_in"].as_i64().expect("expires_in is seconds");
    assert!(
        (ttl - 5..=ttl).contains(&expires_in),
        "expires_in is the configured lifetime, got {expires_in}"
    );
    let id = body["id"]
        .as_str()
        .and_then(|text| Uuid::parse_str(text).ok())
        .expect("id is the hyphenated token uuid");
    assert!(id.get_version_num() == 7, "the id is a UUIDv7 (decision 4)");
    assert!(
        body.get("refresh_token").is_none(),
        "an app-only answer carries no refresh token: {body}"
    );
    assert!(
        body.get("me").is_none(),
        "and no user means no actor id: {body}"
    );
    assert_eq!(
        body.as_object().expect("an object").len(),
        6,
        "exactly the app-only documented keys"
    );

    let (stored, stored_app) = oauth_token::get_with_app(&pool, access)
        .await
        .expect("the lookup reaches the database")
        .expect("the token row was stored");
    assert_eq!(stored.id, id, "the response id is the stored primary key");
    assert_eq!(
        stored.user_id, None,
        "the token acts as the app, not as a user"
    );
    assert_eq!(stored.app_id, app.id, "issued for the validated app");
    assert_eq!(stored.scopes, "read write", "granted what was requested");
    assert_eq!(stored_app.id, app.id, "joined to its app");
}

/// The same grant with the credentials through `Authorization: Basic`
/// and no `scope` parameter: the app authenticates and the token
/// defaults to `read`.
#[tokio::test]
async fn client_credentials_grant_works_with_basic_auth_and_the_default_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::create_app(&pool, "read write").await;
    let http = common::test_app(pool.clone());
    let basic = STANDARD.encode(format!("{}:{}", app.client_id, app.client_secret));
    let response = common::TestRequest::post("/oauth/token")
        .header("authorization", &format!("Basic {basic}"))
        .json(&json!({"grant_type": "client_credentials"}))
        .send(&http)
        .await;

    assert_eq!(response.status, StatusCode::OK, "Basic credentials work");
    assert_no_store(&response);
    let body = response.json_value();
    assert_eq!(body["scope"], "read", "the absent scope defaults");
    assert!(
        body["access_token"].is_string(),
        "an access token came back: {body}"
    );
    assert!(body.get("refresh_token").is_none(), "no refresh token");
    assert!(body.get("me").is_none(), "no actor id");
}

/// A wrong client secret and an unknown client id answer the same 400
/// "Invalid credentials" as on the password grant; user credentials
/// cannot stand in for the client's.
#[tokio::test]
async fn client_credentials_bad_client_credentials_are_invalid_credentials() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "nora", "read").await;
    let http = common::test_app(pool.clone());

    let mut wrong_secret = client_credentials_body(&app, "read");
    wrong_secret["client_secret"] = json!("not-the-secret");
    let secret_answer = post_json(&http, &wrong_secret).await;
    assert_eq!(
        secret_answer.status,
        StatusCode::BAD_REQUEST,
        "a wrong secret is a 400"
    );
    assert_eq!(
        secret_answer.error_text(),
        "Invalid credentials",
        "one message for every credential failure"
    );
    assert_no_store(&secret_answer);

    let mut unknown_client = client_credentials_body(&app, "read");
    unknown_client["client_id"] = json!("no-such-client");
    // A correct user login cannot rescue the failed client authentication.
    unknown_client["username"] = json!("nora");
    unknown_client["password"] = json!(PASSWORD);
    let client_answer = post_json(&http, &unknown_client).await;
    assert_eq!(
        client_answer.status,
        StatusCode::BAD_REQUEST,
        "an unknown client id is a 400"
    );
    assert_eq!(
        client_answer.error_text(),
        "Invalid credentials",
        "same message"
    );

    let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_tokens")
        .fetch_one(&pool)
        .await
        .expect("the count reaches the database");
    assert_eq!(rows.0, 0, "no rejection minted a token");
}

/// A scope outside the app's registration — and the `read` default
/// against a write-only app — is the 400 "Invalid scope".
#[tokio::test]
async fn client_credentials_scope_outside_the_registration_is_invalid_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool.clone());
    let response = post_json(&http, &client_credentials_body(&app, "read write")).await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "unregistered `write` is refused"
    );
    assert_eq!(response.error_text(), "Invalid scope", "Pleroma's message");
    assert_no_store(&response);

    let write_only = common::create_app(&pool, "write").await;
    let mut defaulted = client_credentials_body(&write_only, "read write");
    defaulted["scope"] = json!(null);
    let refused = post_json(&http, &defaulted).await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "a `write`-only app cannot receive the default `read`"
    );
    assert_eq!(refused.error_text(), "Invalid scope", "as Invalid scope");

    let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_tokens")
        .fetch_one(&pool)
        .await
        .expect("the count reaches the database");
    assert_eq!(rows.0, 0, "the refusals stored no token");
}

/// A granted app-only token authenticates `AppAuth` endpoints — `GET
/// /api/v1/apps/verify_credentials` answers the issuing app — while
/// `AuthUser` endpoints refuse it with the 403 "This API requires an
/// authenticated user".
#[tokio::test]
async fn a_granted_app_only_token_serves_app_auth_but_not_user_auth() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool.clone());
    let response = post_json(&http, &client_credentials_body(&app, "read")).await;
    assert_eq!(response.status, StatusCode::OK, "the grant succeeds");
    let access = response.json_value()["access_token"]
        .as_str()
        .expect("access_token is a string")
        .to_owned();

    let verified = common::TestRequest::get("/api/v1/apps/verify_credentials")
        .bearer(&access)
        .send(&http)
        .await;
    assert_eq!(
        verified.status,
        StatusCode::OK,
        "AppAuth accepts the app-only token"
    );
    let entity = verified.json_value();
    assert_eq!(entity["name"], "Test Client", "the issuing app");
    assert_eq!(entity["id"], app.id.to_string(), "identified by its id");

    let user_endpoint = common::TestRequest::get("/api/oauth_tokens")
        .bearer(&access)
        .send(&http)
        .await;
    assert_eq!(
        user_endpoint.status,
        StatusCode::FORBIDDEN,
        "AuthUser refuses an app-only token"
    );
    assert_eq!(
        user_endpoint.error_text(),
        "This API requires an authenticated user",
        "Pleroma's wording"
    );
}
