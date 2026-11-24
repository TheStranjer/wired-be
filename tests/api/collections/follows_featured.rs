//! The follower and following collections, hidden lists and featured.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::models::{
    follow,
    status::Visibility,
    user::{Flag, NewUser},
};

use super::support::{ACTIVITY_JSON, BASE, fetch_ap, granted, pin};
use crate::common;

/// The followers and following collections: the roots count what the
/// account's stored counters hold, and the pages list the actor ids
/// newest relationship first, keyed on the follow row ids.
#[tokio::test]
async fn the_follow_collections_page_actor_ids_newest_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let _bob_follow = granted(&pool, &bob, &alice).await;
    let carol_follow = granted(&pool, &carol, &alice).await;
    let alice_follow = granted(&pool, &alice, &bob).await;
    let app = common::test_app(pool);

    let root = fetch_ap(&app, "/users/alice/followers").await;
    common::assert_answer(&root, StatusCode::OK, "the followers root");
    let document = root.json_value();
    assert_eq!(
        common::sorted_keys(&document),
        ["@context", "first", "id", "totalItems", "type"],
        "the same two-level summary as the outbox's"
    );
    assert_eq!(document["id"], format!("{BASE}/users/alice/followers"));
    assert_eq!(document["type"], "OrderedCollection");
    assert_eq!(document["totalItems"], 2, "both followers, stored count");
    assert_eq!(
        document["first"],
        format!("{BASE}/users/alice/followers?page=true")
    );

    let page = fetch_ap(&app, "/users/alice/followers?page=true").await;
    common::assert_answer(&page, StatusCode::OK, "the followers page");
    let page_document = page.json_value();
    assert_eq!(page_document["type"], "OrderedCollectionPage");
    assert_eq!(
        page_document["orderedItems"],
        json!([carol.ap_id, bob.ap_id]),
        "the actor ids, newest relationship first"
    );
    assert_eq!(
        page_document["prev"],
        json!(format!(
            "{BASE}/users/alice/followers?page=true&min_id={}",
            carol_follow.id
        )),
        "the cursor keys on the follow row id, not the account"
    );
    assert_eq!(page_document["next"], Value::Null, "two is well under 10");

    let following = fetch_ap(&app, "/users/alice/following?page=true").await;
    common::assert_answer(&following, StatusCode::OK, "the following page");
    let following_document = following.json_value();
    assert_eq!(
        following_document["orderedItems"],
        json!([bob.ap_id]),
        "the other side lists whom alice follows"
    );
    assert_eq!(
        following_document["prev"],
        json!(format!(
            "{BASE}/users/alice/following?page=true&min_id={}",
            alice_follow.id
        )),
        "and keys its walk on its own follow rows"
    );
    let following_root = fetch_ap(&app, "/users/alice/following").await;
    assert_eq!(
        following_root.json_value()["totalItems"],
        1,
        "the following count is the stored one"
    );
}

/// Twelve followers page ten, then the rest: the same walk as the
/// outbox's, on the follow row ids.
#[tokio::test]
async fn follow_pages_walk_on_the_relationship_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mut rows = Vec::new();
    let mut fans = Vec::new();
    for n in 1..=12 {
        let name = format!("fan{n}");
        let fan = common::create_local_user(&pool, &name).await;
        let row = granted(&pool, &fan, &alice).await;
        fans.push(fan);
        rows.push(row);
    }
    let app = common::test_app(pool);

    let first = fetch_ap(&app, "/users/alice/followers?page=true").await;
    let document = first.json_value();
    let items = document["orderedItems"].as_array().expect("items");
    assert_eq!(items.len(), 10, "page size ten");
    assert_eq!(items[0], fans[11].ap_id, "newest relationship first");
    let oldest = rows[2].id;
    assert_eq!(
        document["next"],
        json!(format!(
            "{BASE}/users/alice/followers?page=true&max_id={oldest}"
        )),
        "the cursor is the oldest follow row id shown"
    );

    let second = fetch_ap(
        &app,
        &format!("/users/alice/followers?page=true&max_id={oldest}"),
    )
    .await;
    let second_document = second.json_value();
    assert_eq!(
        second_document["orderedItems"],
        json!([fans[1].ap_id, fans[0].ap_id]),
        "the two oldest relationships, newest first, close the walk"
    );
    assert_eq!(
        second_document["next"],
        Value::Null,
        "a short page has no next"
    );
}

/// A withdrawn relationship leaves the list and the root's count with it.
#[tokio::test]
async fn unfollowing_steps_out_of_the_followers_collection() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let late = common::create_local_user(&pool, "late").await;
    let withdrawn = common::create_local_user(&pool, "withdrawn").await;
    let _late_follow = granted(&pool, &late, &alice).await;
    let gone_follow = granted(&pool, &withdrawn, &alice).await;
    let removed = follow::unfollow(&pool, &withdrawn, &alice)
        .await
        .expect("the unfollow runs");
    assert!(removed, "the relationship was there to take back");
    let app = common::test_app(pool);

    let page = fetch_ap(&app, "/users/alice/followers?page=true").await;
    assert_eq!(
        page.json_value()["orderedItems"],
        json!([late.ap_id]),
        "the withdrawn row left the list"
    );
    let root = fetch_ap(&app, "/users/alice/followers").await;
    assert_eq!(
        root.json_value()["totalItems"],
        1,
        "and the stored count the root shows moved with it"
    );
    assert_eq!(gone_follow.followee_id, alice.id, "fixture sanity");
}

/// The hide flags: a hidden list's pages answer empty while its root
/// stays, and a hidden count strips `totalItems` from the root alone.
#[tokio::test]
async fn hidden_lists_page_empty_and_hidden_counts_say_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |new_user| NewUser {
        hide_followers: Flag::new(true),
        hide_follows: Flag::new(true),
        ..new_user
    })
    .await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _bob_to_alice = granted(&pool, &bob, &alice).await;
    let _alice_to_bob = granted(&pool, &alice, &bob).await;
    let counted = common::create_local_user_with(&pool, "counted", |new_user| NewUser {
        hide_followers_count: Flag::new(true),
        hide_follows_count: Flag::new(true),
        ..new_user
    })
    .await;
    let _to_counted = granted(&pool, &bob, &counted).await;
    let app = common::test_app(pool);

    let root = fetch_ap(&app, "/users/alice/followers").await;
    common::assert_answer(&root, StatusCode::OK, "the hidden list's root exists");
    let document = root.json_value();
    assert_eq!(
        document["totalItems"], 1,
        "the list is hidden, the count is not"
    );
    assert_eq!(
        document["first"],
        format!("{BASE}/users/alice/followers?page=true"),
        "and the root still links its pages"
    );

    let hidden_pages = [
        "/users/alice/followers?page=true",
        "/users/alice/following?page=true",
    ];
    for path in hidden_pages {
        let page = fetch_ap(&app, path).await;
        common::assert_answer(&page, StatusCode::OK, path);
        let page_document = page.json_value();
        assert_eq!(
            page_document["orderedItems"],
            json!([]),
            "{path}: a hidden list pages empty"
        );
        assert_eq!(
            page_document["next"],
            Value::Null,
            "{path}: nothing to walk"
        );
    }

    let counted_root = fetch_ap(&app, "/users/counted/followers").await;
    let counted_document = counted_root.json_value();
    assert_eq!(
        common::sorted_keys(&counted_document),
        ["@context", "first", "id", "type"],
        "a hidden count means no totalItems key at all"
    );
    let counted_page = fetch_ap(&app, "/users/counted/followers?page=true").await;
    assert_eq!(
        counted_page.json_value()["orderedItems"],
        json!([bob.ap_id]),
        "and the count says nothing about the list"
    );
}

/// The featured collection: the account's pinned posts as whole note
/// objects, inline, newest pin first, with the count beside them.
#[tokio::test]
async fn the_featured_collection_inlines_whole_notes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let first = common::create_status(&pool, &alice, "the old pin", Visibility::Public).await;
    let second = common::create_status(&pool, &alice, "the new pin", Visibility::Public).await;
    pin(&pool, &alice, first.id).await;
    pin(&pool, &alice, second.id).await;
    let nobody = common::create_local_user(&pool, "nobody").await;
    let app = common::test_app(pool);

    let response = fetch_ap(&app, "/users/alice/collections/featured").await;
    common::assert_answer(&response, StatusCode::OK, "the featured collection");
    assert_eq!(
        response.header("content-type"),
        Some(ACTIVITY_JSON),
        "an ActivityPub document"
    );
    let document = response.json_value();
    assert_eq!(
        common::sorted_keys(&document),
        ["@context", "id", "orderedItems", "totalItems", "type"],
        "one document, items inline, no page links"
    );
    assert_eq!(
        document["id"],
        format!("{BASE}/users/alice/collections/featured"),
        "the address the actor document names"
    );
    assert_eq!(document["type"], "OrderedCollection");
    assert_eq!(document["totalItems"], 2, "the pinned count");
    let objects = document["orderedItems"].as_array().expect("ordered items");
    assert_eq!(objects.len(), 2);
    assert_eq!(
        objects[0]["id"], second.uri,
        "newest pin first, as a whole object"
    );
    assert_eq!(objects[0]["type"], "Note");
    assert_eq!(objects[0]["content"], "<p>the new pin</p>");
    assert_eq!(objects[0]["attributedTo"], alice.ap_id);
    assert_eq!(objects[1]["id"], first.uri);

    let empty = fetch_ap(&app, "/users/nobody/collections/featured").await;
    let empty_document = empty.json_value();
    assert_eq!(empty_document["totalItems"], 0, "an account with no pins");
    assert_eq!(empty_document["orderedItems"], json!([]), "lists nothing");
    assert_eq!(
        empty_document["id"],
        format!("{BASE}/users/{}/collections/featured", nobody.nickname),
        "and the id is the account's own"
    );
}
