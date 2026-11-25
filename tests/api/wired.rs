//! `GET /api/v1/wired/protocols`: the account's identity on each enabled
//! protocol, and a suggested ID where it has none; `POST
//! /api/v1/wired/protocols/{protocol}/claim`, which gets an account onto a
//! protocol later; and registration's `wired_identifiers`, which claims IDs
//! up front.
//!
//! The fixture protocol keeps its taken IDs in a set shared by every test in
//! the process, so each test that marks one uses a nickname no other test uses.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use wired_be::{
    config::{
        BASE_URL_VAR, Config, DATABASE_URL_VAR, PROTOCOLS_VAR, RATE_LIMITS_ENABLED_VAR,
        REGISTRATIONS_OPEN_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
    },
    models::{
        activitypub_actor::{ActivityPubActorWrite, ActorStatus},
        user,
    },
    protocols::fixture::mark_taken,
    test_db::TestDb,
    util::keys::KeyPair,
};

use crate::common;

/// The path under test.
const ROUTE: &str = "/api/v1/wired/protocols";

/// The test configuration with `PROTOCOLS` set to `activitypub,fixture`.
///
/// Registrations are open and rate limits off, as in the default test configuration.
fn config_with_fixture() -> Config {
    Config::from_lookup(|key| match key {
        DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
        BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
        PROTOCOLS_VAR => Some("activitypub,fixture".to_owned()),
        REGISTRATIONS_OPEN_VAR => Some("true".to_owned()),
        RATE_LIMITS_ENABLED_VAR => Some("false".to_owned()),
        _ => None,
    })
    .expect("the fixed protocols test config is valid")
}

/// `GET /api/v1/wired/protocols` as `token`.
async fn protocols(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get(ROUTE)
        .bearer(token)
        .send(app)
        .await
}

/// With the default config the one entry is the account's active `ActivityPub` identity.
#[tokio::test]
async fn the_default_config_lists_the_activitypub_identity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_federating_user_with(&pool, "bob", |new| new).await;
    let token = common::create_app_and_token(&pool, &bob, "read:accounts").await;
    let app = common::test_app(pool);

    let response = protocols(&app, &token).await;

    common::assert_answer(&response, StatusCode::OK, "the owner's read token");
    assert_eq!(
        response.json_value(),
        json!([{
            "protocol": "activitypub",
            "status": "active",
            "identifier": "bob",
            "uri": bob.ap_id,
            "suggestion": null,
        }])
    );
}

/// An `ActivityPub` identity that is still pending says so.
#[tokio::test]
async fn a_pending_identity_is_reported_as_pending() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_local_user(&pool, "bob").await;
    let keys = KeyPair {
        private_pem: "PEM".to_owned(),
        public_pem: "PUB".to_owned(),
    };
    let write = ActivityPubActorWrite {
        status: ActorStatus::Pending,
        ..ActivityPubActorWrite::for_local_account(&bob, &common::test_config().base_url, keys)
    };
    let _row = common::write_actor(&pool, &write).await;
    let token = common::create_app_and_token(&pool, &bob, "read").await;
    let app = common::test_app(pool);

    let response = protocols(&app, &token).await;

    common::assert_answer(&response, StatusCode::OK, "the broad read token");
    let entries = response.json_value();
    assert_eq!(entries[0]["status"], "pending");
    assert_eq!(entries[0]["identifier"], "bob");
    assert_eq!(entries[0]["suggestion"], Value::Null);
}

/// An account on `ActivityPub` only is unclaimed on the fixture, with a suggestion
/// derived from its nickname.
#[tokio::test]
async fn an_unclaimed_protocol_carries_a_derived_suggestion() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let guy = common::create_federating_user_with(&pool, "Cool_Guy", |new| new).await;
    let token = common::create_app_and_token(&pool, &guy, "read:accounts").await;
    let app = common::test_app_with_config(pool, config_with_fixture());

    let response = protocols(&app, &token).await;

    common::assert_answer(&response, StatusCode::OK, "the owner's read token");
    assert_eq!(
        response.json_value(),
        json!([
            {
                "protocol": "activitypub",
                "status": "active",
                "identifier": "Cool_Guy",
                "uri": guy.ap_id,
                "suggestion": null,
            },
            {
                "protocol": "fixture",
                "status": "unclaimed",
                "identifier": null,
                "uri": null,
                "suggestion": "cool-guy",
            },
        ])
    );
}

/// A derived ID that is taken gets the `-2` suffix.
#[tokio::test]
async fn a_taken_suggestion_is_made_unique() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let twin = common::create_federating_user_with(&pool, "Taken_Twin", |new| new).await;
    mark_taken("taken-twin");
    let token = common::create_app_and_token(&pool, &twin, "read:accounts").await;
    let app = common::test_app_with_config(pool, config_with_fixture());

    let response = protocols(&app, &token).await;

    common::assert_answer(&response, StatusCode::OK, "the owner's read token");
    assert_eq!(
        response.json_value()[1]["suggestion"],
        "taken-twin-2",
        "the entry for the fixture"
    );
}

/// An account with no `ActivityPub` identity is unclaimed there too.
///
/// Its own nickname already counts as taken there, so the suggestion gets a suffix.
#[tokio::test]
async fn an_account_without_an_activitypub_row_is_unclaimed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = common::create_local_user(&pool, "carol").await;
    let token = common::create_app_and_token(&pool, &carol, "read:accounts").await;
    let app = common::test_app(pool);

    let response = protocols(&app, &token).await;

    common::assert_answer(&response, StatusCode::OK, "the owner's read token");
    assert_eq!(
        response.json_value(),
        json!([{
            "protocol": "activitypub",
            "status": "unclaimed",
            "identifier": null,
            "uri": null,
            "suggestion": "carol-2",
        }])
    );
}

/// No token is 401; an app-only token or one without a read scope is 403.
#[tokio::test]
async fn the_route_is_gated_by_auth_and_the_read_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_federating_user_with(&pool, "bob", |new| new).await;
    let wrong = common::create_app_and_token(&pool, &bob, "write:accounts read:statuses").await;
    let app = common::test_app(pool);

    let anonymous = common::TestRequest::get(ROUTE).send(&app).await;
    let refused = protocols(&app, &wrong).await;

    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "a token without the scope");
}

/// The scopes the claim tests' tokens carry: they read the entries and write the claim.
const CLAIM_SCOPES: &str = "read:accounts write:accounts";

/// The `error` text of a 422 whose field map is `{field: [message]}`.
fn field_error(field: &str, message: &str) -> String {
    format!(r#"{{"{field}":["{message}"]}}"#)
}

/// `POST /api/v1/wired/protocols/{protocol}/claim` as `token`, with `body` as JSON if given.
async fn claim(
    app: &Router,
    token: &str,
    protocol: &str,
    body: Option<&Value>,
) -> common::TestResponse {
    let request = common::TestRequest::post(&format!("{ROUTE}/{protocol}/claim")).bearer(token);
    match body {
        Some(body) => request.json(body),
        None => request,
    }
    .send(app)
    .await
}

/// An app running the fixture, and a token for a new account named `nickname` on it.
///
/// The account is on `ActivityPub` only, as one registered before the fixture was enabled.
async fn late_enrollee(db: &TestDb, nickname: &str) -> (Router, String) {
    let pool = db.pool().clone();
    let user = common::create_federating_user_with(&pool, nickname, |new| new).await;
    let token = common::create_app_and_token(&pool, &user, CLAIM_SCOPES).await;
    (
        common::test_app_with_config(pool, config_with_fixture()),
        token,
    )
}

/// Claiming the fixture without a body takes the suggestion, and the entry then reads `active`.
#[tokio::test]
async fn claiming_without_a_body_uses_the_suggestion() {
    let db = common::test_db().await;
    let (app, token) = late_enrollee(&db, "Claim_Plain").await;

    let claimed = claim(&app, &token, "fixture", None).await;
    let listed = protocols(&app, &token).await;

    common::assert_answer(&claimed, StatusCode::OK, "a free suggestion");
    assert_eq!(
        claimed.json_value(),
        json!({
            "protocol": "fixture",
            "status": "active",
            "identifier": "claim-plain",
            "uri": "fixture:claim-plain",
            "suggestion": null,
        }),
        "the entry GET lists for the protocol"
    );
    assert_eq!(
        listed.json_value()[1],
        claimed.json_value(),
        "the list shows the same entry"
    );
}

/// A blank identifier asks for the suggestion too.
#[tokio::test]
async fn claiming_a_blank_identifier_uses_the_suggestion() {
    let db = common::test_db().await;
    let (app, token) = late_enrollee(&db, "Claim_Blank").await;

    let claimed = claim(&app, &token, "fixture", Some(&json!({"identifier": "  "}))).await;

    common::assert_answer(&claimed, StatusCode::OK, "a blank identifier");
    assert_eq!(claimed.json_value()["identifier"], "claim-blank");
}

/// A given identifier is validated, normalised and claimed instead of the suggestion.
#[tokio::test]
async fn claiming_a_given_identifier_uses_it() {
    let db = common::test_db().await;
    let (app, token) = late_enrollee(&db, "Claim_Given").await;

    let claimed = claim(
        &app,
        &token,
        "fixture",
        Some(&json!({"identifier": " Chosen-Claim "})),
    )
    .await;

    common::assert_answer(&claimed, StatusCode::OK, "a free identifier");
    assert_eq!(claimed.json_value()["identifier"], "chosen-claim");
    assert_eq!(claimed.json_value()["status"], "active");
}

/// A taken identifier is a 422 under the protocol's field, and the account stays unclaimed.
#[tokio::test]
async fn claiming_a_taken_identifier_is_unprocessable() {
    let db = common::test_db().await;
    let (app, token) = late_enrollee(&db, "Claim_Taker").await;
    mark_taken("claim-taken-one");

    let refused = claim(
        &app,
        &token,
        "fixture",
        Some(&json!({"identifier": "claim-taken-one"})),
    )
    .await;
    let listed = protocols(&app, &token).await;

    common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, "a taken ID");
    assert_eq!(
        refused.error_text(),
        field_error("fixture_identifier", "has already been taken")
    );
    assert_eq!(listed.json_value()[1]["status"], "unclaimed");
}

/// An identifier the protocol rejects is a 422 carrying the protocol's own message.
#[tokio::test]
async fn claiming_a_bad_identifier_is_unprocessable() {
    let db = common::test_db().await;
    let (app, token) = late_enrollee(&db, "Claim_Badger").await;

    let refused = claim(
        &app,
        &token,
        "fixture",
        Some(&json!({"identifier": "bad_id"})),
    )
    .await;

    common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, "a bad ID");
    assert_eq!(
        refused.error_text(),
        field_error("fixture_identifier", "may only contain a-z, 0-9 and -")
    );
}

/// A second claim on the same protocol is a 422 and keeps the first identity.
#[tokio::test]
async fn claiming_twice_is_unprocessable() {
    let db = common::test_db().await;
    let (app, token) = late_enrollee(&db, "Claim_Twice").await;
    let first = claim(&app, &token, "fixture", None).await;

    let second = claim(
        &app,
        &token,
        "fixture",
        Some(&json!({"identifier": "other-id"})),
    )
    .await;
    let listed = protocols(&app, &token).await;

    common::assert_answer(&first, StatusCode::OK, "the first claim");
    common::assert_answer(
        &second,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the second claim",
    );
    assert_eq!(
        second.error_text(),
        field_error(
            "fixture_identifier",
            "has already been claimed by this account"
        )
    );
    assert_eq!(listed.json_value()[1]["identifier"], "claim-twice");
}

/// An account already on `ActivityPub` cannot claim it again.
#[tokio::test]
async fn claiming_activitypub_when_enrolled_is_unprocessable() {
    let db = common::test_db().await;
    let (app, token) = late_enrollee(&db, "Claim_Enrolled").await;

    let refused = claim(&app, &token, "activitypub", None).await;

    common::assert_answer(
        &refused,
        StatusCode::UNPROCESSABLE_ENTITY,
        "already enrolled",
    );
    assert_eq!(
        refused.error_text(),
        field_error("nickname", "has already been claimed by this account")
    );
}

/// An unknown protocol id, or one the instance has not enabled, is a 404.
#[tokio::test]
async fn claiming_an_unknown_or_disabled_protocol_is_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = common::create_federating_user_with(&pool, "Claim_Missing", |new| new).await;
    let token = common::create_app_and_token(&pool, &user, CLAIM_SCOPES).await;
    let default_app = common::test_app(pool.clone());
    let fixture_app = common::test_app_with_config(pool, config_with_fixture());

    let unknown = claim(&fixture_app, &token, "nostr", None).await;
    let disabled = claim(&default_app, &token, "fixture", None).await;

    common::assert_answer(&unknown, StatusCode::NOT_FOUND, "an unknown protocol");
    common::assert_answer(&disabled, StatusCode::NOT_FOUND, "a disabled protocol");
}

/// No token is 401; a token without the write scope is 403.
#[tokio::test]
async fn the_claim_route_is_gated_by_auth_and_the_write_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let user = common::create_federating_user_with(&pool, "Claim_Gated", |new| new).await;
    let readonly = common::create_app_and_token(&pool, &user, "read:accounts").await;
    let app = common::test_app_with_config(pool, config_with_fixture());

    let anonymous = common::TestRequest::post(&format!("{ROUTE}/fixture/claim"))
        .send(&app)
        .await;
    let refused = claim(&app, &readonly, "fixture", None).await;

    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "no write scope");
}

/// The scopes of the app-only token registrations come in with.
const REGISTRATION_SCOPES: &str = "read write follow push";

/// The registration body for `nickname`.
fn registration(nickname: &str) -> Value {
    json!({
        "username": nickname,
        "email": format!("{nickname}@example.org"),
        "password": "correct horse battery staple",
        "agreement": true,
    })
}

/// `POST /api/v1/accounts` as a fresh app-only token with `body`.
async fn register(app: &Router, pool: &sqlx::PgPool, body: &Value) -> common::TestResponse {
    let token = common::create_app_only_token(pool, REGISTRATION_SCOPES).await;
    common::TestRequest::post("/api/v1/accounts")
        .bearer(&token)
        .json(body)
        .send(app)
        .await
}

/// Registration claims the ID `wired_identifiers` asks for, and the new account lists it.
#[tokio::test]
async fn registration_claims_the_requested_identifier() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(pool.clone(), config_with_fixture());
    let mut body = registration("reg_custom");
    body["wired_identifiers"] = json!({"fixture": "custom-name"});

    let registered = register(&app, &pool, &body).await;
    let access_token = registered.json_value()["access_token"]
        .as_str()
        .expect("a ready account gets a token")
        .to_owned();
    let listed = protocols(&app, &access_token).await;

    common::assert_answer(&registered, StatusCode::OK, "a free requested ID");
    assert_eq!(listed.json_value()[1]["identifier"], "custom-name");
    assert_eq!(listed.json_value()[1]["status"], "active");
}

/// A form body carries `wired_identifiers[<protocol>]` the same way.
#[tokio::test]
async fn registration_reads_wired_identifiers_from_a_form() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(pool.clone(), config_with_fixture());
    let token = common::create_app_only_token(&pool, REGISTRATION_SCOPES).await;

    let registered = common::TestRequest::post("/api/v1/accounts")
        .bearer(&token)
        .form(&[
            ("username", "reg_form"),
            ("email", "reg_form@example.org"),
            ("password", "correct horse battery staple"),
            ("agreement", "true"),
            ("wired_identifiers[fixture]", "form-name"),
        ])
        .send(&app)
        .await;
    let access_token = registered.json_value()["access_token"]
        .as_str()
        .expect("a ready account gets a token")
        .to_owned();
    let listed = protocols(&app, &access_token).await;

    common::assert_answer(&registered, StatusCode::OK, "a form registration");
    assert_eq!(listed.json_value()[1]["identifier"], "form-name");
}

/// A requested ID that is taken fails the whole registration and creates no user.
#[tokio::test]
async fn registration_with_a_taken_identifier_creates_no_user() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(pool.clone(), config_with_fixture());
    mark_taken("reg-taken-name");
    let mut body = registration("reg_taken");
    body["wired_identifiers"] = json!({"fixture": "reg-taken-name"});

    let refused = register(&app, &pool, &body).await;

    common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, "a taken ID");
    assert_eq!(
        refused.error_text(),
        field_error("fixture_identifier", "has already been taken")
    );
    let stored = user::get_by_nickname(&pool, "reg_taken")
        .await
        .expect("the lookup runs");
    assert!(stored.is_none(), "no user was created");
}

/// A protocol id that is unknown or disabled is a 422 under `wired_identifiers`.
#[tokio::test]
async fn registration_refuses_unknown_and_disabled_protocols() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let mut body = registration("reg_unknown");
    body["wired_identifiers"] = json!({"fixture": "custom-name"});

    let refused = register(&app, &pool, &body).await;

    common::assert_answer(
        &refused,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a disabled protocol",
    );
    assert_eq!(
        refused.error_text(),
        field_error("wired_identifiers", "fixture is not an enabled protocol")
    );
    let stored = user::get_by_nickname(&pool, "reg_unknown")
        .await
        .expect("the lookup runs");
    assert!(stored.is_none(), "no user was created");
}

/// With the default config and no `wired_identifiers`, registration is unchanged.
#[tokio::test]
async fn registration_without_the_field_is_unchanged() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());

    let registered = register(&app, &pool, &registration("reg_plain")).await;
    let access_token = registered.json_value()["access_token"]
        .as_str()
        .expect("a ready account gets a token")
        .to_owned();
    let listed = protocols(&app, &access_token).await;

    common::assert_answer(&registered, StatusCode::OK, "a plain registration");
    assert_eq!(
        listed.json_value(),
        json!([{
            "protocol": "activitypub",
            "status": "active",
            "identifier": "reg_plain",
            "uri": format!("{}/users/reg_plain", common::test_config().base_url),
            "suggestion": null,
        }])
    );
}
