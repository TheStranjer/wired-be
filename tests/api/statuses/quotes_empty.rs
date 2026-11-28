//! An empty `quote_id` (or `quoted_status_id`) is no quote, as in Pleroma's
//! `ActivityDraft.quote_post/1`: only the exact empty string is ignored, so a
//! blank or unknown id is still the shared 404 and a real one still quotes.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::models::status::Visibility;

use super::support::{
    NOT_FOUND, STATUSES, UNKNOWN_ID, WRITE, created_row, posted, publish, stored_count,
    user_with_token,
};
use crate::common;

/// Asserts a created status's answer carries no quote anywhere.
fn assert_no_quote(value: &Value, what: &str) {
    assert!(
        value["quote"].is_null(),
        "{what}: the entity has `quote: null`"
    );
    assert!(
        value["pleroma"]["quote_id"].is_null(),
        "{what}: `pleroma.quote_id` is null"
    );
}

/// An empty `quote_id` or `quoted_status_id`, in JSON, posts a plain status.
#[tokio::test]
async fn an_empty_quote_id_posts_without_a_quote() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for key in ["quote_id", "quoted_status_id"] {
        let value = posted(&app, &token, json!({"status": "no quote", key: ""})).await;
        assert_no_quote(&value, key);
        assert_eq!(
            created_row(&pool, &value).await.quote_of_id,
            None,
            "{key}: the stored row quotes nothing"
        );
    }
    assert_eq!(stored_count(&pool).await, 2, "both posts were stored");
}

/// A form body's blank `quote_id=` field posts a plain status.
#[tokio::test]
async fn a_blank_form_quote_id_posts_without_a_quote() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[("status", "form no quote"), ("quote_id", "")])
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "a form body with `quote_id=`");
    let value = response.json_value();
    assert_no_quote(&value, "form quote_id");
    assert_eq!(created_row(&pool, &value).await.quote_of_id, None);
}

/// Whitespace is not empty (Pleroma does not trim), and an unknown id is
/// unchanged: both stay the shared 404 and post nothing.
#[tokio::test]
async fn a_blank_or_unknown_quote_id_is_still_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for id in [" ", UNKNOWN_ID] {
        let response = publish(
            &app,
            Some(&token),
            json!({"status": "still refused", "quote_id": id}),
        )
        .await;
        common::assert_answer(
            &response,
            StatusCode::NOT_FOUND,
            &format!("quote_id `{id}`"),
        );
        common::assert_error(&response, NOT_FOUND, &format!("quote_id `{id}`"));
    }
    assert_eq!(stored_count(&pool).await, 0, "no refusal wrote a row");
}

/// A valid `quote_id` still quotes alongside the empty-string rule.
#[tokio::test]
async fn a_valid_quote_id_still_quotes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let original = common::create_status(&pool, &bob, "quote me", Visibility::Public).await;
    let app = common::test_app(pool.clone());

    let value = posted(
        &app,
        &token,
        json!({"status": "quoting", "quote_id": original.id.to_string()}),
    )
    .await;
    assert_eq!(
        created_row(&pool, &value).await.quote_of_id,
        Some(original.id),
        "a non-empty id quotes the original"
    );
}
