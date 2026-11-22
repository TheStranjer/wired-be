//! The replies collection and its visibility rule.

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;
use wired_be::models::status::Visibility;

use super::support::{BASE, fetch_ap, key_set, reply_to, unpost};
use crate::common;

/// The replies collection: the root counts the publicly visible answers,
/// and the page lists their object ids oldest first; private, direct and
/// deleted answers and the replies of other threads stay out.
#[tokio::test]
async fn the_replies_collection_lists_public_answers_oldest_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let root = common::create_status(&pool, &alice, "ask", Visibility::Public).await;
    let other = common::create_status(&pool, &alice, "elsewhere", Visibility::Public).await;
    let one = reply_to(&pool, &bob, &root, "public answer", Visibility::Public).await;
    let two = reply_to(&pool, &alice, &root, "quiet answer", Visibility::Unlisted).await;
    let _fm = reply_to(&pool, &bob, &root, "followers only", Visibility::Private).await;
    let _dm = reply_to(&pool, &bob, &root, "dm", Visibility::Direct).await;
    let _elsewhere = reply_to(&pool, &bob, &other, "another thread", Visibility::Public).await;
    let gone = reply_to(&pool, &bob, &root, "deleted", Visibility::Public).await;
    unpost(&pool, &gone).await;
    let app = common::test_app(pool);

    let collection = fetch_ap(&app, &format!("/objects/{}/replies", root.id)).await;
    common::assert_answer(&collection, StatusCode::OK, "the replies root");
    let document = collection.json_value();
    assert_eq!(
        key_set(&document),
        ["@context", "first", "id", "totalItems", "type"],
        "the summary: two public answers"
    );
    assert_eq!(document["type"], "Collection", "a plain (paged) collection");
    assert_eq!(
        document["id"],
        format!("{BASE}/objects/{}/replies", root.id)
    );
    assert_eq!(document["totalItems"], 2, "the publicly visible count");
    assert_eq!(
        document["first"],
        format!("{BASE}/objects/{}/replies?page=true", root.id)
    );

    let page = fetch_ap(&app, &format!("/objects/{}/replies?page=true", root.id)).await;
    common::assert_answer(&page, StatusCode::OK, "the replies page");
    let page_document = page.json_value();
    assert_eq!(page_document["type"], "CollectionPage", "the page type");
    assert_eq!(
        page_document["id"],
        format!("{BASE}/objects/{}/replies?page=true", root.id),
        "the id echoes the request"
    );
    assert_eq!(
        page_document["partOf"],
        format!("{BASE}/objects/{}/replies", root.id)
    );
    assert_eq!(
        page_document["items"],
        json!([one.uri, two.uri]),
        "the note ids, oldest first"
    );
    assert_eq!(page_document["next"], Value::Null, "two is well under 20");
}

/// Twenty-one public answers: the first page is the 20 oldest with a
/// `next` keyed forward on `min_id`, and following it answers the last.
#[tokio::test]
async fn replies_pages_walk_forward_with_min_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let root = common::create_status(&pool, &alice, "ask", Visibility::Public).await;
    let mut answers = Vec::new();
    for n in 1..=21 {
        let text = format!("answer {n}");
        answers.push(reply_to(&pool, &alice, &root, &text, Visibility::Public).await);
    }
    let app = common::test_app(pool);
    let path = format!("/objects/{}/replies", root.id);

    let first = fetch_ap(&app, &format!("{path}?page=true")).await;
    let document = first.json_value();
    let items = document["items"].as_array().expect("items");
    assert_eq!(items.len(), 20, "the page holds the fixed 20");
    assert_eq!(items[0], answers[0].uri, "oldest first");
    assert_eq!(
        document["next"],
        json!(format!("{BASE}{path}?page=true&min_id={}", answers[19].id)),
        "the walk moves forward from the newest id shown"
    );

    let second = fetch_ap(&app, &format!("{path}?page=true&min_id={}", answers[19].id)).await;
    let second_document = second.json_value();
    assert_eq!(
        second_document["items"],
        json!([answers[20].uri]),
        "the last answer closes the walk"
    );
    assert_eq!(second_document["next"], Value::Null, "and no further page");
    assert_eq!(
        second_document["prev"],
        json!(format!("{BASE}{path}?page=true&max_id={}", answers[20].id)),
        "the prev link keys back on the oldest id shown"
    );
}

/// The replies route answers the 404 for every object it does not serve:
/// an unknown id, a tombstone, a post that is not publicly visible, and
/// an id that is no id at all.
#[tokio::test]
async fn the_replies_route_only_serves_publicly_visible_objects() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let public = common::create_status(&pool, &alice, "public", Visibility::Public).await;
    let private = common::create_status(&pool, &alice, "fm", Visibility::Private).await;
    let direct = common::create_status(&pool, &alice, "dm", Visibility::Direct).await;
    let gone = common::create_status(&pool, &alice, "gone", Visibility::Public).await;
    unpost(&pool, &gone).await;
    let app = common::test_app(pool);

    let ok = fetch_ap(&app, &format!("/objects/{}/replies", public.id)).await;
    common::assert_answer(&ok, StatusCode::OK, "a public post has replies");

    let unknown = Uuid::now_v7();
    let misses = [
        format!("/objects/{unknown}/replies"),
        format!("/objects/{}/replies", private.id),
        format!("/objects/{}/replies", direct.id),
        format!("/objects/{}/replies", gone.id),
        "/objects/not-a-uuid/replies".to_owned(),
    ];
    for path in misses {
        let response = fetch_ap(&app, &path).await;
        common::assert_answer(&response, StatusCode::NOT_FOUND, &path);
        assert_eq!(
            response.json_value(),
            json!({"error": "Not found"}),
            "{path} keeps the fallback body"
        );
    }
}
