//! `GET /api/v1/statuses/{id}/reblogged_by` and reblog visibility (WBE-488):
//! the listing names only distributable boosts (`public` / `unlisted`), as
//! Mastodon's `distributable_visibility` does, so a followers-only boost is
//! never leaked to an anonymous caller, and the `Link` cursors still page over
//! the boosts that are listed.

#![cfg(test)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;
use wired_be::models::status::Visibility;

use crate::common;

/// The `id` of every account in an `Account` array body.
fn account_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the answer is an array")
        .iter()
        .map(|entry| entry["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn a_private_boost_is_not_listed_and_the_cursors_still_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let bob = common::create_local_user(&pool, "bob").await;
    let original = common::create_status(&pool, &bob, "a post", Visibility::Public).await;
    let id = original.id.to_string();
    let path = format!("/api/v1/statuses/{id}/reblogged_by");

    let carol = common::create_local_user(&pool, "carol").await;
    let carol_token = common::create_app_and_token(&pool, &carol, "write:statuses read").await;
    let private = common::TestRequest::post(&format!("/api/v1/statuses/{id}/reblog"))
        .bearer(&carol_token)
        .json(&json!({"visibility": "private"}))
        .send(&app)
        .await;
    common::assert_answer(&private, StatusCode::OK, "carol boosts to followers only");

    let mut public = Vec::new();
    for name in ["dave", "erin"] {
        let account = common::create_local_user(&pool, name).await;
        let token = common::create_app_and_token(&pool, &account, "write:statuses read").await;
        let boost = common::TestRequest::post(&format!("/api/v1/statuses/{id}/reblog"))
            .bearer(&token)
            .send(&app)
            .await;
        common::assert_answer(&boost, StatusCode::OK, "a public boost stores");
        public.push(account);
    }
    let boost_ids: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM statuses
          WHERE reblog_of_id = $1 AND deleted_at IS NULL AND visibility = 'public'
          ORDER BY id DESC",
    )
    .bind(original.id)
    .fetch_all(&pool)
    .await
    .expect("the public boosts read")
    .into_iter()
    .map(|(boost_id,)| boost_id)
    .collect();
    assert_eq!(boost_ids.len(), 2, "two public boosts are stored");

    let listed = common::TestRequest::get(&path).send(&app).await;
    common::assert_answer(&listed, StatusCode::OK, "an anonymous caller lists");
    let body = listed.json_value();
    assert_eq!(
        account_ids(&body),
        vec![public[1].id.to_string(), public[0].id.to_string()],
        "only the public boosters, newest first; carol's private boost is hidden"
    );
    assert!(
        !account_ids(&body).contains(&carol.id.to_string()),
        "carol is not leaked"
    );
    let link = listed.header("link").expect("a non-empty page has a Link");
    assert!(
        link.contains(&format!("{path}?max_id={}>; rel=\"next\"", boost_ids[1])),
        "next keys on the oldest listed boost: {link}"
    );
    assert!(
        link.contains(&format!("{path}?min_id={}>; rel=\"prev\"", boost_ids[0])),
        "prev keys on the newest listed boost: {link}"
    );

    let first = common::TestRequest::get(&format!("{path}?limit=1"))
        .send(&app)
        .await;
    assert_eq!(
        account_ids(&first.json_value()),
        vec![public[1].id.to_string()],
        "limit cuts at the newest listed boost"
    );
    let rest = common::TestRequest::get(&format!("{path}?max_id={}", boost_ids[0]))
        .send(&app)
        .await;
    assert_eq!(
        account_ids(&rest.json_value()),
        vec![public[0].id.to_string()],
        "max_id continues past the page's end without surfacing the private boost"
    );
}
