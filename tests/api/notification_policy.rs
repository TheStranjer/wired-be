//! The notification policy (`/api/v1/notifications/policy`,
//! `/api/v2/notifications/policy`) and the filtered-notification request
//! list (`/api/v1/notifications/requests`): the accept-everything policy
//! shape, writes that ask for no filtering restating it, writes that ask
//! for any filtering refused with a 422, the always-empty request list, and
//! the scope gates (`src/api/notification_policy.rs`).

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use wired_be::test_db::TestDb;

use crate::common;

/// The v1 policy path.
const V1: &str = "/api/v1/notifications/policy";
/// The v2 policy path.
const V2: &str = "/api/v2/notifications/policy";
/// The scope the reads demand.
const READ: &str = "read:notifications";
/// The scope the writes demand.
const WRITE: &str = "write:notifications";

/// An app, its user `alice` and her `scopes` token, on a fresh router.
async fn setup(scopes: &str) -> (Router, String, TestDb) {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, scopes).await;
    (common::test_app(pool), token, db)
}

/// `PATCH path` with a JSON `body` as `token`.
async fn patch(app: &Router, path: &str, token: &str, body: &Value) -> common::TestResponse {
    common::TestRequest::patch(path)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// `GET path` as `token`.
async fn get(app: &Router, path: &str, token: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// The v1 policy this server answers: nothing filtered, nothing pending.
fn v1_policy() -> Value {
    json!({
        "filter_not_following": false,
        "filter_not_followers": false,
        "filter_new_accounts": false,
        "filter_private_mentions": false,
        "summary": {"pending_requests_count": 0, "pending_notifications_count": 0},
    })
}

/// The v2 policy this server answers: everything accepted.
fn v2_policy() -> Value {
    json!({
        "for_not_following": "accept",
        "for_not_followers": "accept",
        "for_new_accounts": "accept",
        "for_private_mentions": "accept",
        "for_limited_accounts": "accept",
        "summary": {"pending_requests_count": 0, "pending_notifications_count": 0},
    })
}

/// Both policy reads answer the accept-everything shape.
#[tokio::test]
async fn the_policy_reads_answer_the_default_shape() {
    let (app, token, _db) = setup(READ).await;

    let v1 = get(&app, V1, &token).await;
    common::assert_answer(&v1, StatusCode::OK, "the v1 policy");
    assert_eq!(v1.json_value(), v1_policy(), "the v1 shape");

    let v2 = get(&app, V2, &token).await;
    common::assert_answer(&v2, StatusCode::OK, "the v2 policy");
    assert_eq!(v2.json_value(), v2_policy(), "the v2 shape");
}

/// A write that asks only for what is already true restates the policy.
#[tokio::test]
async fn a_write_asking_for_no_filtering_restates_the_policy() {
    let (app, token, _db) = setup(WRITE).await;

    let v1 = patch(&app, V1, &token, &json!({"filter_new_accounts": false})).await;
    common::assert_answer(&v1, StatusCode::OK, "a v1 no-op write");
    assert_eq!(v1.json_value(), v1_policy(), "the v1 shape");

    let v2 = patch(&app, V2, &token, &json!({"for_not_following": "accept"})).await;
    common::assert_answer(&v2, StatusCode::OK, "a v2 no-op write");
    assert_eq!(v2.json_value(), v2_policy(), "the v2 shape");

    let empty = patch(&app, V2, &token, &json!({})).await;
    common::assert_answer(&empty, StatusCode::OK, "an empty write");
}

/// A write that asks for any filtering is refused, and nothing changes.
#[tokio::test]
async fn a_write_asking_for_filtering_is_refused() {
    let (app, token, _db) = setup("read:notifications write:notifications").await;

    for (path, body) in [
        (V1, json!({"filter_not_following": true})),
        (V1, json!({"filter_private_mentions": "true"})),
        (V2, json!({"for_not_followers": "filter"})),
        (V2, json!({"for_limited_accounts": "drop"})),
        (V2, json!({"for_new_accounts": "bogus"})),
    ] {
        let response = patch(&app, path, &token, &body).await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("PATCH {path} {body}"),
        );
    }

    let after = get(&app, V2, &token).await;
    assert_eq!(after.json_value(), v2_policy(), "still accept everything");
}

/// The request list is empty and the merge is done.
#[tokio::test]
async fn the_request_list_is_empty() {
    let (app, token, _db) = setup(READ).await;

    let list = get(&app, "/api/v1/notifications/requests", &token).await;
    common::assert_answer(&list, StatusCode::OK, "the request list");
    assert_eq!(list.json_value(), json!([]), "nothing is held back");

    let merged = get(&app, "/api/v1/notifications/requests/merged", &token).await;
    common::assert_answer(&merged, StatusCode::OK, "the merged flag");
    assert_eq!(merged.json_value(), json!({"merged": true}));
}

/// Reads need `read:notifications`, writes `write:notifications`, and every
/// route needs a token.
#[tokio::test]
async fn the_routes_are_gated() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let read_only = common::create_app_and_token(&pool, &alice, READ).await;
    let write_only = common::create_app_and_token(&pool, &alice, WRITE).await;
    let app = common::test_app(pool);

    for path in [
        V1,
        V2,
        "/api/v1/notifications/requests",
        "/api/v1/notifications/requests/merged",
    ] {
        let anonymous = common::TestRequest::get(path).send(&app).await;
        common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, path);
    }
    for path in [V1, V2] {
        let write_denied = patch(&app, path, &read_only, &json!({})).await;
        common::assert_answer(&write_denied, StatusCode::FORBIDDEN, path);
        let read_denied = get(&app, path, &write_only).await;
        common::assert_answer(&read_denied, StatusCode::FORBIDDEN, path);
    }
}
