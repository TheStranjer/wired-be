//! The actor outbox: root summary, pages, items and the empty case.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::models::status::Visibility;

use super::support::{
    ACTIVITY_JSON, AS_PUBLIC, BASE, fetch_ap, key_set, outbox_fixture, stored_user,
};
use crate::common;

/// The outbox root: the collection's address, the page link, and the
/// account's post count as the ticket's `totalItems` estimate — no items.
#[tokio::test]
async fn the_outbox_root_is_a_paged_collection_summary() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let _public = common::create_status(&pool, &alice, "hi", Visibility::Public).await;
    let _quiet = common::create_status(&pool, &alice, "quiet", Visibility::Unlisted).await;
    let _private = common::create_status(&pool, &alice, "fm", Visibility::Private).await;
    let counted = stored_user(&pool, "alice").await;
    let app = common::test_app(pool);

    let response = fetch_ap(&app, "/users/alice/outbox").await;
    common::assert_answer(&response, StatusCode::OK, "the outbox root");
    assert_eq!(
        response.header("content-type"),
        Some(ACTIVITY_JSON),
        "the document is ActivityPub JSON"
    );
    assert_eq!(
        response.header("vary"),
        Some("Accept"),
        "the route negotiated"
    );
    let document = response.json_value();
    assert_eq!(
        key_set(&document),
        ["@context", "first", "id", "totalItems", "type"],
        "the root's documented keys — no items at this level"
    );
    assert_eq!(document["id"], format!("{BASE}/users/alice/outbox"));
    assert_eq!(document["type"], "OrderedCollection");
    assert_eq!(
        document["first"],
        format!("{BASE}/users/alice/outbox?page=true"),
        "the first page is the collection with the page query"
    );
    assert_eq!(
        document["totalItems"],
        json!(counted.statuses_count),
        "the estimate is the account's stored post count, and it counts \
         the private row with the rest"
    );
    assert_eq!(counted.statuses_count, 3, "all three posts moved the count");
}

/// The outbox page lists the account's live `public` and `unlisted`
/// statuses as their `Create` activities and its reblogs as `Announce`s,
/// newest first; `private`, `direct` and deleted rows never appear.
#[tokio::test]
async fn the_outbox_page_lists_public_activities_newest_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fixture = outbox_fixture(&pool).await;
    let app = common::test_app(pool);

    let response = fetch_ap(&app, "/users/alice/outbox?page=true").await;
    common::assert_answer(&response, StatusCode::OK, "the outbox page");
    assert_eq!(
        response.header("content-type"),
        Some(ACTIVITY_JSON),
        "the page is ActivityPub JSON too"
    );
    let document = response.json_value();
    assert_eq!(document["type"], "OrderedCollectionPage", "the page type");
    assert_eq!(
        document["id"],
        format!("{BASE}/users/alice/outbox?page=true"),
        "the page's id echoes the request"
    );
    assert_eq!(
        document["partOf"],
        format!("{BASE}/users/alice/outbox"),
        "the page names its collection"
    );
    assert_eq!(
        document["next"],
        Value::Null,
        "a page of four is not full, so there is no next"
    );
    assert_eq!(
        document["prev"],
        json!(format!(
            "{BASE}/users/alice/outbox?page=true&min_id={}",
            fixture.reblog.id
        )),
        "the prev link keys on the page's newest id"
    );

    let items = document["orderedItems"].as_array().expect("ordered items");
    let listed: Vec<&str> = items
        .iter()
        .map(|item| {
            item.get("object")
                .and_then(|object| {
                    object
                        .as_str()
                        .or_else(|| object.get("id").and_then(Value::as_str))
                })
                .expect("every activity carries its object")
        })
        .collect();
    assert_eq!(
        listed,
        vec![
            fixture.boosted.uri.as_str(),
            fixture.quiet.uri.as_str(),
            fixture.answer.uri.as_str(),
            fixture.first.uri.as_str(),
        ],
        "newest first, and only the public and unlisted rows — the \
         private, direct and deleted ones are out"
    );
}

/// The page's items are the full activity documents: the boost as
/// Pleroma's addressing `Announce`, the posts as `Create`s around whole
/// `Note` objects with their visibility's addressing.
#[tokio::test]
async fn the_outbox_items_are_the_federated_activities() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fixture = outbox_fixture(&pool).await;
    let app = common::test_app(pool);

    let response = fetch_ap(&app, "/users/alice/outbox?page=true").await;
    let document = response.json_value();
    let items = document["orderedItems"].as_array().expect("ordered items");

    let announce = &items[0];
    assert_eq!(
        announce["type"], "Announce",
        "the reblog federates as its Announce"
    );
    assert_eq!(
        announce["id"], fixture.reblog.uri,
        "the activity id is the stored uri"
    );
    assert_eq!(announce["actor"], fixture.alice.ap_id, "the booster speaks");
    assert_eq!(
        announce["to"],
        json!([format!("{BASE}/users/alice/followers"), fixture.bob.ap_id]),
        "Pleroma's announce addressing: the booster's followers and the boosted author"
    );
    assert_eq!(
        announce["cc"],
        json!([AS_PUBLIC]),
        "and the public collection on cc"
    );

    let create = &items[1];
    assert_eq!(
        create["type"], "Create",
        "a plain status federates as its Create"
    );
    assert_eq!(
        create["id"],
        json!(format!("{BASE}/activities/{}", fixture.quiet.id)),
        "the Create's id is this instance's activity address"
    );
    assert_eq!(create["actor"], fixture.alice.ap_id, "the author speaks");
    assert_eq!(
        create["object"]["id"], fixture.quiet.uri,
        "the note is embedded whole"
    );
    assert_eq!(create["object"]["type"], "Note", "a plain status is a Note");
    assert_eq!(create["object"]["content"], "<p>quiet</p>", "rendered text");
    assert_eq!(
        create["to"],
        json!([format!("{BASE}/users/alice/followers")]),
        "an unlisted post addresses its author's followers"
    );
    assert_eq!(
        create["cc"],
        json!([AS_PUBLIC]),
        "with the public collection on cc"
    );
}

/// Twenty-one public posts: the first page is the newest 20 with a `next`
/// keyed on its oldest id; following it answers the last post with a
/// `prev` and no further `next`, and the pages share no row.
#[tokio::test]
async fn outbox_pages_walk_back_with_max_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mut posts = Vec::new();
    for n in 1..=21 {
        let text = format!("post {n}");
        posts.push(common::create_status(&pool, &alice, &text, Visibility::Public).await);
    }
    let app = common::test_app(pool);

    let first = fetch_ap(&app, "/users/alice/outbox?page=true").await;
    let first_document = first.json_value();
    let items = first_document["orderedItems"]
        .as_array()
        .expect("ordered items");
    assert_eq!(items.len(), 20, "the page holds the fixed 20");
    let oldest_of_first = &posts[posts.len() - 20];
    assert_eq!(
        first_document["next"],
        json!(format!(
            "{BASE}/users/alice/outbox?page=true&max_id={}",
            oldest_of_first.id
        )),
        "the next page keys on the oldest id shown"
    );

    let second = fetch_ap(
        &app,
        &format!(
            "/users/alice/outbox?page=true&max_id={}",
            oldest_of_first.id
        ),
    )
    .await;
    let second_document = second.json_value();
    assert_eq!(
        second_document["id"],
        json!(format!(
            "{BASE}/users/alice/outbox?page=true&max_id={}",
            oldest_of_first.id
        )),
        "the second page's id echoes its own cursor"
    );
    let second_items = second_document["orderedItems"]
        .as_array()
        .expect("ordered items");
    assert_eq!(second_items.len(), 1, "the walk ends with the oldest post");
    assert_eq!(
        second_items[0]["object"]["id"], posts[0].uri,
        "and that is the first post made, as its Create"
    );
    assert_eq!(
        second_items[0]["id"],
        json!(format!("{BASE}/activities/{}", posts[0].id)),
        "the activity carries this instance's address"
    );
    assert_eq!(
        second_document["next"],
        Value::Null,
        "a short page has no next"
    );
    assert_eq!(
        second_document["prev"],
        json!(format!(
            "{BASE}/users/alice/outbox?page=true&min_id={}",
            posts[0].id
        )),
        "and it points back toward the newest end"
    );

    let repeated = second_items.iter().any(|item| {
        items.iter().any(|other| {
            item["object"]["id"]
                .as_str()
                .is_some_and(|uri| other["object"]["id"].as_str() == Some(uri))
        })
    });
    assert!(!repeated, "pages never repeat a row");
}

/// The empty account: the root says zero, and its page is an empty
/// `OrderedCollectionPage` with no walk to follow.
#[tokio::test]
async fn an_empty_outbox_still_answers_root_and_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);

    let root = fetch_ap(&app, "/users/alice/outbox").await;
    common::assert_answer(&root, StatusCode::OK, "an empty outbox root");
    let root_document = root.json_value();
    assert_eq!(root_document["totalItems"], 0, "the count says zero");
    assert_eq!(
        root_document["first"],
        format!("{BASE}/users/alice/outbox?page=true"),
        "the first link stays"
    );

    let page = fetch_ap(&app, "/users/alice/outbox?page=true").await;
    common::assert_answer(&page, StatusCode::OK, "an empty outbox page");
    let page_document = page.json_value();
    assert_eq!(page_document["type"], "OrderedCollectionPage");
    assert_eq!(page_document["orderedItems"], json!([]), "nothing to list");
    assert_eq!(page_document["next"], Value::Null, "and no walk");
    assert_eq!(page_document["prev"], Value::Null, "in either direction");
    assert_eq!(
        page_document["partOf"],
        format!("{BASE}/users/alice/outbox"),
        "the page still belongs to the collection"
    );
}
