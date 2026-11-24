//! The `password` grant over `POST /oauth/token`: the full JSON answer
//! with its entity fields and stored row, the urlencoded form body, the
//! `Authorization: Basic` client credentials winning over the body, and
//! the shared 400 "Invalid credentials" for wrong client secrets, unknown
//! clients, absent credentials, wrong passwords and unknown or missing
//! users.

use axum::http::StatusCode;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::json;
use uuid::Uuid;
use wired_be::{
    config::DEFAULT_OAUTH_TOKEN_TTL_SECS, models::oauth_token, util::token::sha256_hex,
};

use super::support::{PASSWORD, assert_no_store, error_text, fixtures, grant_body, post_json};
use crate::common;

/// The full password grant over JSON: the documented body, the entity
/// fields, and the token row behind them.
#[tokio::test]
async fn json_password_grant_returns_a_token_pair() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, app) = fixtures(&pool, "alice", "read write").await;
    let http = common::test_app(pool.clone());
    let response = post_json(&http, &grant_body(&app, "alice", "read write")).await;

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
    let refresh = body["refresh_token"]
        .as_str()
        .expect("refresh_token is a string");
    assert_eq!(access.len(), 43, "the 43-char Pleroma token shape");
    assert_eq!(refresh.len(), 43, "both credentials share it");
    assert_ne!(access, refresh, "and they differ");
    assert_eq!(
        body["scope"], "read write",
        "the granted scopes, space-joined"
    );
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
    assert_eq!(body["me"], user.ap_id, "me is the user's actor id");
    assert_eq!(
        body.as_object().expect("an object").len(),
        8,
        "exactly the documented keys"
    );

    let (stored, stored_app) = oauth_token::get_with_app(&pool, access)
        .await
        .expect("the lookup reaches the database")
        .expect("the token row was stored");
    assert_eq!(stored.id, id, "the response id is the stored primary key");
    assert_eq!(
        stored.refresh_token,
        sha256_hex(refresh),
        "the refresh secret is stored as its digest"
    );
    assert_eq!(
        stored.token,
        sha256_hex(access),
        "the access token is stored as its digest"
    );
    assert_eq!(stored.user_id, Some(user.id), "the token acts as the user");
    assert_eq!(stored.app_id, app.id, "issued for the validated app");
    assert_eq!(stored.scopes, "read write", "granted what was requested");
    assert_eq!(stored_app.id, app.id, "joined to its app");
}

/// The same grant over an urlencoded form body, the shape Mastodon
/// clients post.
#[tokio::test]
async fn form_password_grant_returns_a_token_pair() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, app) = fixtures(&pool, "bob", "read write follow").await;
    let http = common::test_app(pool.clone());
    let response = common::TestRequest::post("/oauth/token")
        .form(&[
            ("grant_type", "password"),
            ("username", "bob"),
            ("password", PASSWORD),
            ("scope", "read follow"),
            ("client_id", app.client_id.as_str()),
            ("client_secret", app.client_secret.as_str()),
        ])
        .send(&http)
        .await;

    assert_eq!(response.status, StatusCode::OK, "the form grant succeeds");
    assert_no_store(&response);
    let body = response.json_value();
    assert_eq!(
        body["scope"], "read follow",
        "the requested subset is granted"
    );
    assert_eq!(body["me"], user.ap_id, "the owner's actor id");
    assert!(
        !body["access_token"].is_null() && !body["refresh_token"].is_null(),
        "both credentials came back"
    );
}

/// Client credentials through `Authorization: Basic`, the RFC 6749
/// preferred presentation, win over the body fields.
#[tokio::test]
async fn basic_client_credentials_authenticate_the_app() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (user, app) = fixtures(&pool, "carol", "read write").await;
    let http = common::test_app(pool.clone());
    let basic = STANDARD.encode(format!("{}:{}", app.client_id, app.client_secret));
    let response = common::TestRequest::post("/oauth/token")
        .header("authorization", &format!("Basic {basic}"))
        .json(&json!({
            "grant_type": "password",
            "username": "carol",
            "password": PASSWORD,
            "scope": "read",
        }))
        .send(&http)
        .await;

    assert_eq!(response.status, StatusCode::OK, "Basic credentials work");
    assert_no_store(&response);
    assert_eq!(
        response.json_value()["me"],
        user.ap_id,
        "the token belongs to the user"
    );

    // The header wins: bogus body credentials cannot poison a good pair.
    let poisoned = json!({
        "grant_type": "password",
        "username": "carol",
        "password": PASSWORD,
        "client_id": "not-this-client",
        "client_secret": "not-this-secret",
    });
    let poisoned_answer = common::TestRequest::post("/oauth/token")
        .header("authorization", &format!("Basic {basic}"))
        .json(&poisoned)
        .send(&http)
        .await;
    assert_eq!(
        poisoned_answer.status,
        StatusCode::OK,
        "the Basic pair takes precedence over the body"
    );
}

/// A wrong client secret, an unknown client id and no credentials at all
/// all answer the same 400, Pleroma's `Invalid credentials`.
#[tokio::test]
async fn bad_client_credentials_are_invalid_credentials() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "dave", "read").await;
    let http = common::test_app(pool.clone());

    let mut wrong_secret = grant_body(&app, "dave", "read");
    wrong_secret["client_secret"] = json!("not-the-secret");
    let secret_answer = post_json(&http, &wrong_secret).await;
    assert_eq!(
        secret_answer.status,
        StatusCode::BAD_REQUEST,
        "a wrong secret is a 400"
    );
    assert_eq!(
        error_text(&secret_answer),
        "Invalid credentials",
        "one message for every credential failure"
    );
    assert_no_store(&secret_answer);

    let mut unknown_client = grant_body(&app, "dave", "read");
    unknown_client["client_id"] = json!("no-such-client");
    let client_answer = post_json(&http, &unknown_client).await;
    assert_eq!(
        client_answer.status,
        StatusCode::BAD_REQUEST,
        "an unknown client id is a 400"
    );
    assert_eq!(
        error_text(&client_answer),
        "Invalid credentials",
        "same message"
    );

    let mut bare = grant_body(&app, "dave", "read");
    bare["client_id"] = json!(null);
    bare["client_secret"] = json!(null);
    let bare_answer = post_json(&http, &bare).await;
    assert_eq!(
        bare_answer.status,
        StatusCode::BAD_REQUEST,
        "absent credentials are a 400"
    );
    assert_eq!(
        error_text(&bare_answer),
        "Invalid credentials",
        "same message"
    );

    let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_tokens")
        .fetch_one(&pool)
        .await
        .expect("the count reaches the database");
    assert_eq!(rows.0, 0, "no rejection minted a token");
}

/// A wrong password and an unknown user share the 400 "Invalid
/// credentials" answer (Pleroma does not tell them apart).
#[tokio::test]
async fn bad_user_credentials_are_invalid_credentials() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "erin", "read").await;
    let http = common::test_app(pool.clone());

    let mut wrong_password = grant_body(&app, "erin", "read");
    wrong_password["password"] = json!("wrong password");
    let password_answer = post_json(&http, &wrong_password).await;
    assert_eq!(
        password_answer.status,
        StatusCode::BAD_REQUEST,
        "a wrong password is a 400"
    );
    assert_eq!(
        error_text(&password_answer),
        "Invalid credentials",
        "its message"
    );

    let unknown_user = grant_body(&app, "nobody", "read");
    let user_answer = post_json(&http, &unknown_user).await;
    assert_eq!(
        user_answer.status,
        StatusCode::BAD_REQUEST,
        "an unknown user is a 400"
    );
    assert_eq!(
        error_text(&user_answer),
        "Invalid credentials",
        "the same message, deliberately"
    );
}

/// Missing `username` and missing `password` fail exactly like wrong ones.
#[tokio::test]
async fn missing_user_parameters_are_invalid_credentials() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_user, app) = fixtures(&pool, "frank", "read").await;
    let http = common::test_app(pool.clone());
    for missing in ["username", "password"] {
        let mut body = grant_body(&app, "frank", "read");
        body[missing] = json!(null);
        let response = post_json(&http, &body).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "without `{missing}` the grant fails"
        );
        assert_eq!(
            error_text(&response),
            "Invalid credentials",
            "with Pleroma's answer"
        );
    }
}
