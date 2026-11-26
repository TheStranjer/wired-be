//! `GET /api/v1/notifications/unread_count`: the v2 count's v1 twin — the
//! unseen undismissed rows past the `notifications` marker, capped by
//! `limit` (default 100, at most 1000), narrowed by the type filters, and
//! gated on `read:notifications`.

use axum::{Router, http::StatusCode};
use wired_be::models::marker;

use super::support::{READ, V1, fixture};
use crate::common;

/// `GET /api/v1/notifications/unread_count{query}` as `token`, asserted a
/// 200, answering its `count`.
async fn unread(app: &Router, token: &str, query: &str) -> i64 {
    let response = common::TestRequest::get(&format!("{V1}/unread_count{query}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the v1 unread count");
    response.json_value()["count"]
        .as_i64()
        .expect("count is a number")
}

/// Without a marker every row counts; with one only newer rows do; the
/// limit caps and `types[]` / `exclude_types[]` narrow.
#[tokio::test]
async fn the_v1_count_follows_the_marker_the_limit_and_the_filters() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let first = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let _second = common::create_notification(&pool, &alice, &carol, "follow", None).await;
    let _third = common::create_notification(&pool, &alice, &bob, "mention", None).await;
    let app = common::test_app(pool.clone());

    assert_eq!(unread(&app, &token, "").await, 3, "no marker: every row");
    assert_eq!(unread(&app, &token, "?limit=2").await, 2, "limit caps");
    assert_eq!(
        unread(&app, &token, "?types[]=follow").await,
        2,
        "types[] narrows"
    );
    assert_eq!(
        unread(&app, &token, "?exclude_types[]=follow").await,
        1,
        "exclude_types[] narrows"
    );

    let marker_id = first.id.to_string();
    let _rows: Vec<marker::Marker> = marker::upsert_many(
        &pool,
        alice.id,
        vec![(marker::NOTIFICATIONS, marker_id.as_str())],
    )
    .await
    .expect("the marker fixture upserts");
    assert_eq!(
        unread(&app, &token, "").await,
        2,
        "with a marker only newer rows count"
    );
}

/// The v1 and v2 counts are the same handler: they always agree.
#[tokio::test]
async fn the_v1_count_matches_the_v2_count() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _row = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let app = common::test_app(pool);

    let v2 = common::TestRequest::get("/api/v2/notifications/unread_count")
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&v2, StatusCode::OK, "the v2 count");

    assert_eq!(
        v2.json_value()["count"].as_i64(),
        Some(unread(&app, &token, "").await)
    );
}

/// The count needs a user token with `read:notifications`.
#[tokio::test]
async fn the_v1_count_is_gated() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, wrong_scope) = fixture(&pool, "alice", "read:accounts").await;
    let app = common::test_app(pool);
    let path = format!("{V1}/unread_count");

    let anonymous = common::TestRequest::get(&path).send(&app).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");

    let denied = common::TestRequest::get(&path)
        .bearer(&wrong_scope)
        .send(&app)
        .await;
    common::assert_answer(&denied, StatusCode::FORBIDDEN, "the wrong scope");
}
