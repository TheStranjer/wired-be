//! Integration tests for `POST /api/v1/accounts` (registration,
//! Pleroma's `AccountController.create`).
//!
//! The route takes any valid bearer token — app-only or user-bound —
//! with the `write:accounts` scope: no token is the 401 "Invalid
//! credentials", a scopeless token the 403 "Insufficient permissions:
//! write:accounts.", and a closed `REGISTRATIONS_OPEN` the 403
//! "Registration is disabled" — unless `INVITES_ENABLED` is on, when a
//! valid invite `token` registers instead and every refused invite
//! (absent, unknown, spent, expired, exhausted) is the 400 "Invalid
//! token". An accepted invite is consumed in the user's own insert
//! transaction, and an open instance ignores the parameter: an invalid
//! token still registers, and no invite is ever spent. A well-formed
//! body lands a local account through `create_local_user` and answers
//! the token JSON (Bearer, the presenting token's scopes, the token
//! row's id, `expires_in` and a `refresh_token`); the issued access
//! token authenticates `GET /api/v1/accounts/verify_credentials` for the
//! new account. Validation failures — absent or false `agreement`, a
//! duplicate nickname, an invalid email, a short password — answer 422s,
//! the model's carrying the JSON *string* of the field map in `error`.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use chrono::{NaiveDate, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::{Config, Toggle},
    models::{
        invite_token::{self, InviteParams, InviteToken},
        user,
    },
};

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

/// Today on the instance clock: invite expiries are calendar dates.
fn today() -> NaiveDate {
    Utc::now().date_naive()
}

/// A closed instance that takes invites: `REGISTRATIONS_OPEN=false`,
/// `INVITES_ENABLED=true`.
fn invite_config() -> Config {
    let mut config = common::test_config();
    config.registrations_open = Toggle::new(false);
    config.invites_enabled = Toggle::new(true);
    config
}

/// The router of [`invite_config`], mounted over `pool`.
fn invite_app(pool: &PgPool) -> Router {
    common::test_app_with_config(pool.clone(), invite_config())
}

/// Mints an invite through the model and returns its row.
async fn mint_invite(pool: &PgPool, params: &InviteParams) -> InviteToken {
    invite_token::create(pool, params)
        .await
        .expect("the invite mints")
}

/// The invite row behind `invite`, re-read from storage.
async fn reload_invite(pool: &PgPool, invite: &InviteToken) -> InviteToken {
    invite_token::get_by_token(pool, &invite.token)
        .await
        .expect("the lookup runs")
        .expect("the invite is still stored")
}

/// Registers `username` on the invite-gated `app`, submitting `token`
/// as the body's invite, over a fresh app-only bearer.
async fn register_with_invite(
    app: &Router,
    pool: &PgPool,
    username: &str,
    token: &str,
) -> common::TestResponse {
    let bearer = common::create_app_only_token(pool, FULL_SCOPES).await;
    register_json(app, &bearer, &body_with(username, &json!({"token": token}))).await
}

/// The 400 body an invite refusal carries, for `who`.
fn assert_invalid_token(response: &common::TestResponse, who: &str) {
    common::assert_answer(response, StatusCode::BAD_REQUEST, who);
    common::assert_error(response, "Invalid token", who);
}

/// The number of rows in `users`, for "nobody registered" proofs.
async fn user_count(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM users")
        .fetch_one(pool)
        .await
        .expect("users can be counted");
    row.0
}

/// A one-time invite opens the closed gate exactly once: the first
/// registration lands an active, approval-skipping account and spends
/// the invite (`uses` 1, `used`), and the same token then registers
/// nobody.
#[tokio::test]
async fn a_one_time_invite_registers_once_and_is_spent() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = invite_app(&pool);
    let invite = mint_invite(&pool, &InviteParams::one_time()).await;

    let first = register_with_invite(&app, &pool, "invited", &invite.token).await;
    assert_eq!(
        first.status,
        StatusCode::OK,
        "a valid invite registers: {}",
        first.text()
    );
    let stored = user::get_by_nickname(&pool, "invited")
        .await
        .expect("the lookup runs")
        .expect("the invited account landed");
    assert!(stored.is_active.get(), "an invited user is active");
    assert!(
        !stored.approval_pending.get(),
        "an invited user skips approval"
    );

    let spent = reload_invite(&pool, &invite).await;
    assert_eq!(spent.uses, 1, "the registration was counted");
    assert!(spent.used, "a one-time invite is spent by its use");
    assert!(!spent.is_valid(today()), "and it is no longer valid");

    let again = register_with_invite(&app, &pool, "twice", &invite.token).await;
    assert_invalid_token(&again, "the spent invite");
    assert!(
        user::get_by_nickname(&pool, "twice")
            .await
            .expect("the lookup runs")
            .is_none(),
        "the spent invite registered nobody"
    );
}

/// A reusable invite counts every registration and stops exactly at its
/// cap: two serves a `max_use` of two, the third is refused, and the row
/// is marked used at the cap.
#[tokio::test]
async fn a_reusable_invite_serves_up_to_its_cap() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = invite_app(&pool);
    let params = InviteParams {
        expires_at: None,
        max_use: Some(2),
    };
    let invite = mint_invite(&pool, &params).await;

    let first = register_with_invite(&app, &pool, "reuse1", &invite.token).await;
    assert_eq!(first.status, StatusCode::OK, "the first serve passes");
    let once = reload_invite(&pool, &invite).await;
    assert_eq!(once.uses, 1, "one of two counted");
    assert!(!once.used, "short of the cap the invite stays open");

    let second = register_with_invite(&app, &pool, "reuse2", &invite.token).await;
    assert_eq!(second.status, StatusCode::OK, "the second serve passes");
    let twice = reload_invite(&pool, &invite).await;
    assert_eq!(twice.uses, 2, "both counted");
    assert!(twice.used, "reaching the cap marks the invite used");

    let third = register_with_invite(&app, &pool, "reuse3", &invite.token).await;
    assert_invalid_token(&third, "the exhausted invite");
    assert_eq!(
        user_count(&pool).await,
        2,
        "exactly the two invited accounts"
    );
}

/// A date-limited invite serves every registration up to and including
/// its day — counting uses but never marking `used` — and the day after
/// its expiry it is the 400.
#[tokio::test]
async fn a_date_limited_invite_serves_until_its_day() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = invite_app(&pool);
    let params = InviteParams {
        expires_at: Some(today()),
        max_use: None,
    };
    let invite = mint_invite(&pool, &params).await;

    let first = register_with_invite(&app, &pool, "dated1", &invite.token).await;
    assert_eq!(first.status, StatusCode::OK, "the expiry day is still open");
    let second = register_with_invite(&app, &pool, "dated2", &invite.token).await;
    assert_eq!(second.status, StatusCode::OK, "a date limit serves twice");
    let after = reload_invite(&pool, &invite).await;
    assert_eq!(after.uses, 2, "both counted");
    assert!(
        !after.used,
        "a date-limited invite is spent by its date, not its use"
    );
    assert!(
        after.is_valid(today()),
        "still valid on the expiry day itself"
    );
    assert!(
        !after.is_valid(today() + chrono::Duration::days(1)),
        "and invalid the day after"
    );

    let lapsed = InviteParams {
        expires_at: Some(today() - chrono::Duration::days(1)),
        max_use: None,
    };
    let expired = mint_invite(&pool, &lapsed).await;
    let refused = register_with_invite(&app, &pool, "latershow", &expired.token).await;
    assert_invalid_token(&refused, "the expired invite");
    assert_eq!(
        user_count(&pool).await,
        2,
        "the expired invite registered nobody"
    );
}

/// Every refusal the invite gate makes is the same 400 "Invalid token":
/// an absent parameter, an unknown text, an expired date, a used row and
/// an exhausted cap — and none of them stores anybody or spends
/// anything.
#[tokio::test]
async fn every_refused_invite_is_a_400_invalid_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = invite_app(&pool);

    let absent = {
        let bearer = common::create_app_only_token(&pool, FULL_SCOPES).await;
        register_json(&app, &bearer, &body_for("nobody1")).await
    };
    let unknown = register_with_invite(&app, &pool, "nobody2", "never-minted-text").await;

    let expired = mint_invite(
        &pool,
        &InviteParams {
            expires_at: Some(today() - chrono::Duration::days(1)),
            max_use: Some(5),
        },
    )
    .await;
    let expired_answer = register_with_invite(&app, &pool, "nobody3", &expired.token).await;

    let used = mint_invite(&pool, &InviteParams::one_time()).await;
    assert!(
        invite_token::consume(&pool, used.id)
            .await
            .expect("the model spends the invite"),
        "the fresh one-time invite is spent"
    );
    let used_answer = register_with_invite(&app, &pool, "nobody4", &used.token).await;

    let exhausted = mint_invite(
        &pool,
        &InviteParams {
            expires_at: None,
            max_use: Some(1),
        },
    )
    .await;
    assert!(
        invite_token::consume(&pool, exhausted.id)
            .await
            .expect("the model reaches the cap"),
        "the capped invite's only use is served"
    );
    let exhausted_answer = register_with_invite(&app, &pool, "nobody5", &exhausted.token).await;

    common::assert_all_refused(
        &[
            absent,
            unknown,
            expired_answer,
            used_answer,
            exhausted_answer,
        ],
        StatusCode::BAD_REQUEST,
        "Invalid token",
        "every invite refusal",
    );
    assert_eq!(user_count(&pool).await, 0, "nobody registered");
    assert_eq!(
        reload_invite(&pool, &used).await.uses,
        1,
        "the pre-spent one-time invite counted only the fixture consume"
    );
    assert_eq!(
        reload_invite(&pool, &expired).await.uses,
        0,
        "a refused registration spends nothing"
    );
}

/// While `INVITES_ENABLED` is off a closed gate keeps the
/// registration-ticket answer even against a freshly minted valid
/// invite — the 403, and the invite stays untouched.
#[tokio::test]
async fn closed_without_invites_refuses_even_a_valid_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = common::test_config();
    config.registrations_open = Toggle::new(false);
    let app = common::test_app_with_config(pool.clone(), config);
    let invite = mint_invite(&pool, &InviteParams::one_time()).await;

    let response = register_with_invite(&app, &pool, "wishful", &invite.token).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN, "403 when closed");
    assert_eq!(
        error_text(&response),
        "Registration is disabled",
        "the pinned wording"
    );
    assert_eq!(user_count(&pool).await, 0, "nothing registered");
    let untouched = reload_invite(&pool, &invite).await;
    assert_eq!(untouched.uses, 0, "a refused gate spends nothing");
    assert!(!untouched.used, "and marks nothing used");
}

/// With `REGISTRATIONS_OPEN` true the token is optional and ignored: a
/// valid invite registers but is never checked or consumed, and a
/// nonsense one changes nothing about the open answer.
#[tokio::test]
async fn open_registrations_ignore_invite_tokens() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let invite = mint_invite(&pool, &InviteParams::one_time()).await;

    let good = register_with_invite(&app, &pool, "shownone", &invite.token).await;
    assert_eq!(good.status, StatusCode::OK, "an open gate takes the body");
    let ignored = reload_invite(&pool, &invite).await;
    assert_eq!(ignored.uses, 0, "an open registration spends nothing");
    assert!(!ignored.used, "and marks nothing used");

    let nonsense = register_with_invite(&app, &pool, "shownull", "not-an-invite").await;
    assert_eq!(
        nonsense.status,
        StatusCode::OK,
        "and an invalid token is no obstacle while open"
    );
    assert_eq!(user_count(&pool).await, 2, "both accounts landed");
}

/// The invite is consumed in the user's own transaction: a registration
/// the model rejects (a too-short password) is the 422 and the invite
/// keeps its count and its flag exactly as they were.
#[tokio::test]
async fn a_rejected_invited_registration_spends_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = invite_app(&pool);
    let params = InviteParams {
        expires_at: Some(today()),
        max_use: Some(2),
    };
    let invite = mint_invite(&pool, &params).await;

    let bearer = common::create_app_only_token(&pool, FULL_SCOPES).await;
    let response = register_json(
        &app,
        &bearer,
        &body_with(
            "nospend",
            &json!({"token": invite.token, "password": "tiny"}),
        ),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the field-map 422 still answers"
    );
    let after = reload_invite(&pool, &invite).await;
    assert_eq!(after.uses, 0, "the rolled-back transaction spent nothing");
    assert!(!after.used, "and marked nothing");
    assert_eq!(user_count(&pool).await, 0, "and stored nobody");
}

/// The frontend origin the confirmation-link tests configure, distinct from
/// the test `BASE_URL` so a redirect to the wrong origin shows.
const FRONTEND: &str = "https://fe.example";

/// A router whose instance requires email confirmation and serves its
/// frontend from [`FRONTEND`], with `SMTP_HOST` set so the letter queues.
fn confirmation_app(pool: &PgPool) -> Router {
    let config = Config {
        account_activation_required: Toggle::new(true),
        smtp_host: Some("smtp.example.org".to_owned()),
        frontend_base_url: FRONTEND.to_owned(),
        ..common::test_config()
    };
    common::test_app_with_config(pool.clone(), config)
}

/// The stored row of `username`.
async fn stored_user(pool: &PgPool, username: &str) -> user::User {
    user::get_by_nickname(pool, username)
        .await
        .expect("the lookup reaches the database")
        .unwrap_or_else(|| panic!("no account {username} was registered"))
}

/// A valid confirmation link confirms the address and redirects with a 302
/// to the frontend's root, not to `BASE_URL`.
#[tokio::test]
async fn a_valid_confirmation_link_redirects_to_the_frontend() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = confirmation_app(&pool);
    let bearer = common::create_app_only_token(&pool, FULL_SCOPES).await;
    let registered = register_json(&app, &bearer, &body_for("fefriend")).await;
    assert_eq!(registered.status, StatusCode::OK, "the registration lands");
    let row = stored_user(&pool, "fefriend").await;
    assert!(!row.email_confirmed.get(), "the account starts unconfirmed");
    let token = row
        .confirmation_token
        .clone()
        .expect("the registration minted a token");

    let response =
        common::TestRequest::get(&format!("/api/account/confirm_email/{}/{token}", row.id))
            .send(&app)
            .await;

    assert_eq!(response.status, StatusCode::FOUND, "an explicit 302");
    assert_eq!(
        response.header("location"),
        Some("https://fe.example/"),
        "to the frontend's root"
    );
    assert!(
        stored_user(&pool, "fefriend").await.email_confirmed.get(),
        "and the address is confirmed"
    );
}

/// An invalid token redirects to the same place and changes nothing.
#[tokio::test]
async fn an_invalid_confirmation_token_redirects_to_the_frontend_unchanged() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = confirmation_app(&pool);
    let bearer = common::create_app_only_token(&pool, FULL_SCOPES).await;
    let registered = register_json(&app, &bearer, &body_for("fefoe")).await;
    assert_eq!(registered.status, StatusCode::OK, "the registration lands");
    let row = stored_user(&pool, "fefoe").await;

    let response = common::TestRequest::get(&format!(
        "/api/account/confirm_email/{}/not-the-token",
        row.id
    ))
    .send(&app)
    .await;

    assert_eq!(response.status, StatusCode::FOUND, "still a 302");
    assert_eq!(
        response.header("location"),
        Some("https://fe.example/"),
        "to the same frontend root"
    );
    let after = stored_user(&pool, "fefoe").await;
    assert!(!after.email_confirmed.get(), "nothing was confirmed");
    assert_eq!(
        after.confirmation_token, row.confirmation_token,
        "and the pending token is untouched"
    );
}
