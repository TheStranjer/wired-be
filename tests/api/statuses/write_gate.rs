//! The write pipeline itself: `POST /api/v1/statuses` demands a user token
//! carrying `write:statuses` — the 401s and 403s for everything else, and
//! none of them posts — spends the `statuses_actions` bucket like the other
//! status writes, and keeps its gate clear of the batch read on the same
//! path.

use axum::http::StatusCode;
use serde_json::json;

use super::support::{
    INVALID_TOKEN, NO_TOKEN, NO_WRITE, USER_REQUIRED, WRITE, author_now, batch, posted, publish,
    stored_count, user_with_token,
};
use crate::common;

/// The write route runs the whole `AuthUser` + `write:statuses` pipeline: no
/// credential and an unresolvable one are the 401s, a token that names no
/// user and one without the scope are the 403s, and none of them posts.
#[tokio::test]
async fn the_route_needs_a_user_token_that_writes_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (reader, reader_token) = user_with_token(&pool, "reader", "read:statuses").await;
    let (_writer, writer_token) = user_with_token(&pool, "writer", WRITE).await;
    let ownerless = common::create_app_only_token(&pool, WRITE).await;
    let app = common::test_app(pool.clone());
    let body = json!({"status": "who is speaking"});

    let anonymous = publish(&app, None, body.clone()).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no credential");
    common::assert_error(&anonymous, NO_TOKEN, "no credential");

    let unknown = publish(&app, Some("token-that-was-never-minted"), body.clone()).await;
    common::assert_answer(&unknown, StatusCode::UNAUTHORIZED, "an unknown token");
    common::assert_error(&unknown, INVALID_TOKEN, "an unknown token");

    let narrow = publish(&app, Some(reader_token.as_str()), body.clone()).await;
    common::assert_answer(&narrow, StatusCode::FORBIDDEN, "a token without the scope");
    common::assert_error(&narrow, NO_WRITE, "a token without the scope");

    let app_only = publish(&app, Some(ownerless.as_str()), body.clone()).await;
    common::assert_answer(&app_only, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(&app_only, USER_REQUIRED, "an app-only token");

    assert_eq!(stored_count(&pool).await, 0, "not one refusal wrote a row");
    assert_eq!(
        author_now(&pool, reader.id).await.statuses_count,
        0,
        "and the near-enough token gained nothing"
    );

    let granted = posted(&app, &writer_token, body).await;
    assert_eq!(
        granted["account"]["acct"], "writer",
        "the scope does post, as its own author"
    );
}

/// The route is in the `statuses_actions` bucket Pleroma throttles status
/// writes through: the request past the budget is refused before the
/// service runs, so nothing is posted and nothing is counted.
#[tokio::test]
async fn the_route_spends_the_statuses_actions_bucket() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("statuses_actions=2:600"),
    );

    for spent in 0..2 {
        let response = publish(&app, Some(token.as_str()), json!({"status": "counted"})).await;
        common::assert_answer(&response, StatusCode::OK, "a request inside the budget");
        assert_eq!(
            response.header("x-ratelimit-limit"),
            Some("2"),
            "the bucket's budget"
        );
        assert_eq!(
            response.header("x-ratelimit-remaining"),
            Some(if spent == 0 { "1" } else { "0" }),
            "the window counts down"
        );
    }

    let throttled = publish(&app, Some(token.as_str()), json!({"status": "throttled"})).await;
    common::assert_answer(&throttled, StatusCode::TOO_MANY_REQUESTS, "over the budget");
    common::assert_error(&throttled, "Throttled", "over the budget");
    assert_eq!(
        stored_count(&pool).await,
        2,
        "the third request never reached the service"
    );
    assert_eq!(
        author_now(&pool, alice.id).await.statuses_count,
        2,
        "and so the author counts two posts, not three"
    );
}

/// The batch read and the write share the `/statuses` path without either
/// losing its gate: the throttled `POST` is a 401 with no credential while
/// the unthrottled `GET` still answers the anonymous `[]` it always did.
#[tokio::test]
async fn the_write_route_does_not_leak_onto_the_read_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("statuses_actions=1:600"),
    );
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;

    let read = batch(&app, None, "").await;
    common::assert_answer(&read, StatusCode::OK, "the anonymous batch");
    assert_eq!(
        read.header("x-ratelimit-limit"),
        None,
        "and it carries no throttle headers"
    );

    let first = publish(&app, Some(token.as_str()), json!({"status": "one"})).await;
    common::assert_answer(&first, StatusCode::OK, "the first post of the window");
    let second = publish(&app, Some(token.as_str()), json!({"status": "two"})).await;
    common::assert_answer(&second, StatusCode::TOO_MANY_REQUESTS, "the second");
    let read_again = batch(&app, None, "").await;
    common::assert_answer(
        &read_again,
        StatusCode::OK,
        "the batch is still unthrottled",
    );
    assert_eq!(
        read_again.json_value(),
        json!([]),
        "and the visible ids it answers are unchanged by the throttle"
    );
}
