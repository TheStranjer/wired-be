//! The JSON registration under the registration modes: approval and email
//! confirmation answer a `pending` marker instead of a code, a closed
//! instance a 403.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use wired_be::{config::Toggle, models::user};

use super::support::{
    assert_no_store, count_codes, count_identities, count_users, fixture_with, pending_state,
};
use crate::common;

/// `POST /oauth/register` with a JSON body.
async fn register_json(http: &Router, body: &Value) -> common::TestResponse {
    common::TestRequest::post("/oauth/register")
        .json(body)
        .send(http)
        .await
}

/// `REGISTRATIONS_APPROVAL_REQUIRED`: the account exists and is linked, the
/// answer is `{"pending":"approval"}` and no code.
#[tokio::test]
async fn approval_mode_answers_pending_approval() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture_with(pool.clone(), "pax", "read", |config| {
        config.registrations_approval_required = Toggle::new(true);
    })
    .await;
    let state = pending_state(&site.http, &site.app, "").await;

    let response = register_json(
        &site.http,
        &json!({"state": state, "nickname": "octocat", "email": "octocat@example.org"}),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_no_store(&response);
    assert_eq!(
        response.json_value(),
        json!({"pending": "approval"}),
        "the approval wait"
    );
    let created = user::get_by_nickname(&pool, "octocat")
        .await
        .expect("the lookup runs")
        .expect("the registration created the user");
    assert!(
        created.approval_pending.get(),
        "the account awaits approval"
    );
    assert_eq!(count_codes(&pool).await, 0, "no code for an unapproved one");
    assert_eq!(
        count_identities(&pool).await,
        1,
        "but the identity is linked"
    );
    site.provider.stop();
}

/// `ACCOUNT_ACTIVATION_REQUIRED` with an email the provider does not vouch
/// for: `{"pending":"confirmation"}`.
#[tokio::test]
async fn activation_mode_answers_pending_confirmation() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture_with(pool.clone(), "quin", "read", |config| {
        config.account_activation_required = Toggle::new(true);
    })
    .await;
    let state = pending_state(&site.http, &site.app, "").await;

    let response = register_json(
        &site.http,
        &json!({"state": state, "nickname": "octocat", "email": "different@example.org"}),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        response.json_value(),
        json!({"pending": "confirmation"}),
        "the confirmation wait"
    );
    assert_eq!(count_codes(&pool).await, 0, "no code before confirming");
    site.provider.stop();
}

/// `REGISTRATIONS_OPEN` off: the 403 the accounts endpoint's closed
/// registration answers, nothing written, the state untouched.
#[tokio::test]
async fn closed_registrations_are_a_403() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture_with(pool.clone(), "ruth", "read", |config| {
        config.registrations_open = Toggle::new(false);
    })
    .await;
    let state = pending_state(&site.http, &site.app, "").await;

    let response = register_json(
        &site.http,
        &json!({"state": state, "nickname": "octocat", "email": "octocat@example.org"}),
    )
    .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN, "closed");
    assert_eq!(
        response.error_text(),
        "Registrations are closed",
        "the refusal line"
    );
    assert_eq!(count_users(&pool).await, 1, "only the fixture user exists");
    assert_eq!(count_identities(&pool).await, 0, "nothing was linked");
    site.provider.stop();
}
