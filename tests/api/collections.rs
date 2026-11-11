//! Tests for the `ActivityPub` collection routes of
//! `src/api/activitypub.rs`, built by `src/federation/collections.rs`:
//! `GET /users/{nickname}/outbox` (and its `?page=true` keyset page),
//! `GET /users/{nickname}/followers` and `/following` (same two-level
//! structure, page size 10, keyed on the follow row id),
//! `GET /users/{nickname}/collections/featured`, and
//! `GET /objects/{uuid}/replies` with its page.
//!
//! The routes negotiate like the actor route (`ActivityPub` `Accept` gets
//! the document, a browser leaves with the frontend redirect, everyone
//! else gets the 404, every answer varies on `Accept`), apply the
//! `AUTHORIZED_FETCH` gate, and answer unknown and remote accounts like
//! the actor route's 404 and deactivated ones like its 410. The pure
//! document shapes are pinned where they are built, in
//! `src/federation/collections.rs`; what these tests own is the route:
//! the stored rows behind the items, the visibility and hide rules over
//! real data, the cursors' walk, the status codes and the headers.

#![cfg(test)]

use axum::Router;
use axum::http::StatusCode;
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    config::Toggle,
    federation::signature::{SignedHeaders, SigningKey, sign_request},
    models::{
        follow::{self, Follow},
        pinned_status,
        status::{self, NewStatus, Status, Visibility},
        user::{self, Flag, NewUser, User},
    },
    services::instance_actor::ensure_instance_actor,
};

use crate::common;

/// The media type of an `ActivityPub` document, charset included.
const ACTIVITY_JSON: &str = "application/activity+json; charset=utf-8";
/// The instance origin of [`common::test_config`].
const BASE: &str = TEST_BASE_URL;
/// The `ActivityStreams` public addressing collection.
const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The refusal body of every unsatisfied `AUTHORIZED_FETCH` check.
const REFUSAL: &str = "This instance requires authenticated fetches";

/// Fetches `path` as an `ActivityPub` client.
async fn fetch_ap(app: &Router, path: &str) -> common::TestResponse {
    common::TestRequest::get(path)
        .header("accept", "application/activity+json")
        .send(app)
        .await
}

/// The sorted key set of a document object.
fn key_set(document: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = document
        .as_object()
        .expect("a document is a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// The account named `nickname` as stored now — the row whose denormalised
/// counters the collection roots show.
async fn stored_user(pool: &PgPool, nickname: &str) -> User {
    user::get_by_nickname(pool, nickname)
        .await
        .expect("the user row reads back")
        .expect("the fixture user exists")
}

/// `author`'s answer to `parent` as a status of `visibility`.
async fn reply_to(
    pool: &PgPool,
    author: &User,
    parent: &Status,
    text: &str,
    visibility: Visibility,
) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            visibility,
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("a reply fixture inserts")
}

/// `author`'s public boost of `original`, as its reblog row.
async fn boost(pool: &PgPool, author: &User, original: &Status) -> Status {
    let (row, created) = status::create_reblog(pool, BASE, author, original, Visibility::Public)
        .await
        .expect("a reblog fixture writes");
    assert!(created, "the fixture pair had no live reblog yet");
    row
}

/// `follower`'s follow of the open local `target`, accepted on the spot;
/// the returned row is the relationship the follow collections key on.
async fn granted(pool: &PgPool, follower: &User, target: &User) -> Follow {
    follow::follow(pool, BASE, follower, target)
        .await
        .expect("the fixture follow records")
}

/// The pinned-status write the profile's pin endpoint performs, run direct.
async fn pin(pool: &PgPool, author: &User, status_id: Uuid) {
    let pinned = pinned_status::pin(pool, author.id, status_id)
        .await
        .expect("the fixture pin writes");
    assert!(pinned.is_some(), "the pair had no pin yet");
}

/// Tombstones `row` (the delete's own cascade of counters and pins runs).
async fn unpost(pool: &PgPool, row: &Status) {
    let tombstone = status::soft_delete(pool, row.id)
        .await
        .expect("the fixture delete runs");
    assert!(tombstone.is_some(), "the fixture row was live to delete");
}

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

/// Alice's fixture for the page tests: a public root with a public answer,
/// an unlisted post, a private, a direct and a deleted row, and a public
/// boost of bob's post — created in that order, so the expected page order
/// is the reverse of it.
#[derive(Debug)]
struct OutboxFixture {
    /// The account whose outbox the tests read.
    alice: User,
    /// The author of the boosted post.
    bob: User,
    /// Alice's oldest public root post.
    first: Status,
    /// Alice's answer to `first`.
    answer: Status,
    /// Alice's unlisted post.
    quiet: Status,
    /// Bob's boosted original.
    boosted: Status,
    /// Alice's reblog of `boosted`.
    reblog: Status,
}

/// Builds [`OutboxFixture`].
async fn outbox_fixture(pool: &PgPool) -> OutboxFixture {
    let alice = common::create_local_user(pool, "alice").await;
    let bob = common::create_local_user(pool, "bob").await;
    let first = common::create_status(pool, &alice, "first", Visibility::Public).await;
    let answer = reply_to(pool, &alice, &first, "answering", Visibility::Public).await;
    let _hidden = common::create_status(pool, &alice, "followers only", Visibility::Private).await;
    let quiet = common::create_status(pool, &alice, "quiet", Visibility::Unlisted).await;
    let _dm = common::create_status(pool, &alice, "dms", Visibility::Direct).await;
    let boosted = common::create_status(pool, &bob, "bob speaks", Visibility::Public).await;
    let reblog = boost(pool, &alice, &boosted).await;
    let gone = common::create_status(pool, &alice, "gone", Visibility::Public).await;
    unpost(pool, &gone).await;
    OutboxFixture {
        alice,
        bob,
        first,
        answer,
        quiet,
        boosted,
        reblog,
    }
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
        key_set(&document),
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
        key_set(&counted_document),
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
        key_set(&document),
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

/// Every user-keyed collection route answers like the actor route:
/// unknown and remote accounts are the 404, a deactivated one the 410.
#[tokio::test]
async fn collection_routes_answer_404_and_410_like_the_actor_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let _ex = common::create_local_user_with(&pool, "ex", |new_user| NewUser {
        is_active: Flag::new(false),
        ..new_user
    })
    .await;
    let app = common::test_app(pool);

    let gone_paths = [
        "/users/ex/outbox".to_owned(),
        "/users/ex/followers".to_owned(),
        "/users/ex/following".to_owned(),
        "/users/ex/collections/featured".to_owned(),
    ];
    for path in gone_paths {
        let response = fetch_ap(&app, &path).await;
        common::assert_answer(&response, StatusCode::GONE, &path);
        assert_eq!(
            response.json_value(),
            json!({"error": "Gone"}),
            "{path}: gone, like the actor route"
        );
    }

    let missing_names = ["nobody", bob.nickname.as_str()];
    let roots = [
        "/outbox",
        "/followers",
        "/following",
        "/collections/featured",
    ];
    for name in missing_names {
        for root in roots {
            let path = format!("/users/{name}{root}");
            let plain = fetch_ap(&app, &path).await;
            common::assert_answer(&plain, StatusCode::NOT_FOUND, &path);
            assert_eq!(
                plain.json_value(),
                json!({"error": "Not found"}),
                "{path}: the fallback body, like the actor route"
            );
            assert_eq!(
                plain.header("vary"),
                Some("Accept"),
                "{path}: and the negotiation header"
            );
            let paged = fetch_ap(&app, &format!("{path}?page=true")).await;
            common::assert_answer(&paged, StatusCode::NOT_FOUND, &format!("{path}?page=true"));
        }
    }
}

/// The collection routes negotiate like the actor route: either
/// `ActivityPub` media type gets the document, a browser leaves with the
/// frontend redirect (the profile for account paths, the post page for
/// the object's), and any other `Accept` is the 404.
#[tokio::test]
async fn collections_negotiate_content() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &alice, "hi", Visibility::Public).await;
    let app = common::test_app(pool);

    let accepts = [
        "application/activity+json",
        "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"",
    ];
    for accept in accepts {
        let response = common::TestRequest::get("/users/alice/outbox")
            .header("accept", accept)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{accept} speaks ActivityPub"
        );
        assert_eq!(
            response.header("content-type"),
            Some(ACTIVITY_JSON),
            "{accept} gets the ActivityPub media type"
        );
    }

    let browser_asks = [
        ("/users/alice/outbox", format!("{BASE}/alice")),
        ("/users/alice/followers", format!("{BASE}/alice")),
        ("/users/alice/following", format!("{BASE}/alice")),
        ("/users/alice/collections/featured", format!("{BASE}/alice")),
        (
            &format!("/objects/{}/replies", post.id),
            format!("{BASE}/notice/{}", post.id),
        ),
    ];
    for (path, location) in browser_asks {
        let browser = common::TestRequest::get(path)
            .header("accept", "text/html")
            .send(&app)
            .await;
        assert_eq!(
            browser.status,
            StatusCode::FOUND,
            "{path} sends the browser on"
        );
        assert_eq!(
            browser.header("location"),
            Some(location.as_str()),
            "{path}"
        );
        assert_eq!(browser.header("vary"), Some("Accept"), "{path} varies");
    }

    let strangers = ["*/*", "application/json", "text/plain"];
    for accept in strangers {
        let response = common::TestRequest::get("/users/alice/following")
            .header("accept", accept)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{accept} is no ActivityPub"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Not found"}),
            "the fallback body for {accept}"
        );
    }
}

/// A cursor that is no id is the shared 400, on every paged route.
#[tokio::test]
async fn bad_cursors_are_the_shared_invalid_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &alice, "hi", Visibility::Public).await;
    let app = common::test_app(pool);

    let paged = [
        "/users/alice/outbox".to_owned(),
        "/users/alice/followers".to_owned(),
        "/users/alice/following".to_owned(),
        format!("/objects/{}/replies", post.id),
    ];
    for path in paged {
        let response = fetch_ap(&app, &format!("{path}?page=true&max_id=nonsense")).await;
        let who = format!("{path} with a bad cursor");
        common::assert_answer(&response, StatusCode::BAD_REQUEST, &who);
        assert_eq!(
            response.json_value(),
            json!({"error": "Invalid id"}),
            "{who}: the shared pagination refusal"
        );
    }
}

/// A signed `GET` of `path` as an `ActivityPub` client, with `signed`'s
/// headers added.
async fn signed_get(app: &Router, path: &str, signed: &SignedHeaders) -> common::TestResponse {
    common::TestRequest::get(path)
        .header("accept", "application/activity+json")
        .header("host", &signed.host)
        .header("date", &signed.date)
        .header("signature", &signed.signature)
        .send(app)
        .await
}

/// With `AUTHORIZED_FETCH` on, every `ActivityPub` read route demands a
/// valid HTTP Signature first: the unsigned request is the 401, and a
/// signature made over the exact path (query included) is served.
#[tokio::test]
async fn authorized_fetch_gates_every_collection_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &alice, "hi", Visibility::Public).await;
    let mut config = common::test_config();
    config.authorized_fetch = Toggle::new(true);
    let app = common::test_app_with_config(pool.clone(), config.clone());

    let gated = [
        "/users/alice/outbox".to_owned(),
        "/users/alice/outbox?page=true".to_owned(),
        "/users/alice/followers".to_owned(),
        "/users/alice/following".to_owned(),
        "/users/alice/collections/featured".to_owned(),
        format!("/objects/{}/replies", post.id),
    ];
    for path in gated {
        let response = fetch_ap(&app, &path).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{path} is gated");
        assert_eq!(response.json_value()["error"], REFUSAL, "{path} says why");
    }

    let actor = ensure_instance_actor(&pool, &config)
        .await
        .expect("the instance actor is provisioned");
    let key_id = format!("{}#main-key", actor.ap_id);
    let opened = [
        "/users/alice/outbox?page=true".to_owned(),
        "/users/alice/collections/featured".to_owned(),
        format!("/objects/{}/replies", post.id),
    ];
    for path in opened {
        let signed = sign_request(
            "GET",
            &format!("{BASE}{path}"),
            None,
            &SigningKey {
                key_id: &key_id,
                private_pem: actor
                    .private_key
                    .as_deref()
                    .expect("the provisioned actor holds its private key"),
            },
            Utc::now(),
        )
        .expect("signing the test fetch");
        let response = signed_get(&app, &path, &signed).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{path} opens to its signature"
        );
    }
}
