//! The cap on `DELETE /api/v1/notifications/destroy_multiple`: more than 200
//! distinct ids is the 422 `Too many ids` and dismisses nothing; exactly 200
//! is served.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use super::support::{READ_WRITE, V1, assert_empty_object, fixture, stored_row};
use crate::common;

/// The 422 body of an id array past the cap.
const TOO_MANY_IDS: &str = "Too many ids";
/// The most distinct ids one request may name.
const MAX_BATCH_IDS: usize = 200;

/// `count` distinct ids that name no notification.
fn unknown_ids(count: usize) -> Vec<String> {
    (1..=count)
        .map(|n| Uuid::from_u128(u128::try_from(n).expect("a small count") << 64).to_string())
        .collect()
}

/// 201 distinct ids is refused — in the body and in the query — and the
/// real rows among them are not dismissed.
#[tokio::test]
async fn destroy_multiple_past_the_cap_is_refused_and_dismisses_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ_WRITE).await;
    let carol = common::create_local_user(&pool, "carol").await;
    let first = common::create_notification(&pool, &alice, &carol, "follow", None).await;
    let second = common::create_notification(&pool, &alice, &carol, "mention", None).await;
    let app = common::test_app(pool.clone());

    let mut ids = vec![first.id.to_string(), second.id.to_string()];
    ids.extend(unknown_ids(MAX_BATCH_IDS - 1));
    assert_eq!(ids.len(), MAX_BATCH_IDS + 1, "one past the cap");

    let by_body = common::TestRequest::delete(&format!("{V1}/destroy_multiple"))
        .bearer(&token)
        .json(&json!({ "ids": ids }))
        .send(&app)
        .await;
    assert_eq!(
        by_body.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the body spelling is a 422"
    );
    assert_eq!(
        by_body.json_value(),
        json!({"error": TOO_MANY_IDS}),
        "with the cap's body"
    );

    let query: Vec<String> = ids.iter().map(|id| format!("ids[]={id}")).collect();
    let by_query =
        common::TestRequest::delete(&format!("{V1}/destroy_multiple?{}", query.join("&")))
            .bearer(&token)
            .send(&app)
            .await;
    assert_eq!(
        by_query.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "so is the query spelling"
    );

    for row in [&first, &second] {
        let stored = stored_row(&pool, row.id).await.expect("the row stays");
        assert!(stored.dismissed_at.is_none(), "and nothing was dismissed");
    }
}

/// Exactly 200 distinct ids is served, and the count is of distinct ids:
/// naming each twice is still 200.
#[tokio::test]
async fn destroy_multiple_of_exactly_the_cap_succeeds() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ_WRITE).await;
    let carol = common::create_local_user(&pool, "carol").await;
    let first = common::create_notification(&pool, &alice, &carol, "follow", None).await;
    let second = common::create_notification(&pool, &alice, &carol, "mention", None).await;
    let app = common::test_app(pool.clone());

    let mut ids = vec![first.id.to_string(), second.id.to_string()];
    ids.extend(unknown_ids(MAX_BATCH_IDS - 2));
    ids.extend(ids.clone());
    let response = common::TestRequest::delete(&format!("{V1}/destroy_multiple"))
        .bearer(&token)
        .json(&json!({ "ids": ids }))
        .send(&app)
        .await;
    assert_empty_object(&response, "200 distinct ids");

    for row in [&first, &second] {
        let stored = stored_row(&pool, row.id).await.expect("the row stays");
        assert!(
            stored.dismissed_at.is_some(),
            "the listed rows are dismissed"
        );
    }
}
