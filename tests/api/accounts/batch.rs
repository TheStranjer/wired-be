//! `GET /api/v1/accounts?id[]=…`, Mastodon 4.3's multiple-account fetch:
//! the visible accounts in request order, unknown, malformed, repeated and
//! hidden ids skipped, at most 40 ids, and the `read:accounts` gate.

use axum::{Router, http::StatusCode};
use serde_json::Value;
use uuid::Uuid;
use wired_be::models::user::{Flag, User};

use crate::common;

/// `GET /api/v1/accounts` for `ids` as `token` (anonymous when `None`):
/// each id in its own percent-encoded `id[]` parameter.
async fn fetch(app: &Router, ids: &[String], token: Option<&str>) -> common::TestResponse {
    let query: Vec<String> = ids.iter().map(|id| format!("id%5B%5D={id}")).collect();
    let request = common::TestRequest::get(&format!("/api/v1/accounts?{}", query.join("&")));
    match token {
        Some(token) => request.bearer(token).send(app).await,
        None => request.send(app).await,
    }
}

/// The `id` text of every account in a response body, in order.
fn ids_of(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the answer is an array")
        .iter()
        .map(|account| account["id"].as_str().expect("ids are text").to_owned())
        .collect()
}

/// The id text of `user`.
fn id(user: &User) -> String {
    user.id.to_string()
}

/// Accounts come back in the order requested, not in id order; unknown and
/// malformed ids are skipped and a repeated id answers once.
#[tokio::test]
async fn accounts_come_back_in_request_order_and_misses_are_skipped() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = common::test_app(pool);
    let ids = [
        id(&carol),
        Uuid::now_v7().to_string(),
        "not-an-id".to_owned(),
        id(&alice),
        id(&carol),
        id(&bob),
    ];

    let response = fetch(&app, &ids, Some(&token)).await;

    common::assert_answer(&response, StatusCode::OK, "the batch");
    assert_eq!(
        ids_of(&response.json_value()),
        [id(&carol), id(&alice), id(&bob)],
        "request order, misses skipped, the repeat answered once"
    );
}

/// The anonymous view works too, and no usable id is the empty array.
#[tokio::test]
async fn an_anonymous_batch_works_and_no_ids_is_an_empty_array() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool);

    let response = fetch(&app, &[id(&bob)], None).await;
    common::assert_answer(&response, StatusCode::OK, "the anonymous batch");
    let body = response.json_value();
    assert_eq!(ids_of(&body), [id(&bob)], "bob is public");
    assert_eq!(body[0]["acct"], "bob", "a full account entity");

    let empty = fetch(&app, &[], None).await;
    common::assert_answer(&empty, StatusCode::OK, "no ids");
    assert_eq!(empty.text(), "[]", "an empty array, not an error");
}

/// Forty ids are fine; forty-one are a 422.
#[tokio::test]
async fn more_than_forty_ids_are_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let mut ids: Vec<String> = (1..=40).map(|n| Uuid::from_u128(n).to_string()).collect();

    let at_the_limit = fetch(&app, &ids, None).await;
    common::assert_answer(&at_the_limit, StatusCode::OK, "forty ids");

    ids.push(Uuid::from_u128(41).to_string());
    let over = fetch(&app, &ids, None).await;
    common::assert_answer(&over, StatusCode::UNPROCESSABLE_ENTITY, "forty-one ids");
}

/// A deactivated (suspended) account and one awaiting approval are skipped
/// for a stranger, as every other account read treats them.
#[tokio::test]
async fn hidden_accounts_are_left_out() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_local_user(&pool, "bob").await;
    let suspended = common::create_local_user_with(&pool, "suspended", |mut new| {
        new.is_active = Flag::new(false);
        new
    })
    .await;
    let pending = common::create_local_user_with(&pool, "pending", |mut new| {
        new.approval_pending = Flag::new(true);
        new
    })
    .await;
    let app = common::test_app(pool);
    let ids = [id(&suspended), id(&bob), id(&pending)];

    let response = fetch(&app, &ids, None).await;

    common::assert_answer(&response, StatusCode::OK, "the batch");
    assert_eq!(
        ids_of(&response.json_value()),
        [id(&bob)],
        "only the visible account answers"
    );
}

/// A user token without `read:accounts` is a 403; an invalid one a 401.
#[tokio::test]
async fn the_batch_demands_the_read_accounts_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:notifications").await;
    let app = common::test_app(pool);

    let denied = fetch(&app, &[id(&alice)], Some(&token)).await;
    common::assert_answer(&denied, StatusCode::FORBIDDEN, "a token without the scope");

    let invalid = fetch(&app, &[id(&alice)], Some("garbage")).await;
    common::assert_answer(&invalid, StatusCode::UNAUTHORIZED, "an invalid token");
}
