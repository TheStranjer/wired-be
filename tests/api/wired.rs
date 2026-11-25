//! `GET /api/v1/wired/protocols`: the account's identity on each enabled
//! protocol, and a suggested ID where it has none.
//!
//! The fixture protocol keeps its taken IDs in a set shared by every test in
//! the process, so each test that marks one uses a nickname no other test uses.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use wired_be::{
    config::{
        BASE_URL_VAR, Config, DATABASE_URL_VAR, PROTOCOLS_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
    },
    models::activitypub_actor::{ActivityPubActorWrite, ActorStatus},
    protocols::fixture::mark_taken,
    util::keys::KeyPair,
};

use crate::common;

/// The path under test.
const ROUTE: &str = "/api/v1/wired/protocols";

/// The test configuration with `PROTOCOLS` set to `activitypub,fixture`.
fn config_with_fixture() -> Config {
    Config::from_lookup(|key| match key {
        DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
        BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
        PROTOCOLS_VAR => Some("activitypub,fixture".to_owned()),
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
