//! Integration tests for `POST /api/v1/accounts` (open-mode registration,
//! Pleroma's `AccountController.create`).
//!
//! The route takes any valid bearer token — app-only or user-bound —
//! with the `write:accounts` scope: no token is the 401 "Invalid
//! credentials", a scopeless token the 403 "Insufficient permissions:
//! write:accounts.", and a closed `REGISTRATIONS_OPEN` the 403
//! "Registration is disabled". A well-formed body lands a local account
//! through `create_local_user` and answers the token JSON (Bearer, the
//! presenting token's scopes, the token row's id, `expires_in` and a
//! `refresh_token`); the issued access token authenticates
//! `GET /api/v1/accounts/verify_credentials` for the new account.
//! Validation failures — absent or false `agreement`, a duplicate
//! nickname, an invalid email, a short password — answer 422s, the
//! model's carrying the JSON *string* of the field map in `error`.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{config::Toggle, models::user};

use crate::common;

/// The password every registration test submits (the 8-character minimum,
/// comfortably).
const PASSWORD: &str = "correct horse battery staple";
/// The scopes the registration fixtures register their apps with: the
/// `client_credentials` set Mastodon clients end up holding.
const FULL_SCOPES: &str = "read write follow push";
/// The registration path under test.
const REGISTER: &str = "/api/v1/accounts";
/// The default `OAUTH_TOKEN_TTL_SECS`, 30 days in seconds: what a fresh
/// registration's `expires_in` counts down from.
const TTL_SECS: i64 = 2_592_000;

/// A complete, valid registration body for `username`.
fn body_for(username: &str) -> Value {
    json!({
        "username": username,
        "email": format!("{username}@example.org"),
        "password": PASSWORD,
        "agreement": true,
    })
}

/// [`body_for`] with `extra` merged over it: the one-field-mutated
/// bodies the failure tests send.
fn body_with(username: &str, extra: &Value) -> Value {
    let mut body = body_for(username);
    let map = body.as_object_mut().expect("`body_for` builds an object");
    for (key, value) in extra.as_object().expect("overrides are an object") {
        let _replaced = map.insert(key.clone(), value.clone());
    }
    body
}

/// POSTs `body` as JSON to the registration endpoint with `token`.
async fn register_json(app: &Router, token: &str, body: &Value) -> common::TestResponse {
    common::TestRequest::post(REGISTER)
        .bearer(token)
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

/// Registers `username` over a fresh app-only token and returns the
/// parsed 200 token body, asserting its shape.
async fn register_one(app: &Router, pool: &PgPool, username: &str) -> Value {
    let token = common::create_app_only_token(pool, FULL_SCOPES).await;
    let response = register_json(app, &token, &body_for(username)).await;
    assert_eq!(response.status, StatusCode::OK, "200 on success");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the answer is JSON"
    );
    response.json_value()
}

/// Success end to end: the token JSON answers with its keys, the issued
/// access token authenticates `verify_credentials` for the new account,
/// and the row carries everything the body claimed — the `reason` in
/// `registration_reason` for the later approval flow.
#[tokio::test]
async fn registration_answers_a_usable_token_and_stores_the_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = common::create_app_only_token(&pool, FULL_SCOPES).await;
    let response = register_json(
        &app,
        &token,
        &json!({
            "username": "lain",
            "email": "lain@example.org",
            "password": PASSWORD,
            "agreement": true,
            "locale": "ja",
            "reason": "I'm a cool dude, bro",
            "fullname": "Lain Iwakura",
            "bio": "wired",
            "confirm": PASSWORD,
        }),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "200 on success");
    let body = response.json_value();
    assert_eq!(body["token_type"], "Bearer", "bearer credentials");
    assert_eq!(
        body["scope"], FULL_SCOPES,
        "the presenting token's scopes carry over"
    );
    assert!(
        (TTL_SECS - 60..=TTL_SECS + 1).contains(&body["expires_in"].as_i64().expect("a number")),
        "expires_in counts down the configured month, got {}",
        body["expires_in"]
    );
    let created_at = body["created_at"].as_i64().expect("unix seconds");
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is past 1970")
            .as_secs(),
    )
    .expect("unix seconds fit an i64");
    assert!(
        (now - 60..=now + 1).contains(&created_at),
        "created_at is the issue time, got {created_at}"
    );
    let token_id = Uuid::parse_str(body["id"].as_str().expect("the token id"))
        .expect("the id is hyphenated UUID text");
    assert!(token_id.get_version_num() == 7, "the id is a UUIDv7");
    let access_token = body["access_token"]
        .as_str()
        .expect("an access token")
        .to_owned();
    assert_eq!(access_token.len(), 43, "a 43-character credential");
    assert_ne!(
        access_token,
        body["refresh_token"].as_str().expect("a refresh token"),
        "the pair is two distinct tokens"
    );

    // The issued token speaks for the new account.
    let who = common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(&access_token)
        .send(&app)
        .await;
    assert_eq!(who.status, StatusCode::OK, "the token is usable");
    assert_eq!(who.json_value()["username"], "lain", "it names the account");

    let stored = user::get_by_nickname(&pool, "lain")
        .await
        .expect("the lookup reaches the database")
        .expect("the registration stored a row");
    assert!(stored.local.get(), "a registered account is local");
    assert!(stored.is_active.get(), "and active");
    assert_eq!(stored.name, "Lain Iwakura", "fullname is the display name");
    assert_eq!(stored.raw_bio, "wired", "bio lands as source text");
    assert_eq!(
        stored.registration_reason.as_deref(),
        Some("I'm a cool dude, bro"),
        "the reason is stored for the approval flow"
    );
}

/// The same round trip over an urlencoded form body: `agreement=true`
/// arrives as a string and the flex boolean accepts it.
#[tokio::test]
async fn a_form_body_registers_the_same() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = common::create_app_only_token(&pool, FULL_SCOPES).await;
    let response = common::TestRequest::post(REGISTER)
        .bearer(&token)
        .form(&[
            ("username", "formy"),
            ("email", "formy@example.org"),
            ("password", PASSWORD),
            ("agreement", "true"),
        ])
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "a form body registers");
    assert!(
        user::get_by_nickname(&pool, "formy")
            .await
            .expect("the lookup runs")
            .is_some(),
        "the row is there"
    );
}

/// A caller who already holds a user token may register anyway (Pleroma
/// permits it), and the new account gets its own token.
#[tokio::test]
async fn a_user_token_may_register() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let alice_token = common::create_app_and_token(&pool, &alice, FULL_SCOPES).await;
    let response = register_json(&app, &alice_token, &body_for("second")).await;
    assert_eq!(response.status, StatusCode::OK, "user tokens register too");
    let body = response.json_value();
    let who = common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(body["access_token"].as_str().expect("an access token"))
        .send(&app)
        .await;
    assert_eq!(who.status, StatusCode::OK, "the fresh token is usable");
    assert_eq!(
        who.json_value()["username"],
        "second",
        "and it speaks for the new account, not alice"
    );
}

/// `agreement` absent is the extractor's 422; `agreement: false` is the
/// endpoint's, naming Mastodon's signup terms wording.
#[tokio::test]
async fn agreement_is_required_and_must_be_true() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = common::create_app_only_token(&pool, FULL_SCOPES).await;

    let absent = register_json(
        &app,
        &token,
        &json!({"username": "lain", "email": "lain@example.org", "password": PASSWORD}),
    )
    .await;
    assert_eq!(
        absent.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "absent 422"
    );

    let declined = register_json(
        &app,
        &token,
        &body_with("lain", &json!({"agreement": false})),
    )
    .await;
    assert_eq!(
        declined.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "false 422"
    );
    assert_eq!(
        error_text(&declined),
        "You have to accept the terms to sign up",
        "the refusal wording"
    );
    assert!(
        user::get_by_nickname(&pool, "lain")
            .await
            .expect("the lookup runs")
            .is_none(),
        "nothing was registered"
    );
}

/// A taken nickname is the model's 422: the `error` value is the JSON
/// *string* of the field map, naming `nickname`.
#[tokio::test]
async fn a_duplicate_nickname_is_a_json_string_field_map() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let lain = common::create_local_user(&pool, "lain").await;
    assert_eq!(lain.nickname, "lain", "the fixture row exists");
    let token = common::create_app_only_token(&pool, FULL_SCOPES).await;
    let response = register_json(&app, &token, &body_for("LAIN")).await;
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY, "422");
    let message = error_text(&response);
    let field_map: Value =
        serde_json::from_str(&message).expect("the error value is the JSON text of the field map");
    assert_eq!(
        field_map["nickname"],
        json!(["has already been taken"]),
        "the citext collision names the nickname field, in any case"
    );
}

/// An invalid email and a too-short password arrive as the model's field
/// errors, keyed `email` and `password`.
#[tokio::test]
async fn invalid_email_and_short_password_are_field_errors() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = common::create_app_only_token(&pool, FULL_SCOPES).await;

    let bad_email = register_json(
        &app,
        &token,
        &json!({"username": "lain", "email": "plainaddress", "password": PASSWORD, "agreement": true}),
    )
    .await;
    assert_eq!(bad_email.status, StatusCode::UNPROCESSABLE_ENTITY, "422");
    let email_map: Value =
        serde_json::from_str(&error_text(&bad_email)).expect("a JSON string field map");
    assert_eq!(
        email_map["email"],
        json!(["has invalid format"]),
        "the email key carries Ecto's format message"
    );

    let short = register_json(
        &app,
        &token,
        &body_with("lain", &json!({"password": "tiny"})),
    )
    .await;
    assert_eq!(short.status, StatusCode::UNPROCESSABLE_ENTITY, "422");
    let password_map: Value =
        serde_json::from_str(&error_text(&short)).expect("a JSON string field map");
    assert_eq!(
        password_map["password"],
        json!(["should be at least 8 character(s)"]),
        "the password key carries the minimum's message"
    );
}

/// While `REGISTRATIONS_OPEN` is false the endpoint answers 403 with the
/// ticket's wording and stores nothing.
#[tokio::test]
async fn closed_registrations_are_a_403() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = common::test_config();
    config.registrations_open = Toggle::new(false);
    let app = common::test_app_with_config(pool.clone(), config);
    let token = common::create_app_only_token(&pool, FULL_SCOPES).await;
    let response = register_json(&app, &token, &body_for("lain")).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN, "403 when closed");
    assert_eq!(
        error_text(&response),
        "Registration is disabled",
        "the pinned wording"
    );
    assert!(
        user::get_by_nickname(&pool, "lain")
            .await
            .expect("the lookup runs")
            .is_none(),
        "a closed gate registered nothing"
    );
}

/// The scope gate: a valid token without `write:accounts` is Pleroma's
/// insufficient-permissions 403, and no token at all is the 401.
#[tokio::test]
async fn scope_and_token_gates() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());

    let read_only = common::create_app_only_token(&pool, "read").await;
    let denied = register_json(&app, &read_only, &body_for("lain")).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "a scope miss is 403");
    assert_eq!(
        error_text(&denied),
        "Insufficient permissions: write:accounts.",
        "Pleroma names the missing scope, period included"
    );

    let anonymous = common::TestRequest::post(REGISTER)
        .json(&body_for("lain"))
        .send(&app)
        .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "no token 401");
    assert_eq!(
        error_text(&anonymous),
        "Invalid credentials",
        "the 401 body"
    );

    let bogus = register_json(&app, "not-a-real-token", &body_for("lain")).await;
    assert_eq!(bogus.status, StatusCode::UNAUTHORIZED, "a bad token 401");
    assert_eq!(
        error_text(&bogus),
        "The access token is invalid",
        "the 401 names the invalid credential"
    );
}

/// An app-only (`client_credentials`) token may register: Pleroma's
/// `create` needs the app behind the token, not a user, and an unstated
/// `reason` stays `NULL` beside the row.
#[tokio::test]
async fn an_app_only_token_registers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let body = register_one(&app, &pool, "appkid").await;
    assert!(
        user::get_by_nickname(&pool, "appkid")
            .await
            .expect("the lookup runs")
            .is_some(),
        "the account row exists"
    );
    let (reason,): (Option<String>,) =
        sqlx::query_as("SELECT registration_reason FROM users WHERE nickname = $1::citext")
            .bind("appkid")
            .fetch_one(&pool)
            .await
            .expect("the column reads back");
    assert!(reason.is_none(), "an unstated reason stays NULL");
    assert!(body["access_token"].is_string(), "and a token answered");
}

/// A bare `write` token satisfies `write:accounts` through the Mastodon
/// hierarchy, so the gate accepts broader grants, and the token it gets
/// back carries those scopes.
#[tokio::test]
async fn a_write_token_is_enough() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = common::create_app_only_token(&pool, "write").await;
    let response = register_json(&app, &token, &body_for("writable")).await;
    assert_eq!(response.status, StatusCode::OK, "`write` covers the gate");
    assert_eq!(
        response.json_value()["scope"],
        "write",
        "the issued token copies the presenting scopes"
    );
}
