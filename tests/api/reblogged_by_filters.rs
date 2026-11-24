//! `GET /api/v1/statuses/{id}/reblogged_by` and the accounts it drops
//! (WBE-717): a deactivated booster is never listed, and a viewer who blocked
//! a booster no longer sees it, while an anonymous request does. The `Link`
//! cursors come from the raw page, so they still name its edges when the
//! filter takes the edge row out of the rendered list.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{block, status::Visibility, user::User},
};

use crate::common;

/// The `id` of every account in an `Account` array body.
fn account_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the answer is an array")
        .iter()
        .map(|entry| entry["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// A local user named `nickname` who may boost and read.
async fn booster(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, "write:statuses read").await;
    (created, token)
}

/// `token`'s boost of status `id`, asserted stored.
async fn boost(app: &Router, token: &str, id: &str) {
    let response = common::TestRequest::post(&format!("/api/v1/statuses/{id}/reblog"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the boost stores");
}

/// `GET …/reblogged_by` for `id`, anonymously when `token` is `None`.
async fn reblogged_by(app: &Router, token: Option<&str>, id: &str) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("/api/v1/statuses/{id}/reblogged_by"));
    match token {
        Some(bearer) => request.bearer(bearer).send(app).await,
        None => request.send(app).await,
    }
}

/// The live boost row ids of `original`, newest first.
async fn boost_ids(pool: &PgPool, original: Uuid) -> Vec<Uuid> {
    sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM statuses
          WHERE reblog_of_id = $1 AND deleted_at IS NULL
          ORDER BY id DESC",
    )
    .bind(original)
    .fetch_all(pool)
    .await
    .expect("the boost ids read")
    .into_iter()
    .map(|(id,)| id)
    .collect()
}

#[tokio::test]
async fn reblogged_by_omits_deactivated_accounts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user(&pool, "author").await;
    let original = common::create_status(&pool, &author, "a post", Visibility::Public).await;
    let id = original.id.to_string();
    let (dave, dave_token) = booster(&pool, "dave").await;
    let (bob, bob_token) = booster(&pool, "bob").await;
    boost(&app, &dave_token, &id).await;
    boost(&app, &bob_token, &id).await;
    let _done: sqlx::postgres::PgQueryResult =
        sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(dave.id)
            .execute(&pool)
            .await
            .expect("the fixture deactivates dave");

    let page = reblogged_by(&app, None, &id).await;
    common::assert_answer(&page, StatusCode::OK, "the list answers");
    assert_eq!(
        account_ids(&page.json_value()),
        vec![bob.id.to_string()],
        "the deactivated booster is absent"
    );
}

#[tokio::test]
async fn reblogged_by_omits_the_viewers_blocked_accounts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user(&pool, "author").await;
    let original = common::create_status(&pool, &author, "a post", Visibility::Public).await;
    let id = original.id.to_string();
    let (alice, alice_token) = booster(&pool, "alice").await;
    let (bob, bob_token) = booster(&pool, "bob").await;
    let (carol, carol_token) = booster(&pool, "carol").await;
    boost(&app, &bob_token, &id).await;
    boost(&app, &carol_token, &id).await;
    let _blocked: block::Block = block::block(&pool, TEST_BASE_URL, &alice, &carol)
        .await
        .expect("alice blocks carol");

    let as_alice = reblogged_by(&app, Some(&alice_token), &id).await;
    common::assert_answer(&as_alice, StatusCode::OK, "the list answers the blocker");
    assert_eq!(
        account_ids(&as_alice.json_value()),
        vec![bob.id.to_string()],
        "the blocked account is absent for the blocker"
    );

    let anonymous = reblogged_by(&app, None, &id).await;
    assert_eq!(
        account_ids(&anonymous.json_value()),
        vec![carol.id.to_string(), bob.id.to_string()],
        "an anonymous request still sees both"
    );
}

#[tokio::test]
async fn reblogged_by_link_names_the_raw_page_edges() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user(&pool, "author").await;
    let original = common::create_status(&pool, &author, "a post", Visibility::Public).await;
    let id = original.id.to_string();
    let path = format!("/api/v1/statuses/{id}/reblogged_by");
    let (alice, alice_token) = booster(&pool, "alice").await;
    let (_bob, bob_token) = booster(&pool, "bob").await;
    let (carol, carol_token) = booster(&pool, "carol").await;
    boost(&app, &carol_token, &id).await;
    boost(&app, &bob_token, &id).await;
    let _blocked: block::Block = block::block(&pool, TEST_BASE_URL, &alice, &carol)
        .await
        .expect("alice blocks carol");
    let boosts = boost_ids(&pool, original.id).await;
    assert_eq!(boosts.len(), 2, "two boosts are stored");

    let page = reblogged_by(&app, Some(&alice_token), &id).await;
    assert_eq!(account_ids(&page.json_value()).len(), 1, "only bob renders");
    let link = page.header("link").expect("the page carries a Link header");
    assert!(
        link.contains(&format!("{path}?max_id={}>; rel=\"next\"", boosts[1])),
        "next keys on the oldest raw boost, carol's: {link}"
    );
    assert!(
        link.contains(&format!("{path}?min_id={}>; rel=\"prev\"", boosts[0])),
        "prev keys on the newest raw boost: {link}"
    );
}
