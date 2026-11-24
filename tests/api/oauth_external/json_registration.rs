//! The JSON registration of an external login: `GET
//! /oauth/registration_details` with `Accept: application/json` and `POST
//! /oauth/register` with a JSON body, ending in a redeemable code, a
//! pending wait, or a `POST /api/v1/accounts`-worded refusal.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use wired_be::models::{
    oauth_token,
    user::{self, register},
};

use super::provider::{PROVIDER, REDIRECT, UNLINKED_UID};
use super::support::{
    assert_no_store, count_codes, count_identities, count_states, count_users, fixture,
    pending_nickname, pending_state, prepare, query_param,
};
use crate::common;

/// `GET /oauth/registration_details` asking for JSON.
async fn details(http: &Router, query: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/oauth/registration_details{query}"))
        .header("accept", "application/json")
        .send(http)
        .await
}

/// `POST /oauth/register` with `body` as JSON.
async fn register_json(http: &Router, body: &Value) -> common::TestResponse {
    common::TestRequest::post("/oauth/register")
        .json(body)
        .send(http)
        .await
}

/// A pending state answers the provider's account info as JSON, under
/// no-store, with the provider's name.
#[tokio::test]
async fn registration_details_answers_the_pending_account_as_json() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "jade", "read").await;
    let state = pending_state(&site.http, &site.app, "").await;

    let response = details(&site.http, &format!("?state={state}")).await;
    assert_eq!(response.status, StatusCode::OK, "the details answer");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "as JSON, not the page"
    );
    assert_no_store(&response);
    assert_eq!(
        response.json_value(),
        json!({
            "nickname": pending_nickname(),
            "email": format!("user-{UNLINKED_UID}@example.org"),
            "name": format!("User {UNLINKED_UID}"),
            "provider": PROVIDER,
        }),
        "exactly the pending account's four facts"
    );
    site.provider.stop();
}

/// A state that cannot register is the 400 `Invalid state` as JSON: never
/// issued, empty, absent, and live without a pending account.
#[tokio::test]
async fn registration_details_json_refuses_a_bad_state() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "kira", "read").await;
    let bare = prepare(&site.http, &site.app, "").await;

    for (query, why) in [
        ("?state=never-issued".to_owned(), "never issued"),
        ("?state=".to_owned(), "empty"),
        (String::new(), "absent"),
        (format!("?state={bare}"), "live but nothing pending"),
    ] {
        let response = details(&site.http, &query).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{why}");
        assert_eq!(response.error_text(), "Invalid state", "for {why}");
        assert_no_store(&response);
    }
    site.provider.stop();
}

/// The JSON registration creates the account and answers the redirect with
/// a code its application redeems, the client's state echoed; the identity
/// is linked and the state is spent.
#[tokio::test]
async fn json_register_answers_the_redirect_with_a_code() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "luna", "read write").await;
    let state = pending_state(&site.http, &site.app, "&state=client-state").await;

    let response = register_json(
        &site.http,
        &json!({"state": state, "nickname": "octocat", "email": "octocat@example.org"}),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "JSON, not a redirect"
    );
    assert_no_store(&response);
    let url = response.json_value()["redirect_uri"]
        .as_str()
        .expect("a redirect_uri")
        .to_owned();
    let code = query_param(&url, "code").expect("a code on the redirect");
    assert_eq!(
        url,
        format!("{REDIRECT}?code={code}&state=client-state"),
        "the registered URI, the code and the client's state"
    );

    let redeemed = oauth_token::consume_authorization(&pool, &code, site.app.id, Some(REDIRECT))
        .await
        .expect("the redemption reaches the database")
        .expect("the owning app can redeem the code");
    let created = user::get_by_nickname(&pool, "octocat")
        .await
        .expect("the lookup runs")
        .expect("the registration created the user");
    assert_eq!(redeemed.user_id, created.id, "the code names the new user");
    assert_eq!(count_states(&pool).await, 0, "the state is spent");
    assert_eq!(count_identities(&pool).await, 1, "and the identity linked");
    site.provider.stop();
}

/// A password in the body is the account's own: it logs in by password,
/// which the random one of the form never does. A too-short one is refused
/// like any registration's.
#[tokio::test]
async fn json_register_takes_a_chosen_password() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "mara", "read").await;
    let state = pending_state(&site.http, &site.app, "").await;

    let short = register_json(
        &site.http,
        &json!({"state": state, "nickname": "octocat", "password": "x"}),
    )
    .await;
    assert_eq!(short.status, StatusCode::UNPROCESSABLE_ENTITY, "too short");
    assert!(
        short.error_text().contains("password"),
        "the password is named: {}",
        short.error_text()
    );

    let response = register_json(
        &site.http,
        &json!({
            "state": state,
            "nickname": "octocat",
            "password": "correct horse battery staple",
        }),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    assert!(
        register::authenticate(&pool, "octocat", "correct horse battery staple")
            .await
            .is_some(),
        "the chosen password logs the account in"
    );
    site.provider.stop();
}

/// A taken nickname is the 422 `POST /api/v1/accounts` answers, its `error`
/// the JSON text of the field map; nothing is created and the state stays
/// live for the retry.
#[tokio::test]
async fn json_register_refuses_a_taken_nickname_with_a_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "nora", "read").await;
    let state = pending_state(&site.http, &site.app, "").await;

    let response = register_json(
        &site.http,
        &json!({"state": state, "nickname": "nora", "email": "n@example.org"}),
    )
    .await;
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY, "refused");
    assert_eq!(
        response.error_text(),
        r#"{"nickname":["has already been taken"]}"#,
        "the accounts endpoint's wording"
    );
    assert_eq!(count_users(&pool).await, 1, "only the fixture user");
    assert_eq!(count_identities(&pool).await, 0, "nothing was linked");
    assert_eq!(
        count_states(&pool).await,
        1,
        "the state is kept for a retry"
    );
    assert_eq!(count_codes(&pool).await, 0, "and no code was minted");

    let retry = register_json(
        &site.http,
        &json!({"state": state, "nickname": "fresh", "email": "n@example.org"}),
    )
    .await;
    assert_eq!(retry.status, StatusCode::OK, "the retry succeeds");
    site.provider.stop();
}

/// A state that was never issued, or is empty, is refused as JSON.
#[tokio::test]
async fn json_register_refuses_bad_states() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "odin", "read").await;
    for state in ["never-issued", ""] {
        let response = register_json(&site.http, &json!({"state": state, "nickname": "x"})).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "state `{state}`");
        assert_eq!(response.error_text(), "Invalid state", "for `{state}`");
    }
    assert_eq!(count_users(&pool).await, 1, "no user was created");
    site.provider.stop();
}
