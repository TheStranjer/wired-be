//! Notification settings (`PUT /api/pleroma/notification_settings`,
//! WBE-111) over the real router: the present-only flag updates, the
//! `{"status":"success"}` answer, the `pleroma.notification_settings`
//! render of the own-view Account, and the auth and scope gates.
//!
//! The suppression `block_from_strangers` applies when a notification is
//! created (`maybe_notify`) is covered at the model (`src/models/notification.rs`)
//! and end to end in `notification_generation.rs`, whose actions now run
//! through it.

#![cfg(test)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::user::{self, User};

use crate::common;

/// The endpoint under test.
const ROUTE: &str = "/api/pleroma/notification_settings";

/// Inserts a local user named `nickname` plus a bearer token granting
/// `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// The stored row of user `id`.
async fn stored(pool: &PgPool, id: Uuid) -> User {
    user::get_by_id(pool, id)
        .await
        .expect("the read runs")
        .expect("the fixture row exists")
}

/// The stored `notification_settings` blob rendered for comparison.
fn settings(user: &User) -> Value {
    user.notification_settings.clone()
}

/// `PUT` the JSON `body` for `token`.
async fn put_json(app: &axum::Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::put(ROUTE)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// Every flag starts off, each update moves only the flag it names, the
/// answer is `{"status":"success"}`, and an empty body changes nothing —
/// over JSON and over a form alike.
#[tokio::test]
async fn updates_move_only_the_named_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let app = common::test_app(pool.clone());

    assert_eq!(
        settings(&stored(&pool, alice.id).await),
        json!({"block_from_strangers": false, "hide_notification_contents": false}),
        "a fresh account starts with both flags off"
    );

    let first = put_json(&app, &token, json!({"block_from_strangers": true})).await;
    common::assert_answer(&first, StatusCode::OK, "the block update");
    assert_eq!(
        first.json_value(),
        json!({"status": "success"}),
        "the ticket's answer body"
    );
    assert_eq!(
        settings(&stored(&pool, alice.id).await),
        json!({"block_from_strangers": true, "hide_notification_contents": false}),
        "the named flag moved, the unnamed one stayed"
    );

    let second = put_json(&app, &token, json!({"hide_notification_contents": true})).await;
    common::assert_answer(&second, StatusCode::OK, "the hide update");
    assert_eq!(
        settings(&stored(&pool, alice.id).await),
        json!({"block_from_strangers": true, "hide_notification_contents": true}),
        "the other flag keeps the update it got"
    );

    let off = put_json(&app, &token, json!({"block_from_strangers": false})).await;
    common::assert_answer(&off, StatusCode::OK, "the reset");
    assert_eq!(
        settings(&stored(&pool, alice.id).await),
        json!({"block_from_strangers": false, "hide_notification_contents": true}),
        "false writes like true"
    );

    let empty = put_json(&app, &token, json!({})).await;
    common::assert_answer(&empty, StatusCode::OK, "the empty body");
    assert_eq!(
        empty.json_value(),
        json!({"status": "success"}),
        "naming nothing is still a success"
    );
    assert_eq!(
        settings(&stored(&pool, alice.id).await),
        json!({"block_from_strangers": false, "hide_notification_contents": true}),
        "and it changed nothing"
    );

    let form = common::TestRequest::put(ROUTE)
        .bearer(&token)
        .form(&[
            ("block_from_strangers", "on"),
            ("hide_notification_contents", "0"),
        ])
        .send(&app)
        .await;
    common::assert_answer(&form, StatusCode::OK, "the form update");
    assert_eq!(
        settings(&stored(&pool, alice.id).await),
        json!({"block_from_strangers": true, "hide_notification_contents": false}),
        "the form spellings coerce like the JSON ones"
    );
}

/// A flag that is neither a JSON boolean nor a `flex` spelling is a 422,
/// and the refusal leaves the stored settings untouched.
#[tokio::test]
async fn invalid_booleans_are_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let app = common::test_app(pool.clone());

    let before = put_json(&app, &token, json!({"block_from_strangers": true})).await;
    common::assert_answer(&before, StatusCode::OK, "the seeded update");

    for body in [
        json!({"block_from_strangers": "maybe"}),
        json!({"hide_notification_contents": 1}),
        json!({"block_from_strangers": ["true"]}),
        json!({"hide_notification_contents": {"nested": true}}),
    ] {
        let refused = put_json(&app, &token, body.clone()).await;
        let who = format!("`{body}`");
        common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, &who);
        let message = refused.json_value()["error"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            message.starts_with("Invalid parameters"),
            "the 422 names the bad parameter: {message}"
        );
    }
    assert_eq!(
        settings(&stored(&pool, alice.id).await),
        json!({"block_from_strangers": true, "hide_notification_contents": false}),
        "no refusal wrote anything"
    );
}

/// The own-view Account (`verify_credentials`) renders the stored flags as
/// the `pleroma.notification_settings` object; another viewer never sees
/// the key.
#[tokio::test]
async fn the_own_view_renders_the_stored_flags() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read write").await;
    let mallory = common::create_local_user(&pool, "mallory").await;
    let stranger_token = common::create_app_and_token(&pool, &mallory, "read:accounts").await;
    let app = common::test_app(pool);

    let updated = put_json(
        &app,
        &alice_token,
        json!({"block_from_strangers": true, "hide_notification_contents": true}),
    )
    .await;
    common::assert_answer(&updated, StatusCode::OK, "the update");

    let own = common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(&alice_token)
        .send(&app)
        .await;
    common::assert_answer(&own, StatusCode::OK, "verify_credentials");
    assert_eq!(
        own.json_value()["pleroma"]["notification_settings"],
        json!({"block_from_strangers": true, "hide_notification_contents": true}),
        "the owner sees exactly the two flags as stored"
    );

    let shown = common::TestRequest::get(&format!("/api/v1/accounts/{}", alice.id))
        .bearer(&stranger_token)
        .send(&app)
        .await;
    common::assert_answer(&shown, StatusCode::OK, "the stranger's show");
    assert!(
        shown.json_value()["pleroma"]
            .get("notification_settings")
            .is_none(),
        "the key stays owner-only"
    );
}

/// The route demands a user token with `write:accounts`: no credential is
/// a 401, an app-only token and a scope-less user token are a 403, and the
/// parent `write` grants it. Only `PUT` is routed; a `GET` is a 405.
#[tokio::test]
async fn auth_and_scope_gate_the_update() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read_only) = fixture(&pool, "alice", "read:accounts").await;
    let wrong_scope = common::create_app_and_token(&pool, &alice, "read:chats").await;
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let broad_write = common::create_app_and_token(&pool, &alice, "write").await;
    let app = common::test_app(pool.clone());

    let anonymous = common::TestRequest::put(ROUTE)
        .json(&json!({"block_from_strangers": true}))
        .send(&app)
        .await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
    common::assert_error(&anonymous, "Invalid credentials", "the anonymous PUT");

    let userless = common::TestRequest::put(ROUTE)
        .bearer(&app_only)
        .json(&json!({"block_from_strangers": true}))
        .send(&app)
        .await;
    common::assert_answer(&userless, StatusCode::FORBIDDEN, "an app token");
    common::assert_error(
        &userless,
        "This API requires an authenticated user",
        "the app-only PUT",
    );

    let read = put_json(&app, &read_only, json!({"block_from_strangers": true})).await;
    common::assert_answer(&read, StatusCode::FORBIDDEN, "read:accounts");
    common::assert_error(
        &read,
        "Insufficient permissions: write:accounts.",
        "the unscoped-enough user token",
    );

    let chats = put_json(&app, &wrong_scope, json!({"block_from_strangers": true})).await;
    common::assert_answer(&chats, StatusCode::FORBIDDEN, "read:chats");

    let granted = put_json(&app, &broad_write, json!({"block_from_strangers": true})).await;
    common::assert_answer(&granted, StatusCode::OK, "the parent write scope");

    assert_eq!(
        settings(&stored(&pool, alice.id).await),
        json!({"block_from_strangers": true, "hide_notification_contents": false}),
        "only the granted write landed"
    );

    let get = common::TestRequest::get(ROUTE)
        .bearer(&broad_write)
        .send(&app)
        .await;
    common::assert_answer(&get, StatusCode::METHOD_NOT_ALLOWED, "the unrouted GET");
}
