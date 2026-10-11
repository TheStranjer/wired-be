//! Integration tests for `POST /oauth/token`: the password grant.
//!
//! Success over JSON, form and HTTP Basic bodies; the 400 "Invalid
//! credentials" shared by wrong secrets, wrong passwords and unknown
//! users; the 400 "Invalid scope" for a request outside the app's
//! registration; the 403 `account_is_disabled` for a deactivated
//! account; the `unsupported_grant_type` and "Missing parameter:
//! `grant_type`" refusals; and the response shape, headers and stored
//! token row (Pleroma's `OAuthController.token_exchange`).

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::DEFAULT_OAUTH_TOKEN_TTL_SECS,
    models::{oauth_app::OauthApp, oauth_token, user::User},
};

/// The password every user fixture logs in with.
const PASSWORD: &str = "correct horse battery staple";

/// POSTs `body` as JSON to the token endpoint.
async fn post_json(app: &Router, body: &Value) -> common::TestResponse {
    common::TestRequest::post("/oauth/token")
        .json(body)
        .send(app)
        .await
}

/// The `error` message of a JSON error response.
fn error_text(response: &common::TestResponse) -> String {
    response.json_value()["error"]
        .as_str()
        .expect("errors carry a string error field")
        .to_owned()
}

/// Assert the no-store cache headers every answer carries (RFC 6749 §5.1).
fn assert_no_store(response: &common::TestResponse) {
    assert_eq!(
        response.header("cache-control"),
        Some("no-store"),
        "cache-control: no-store"
    );
    assert_eq!(
        response.header("pragma"),
        Some("no-cache"),
        "pragma: no-cache"
    );
}

/// The password-grant body carrying app `app`'s credentials in the plain
/// body fields, asking for `scope` as `username`.
fn grant_body(app: &OauthApp, username: &str, scope: &str) -> Value {
    json!({
        "grant_type": "password",
        "username": username,
        "password": PASSWORD,
        "scope": scope,
        "client_id": app.client_id,
        "client_secret": app.client_secret,
    })
}

/// A named user and a registered app, the two fixtures every test starts
/// from.
async fn fixtures(pool: &PgPool, nickname: &str, app_scopes: &str) -> (User, OauthApp) {
    let user = common::create_user_with_password(pool, nickname, PASSWORD).await;
    let app = common::create_app(pool, app_scopes).await;
    (user, app)
}

/// The full password grant over JSON: the documented body, the entity
/// fields, and the token row behind them.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn json_password_grant_returns_a_token_pair(pool: PgPool) {
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
        stored.refresh_token, refresh,
        "the refresh secret round trips"
    );
    assert_eq!(stored.user_id, Some(user.id), "the token acts as the user");
    assert_eq!(stored.app_id, app.id, "issued for the validated app");
    assert_eq!(stored.scopes, "read write", "granted what was requested");
    assert_eq!(stored_app.id, app.id, "joined to its app");
}

/// The same grant over an urlencoded form body, the shape Mastodon
/// clients post.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn form_password_grant_returns_a_token_pair(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn basic_client_credentials_authenticate_the_app(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn bad_client_credentials_are_invalid_credentials(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn bad_user_credentials_are_invalid_credentials(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn missing_user_parameters_are_invalid_credentials(pool: PgPool) {
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

/// A scope outside the app's registration is the 400 "Invalid scope".
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_scope_outside_the_registration_is_invalid_scope(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn requested_subscopes_are_granted_verbatim(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_default_scope_is_read(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn an_email_works_as_username(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_deactivated_account_is_refused_with_an_identifier(pool: PgPool) {
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

/// A grant type other than `password` is the 400
/// `unsupported_grant_type`.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn an_unknown_grant_type_is_unsupported(pool: PgPool) {
    let (_user, app) = fixtures(&pool, "luna", "read").await;
    let http = common::test_app(pool.clone());
    for grant in [
        "authorization_code",
        "refresh_token",
        "client_credentials",
        "bogus",
    ] {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_missing_grant_type_is_named(pool: PgPool) {
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
