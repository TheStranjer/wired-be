//! The client-to-server inbox read (`GET /users/{nickname}/inbox`): the
//! owner-only gates, the `OrderedCollection` root and its paged mentions
//! as `Create` activities with their `next`/`prev` cursors, and the shared
//! 400 of a malformed cursor.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{status::Visibility, user::User};

use super::support::{c2s_account, mention_status};
use crate::common;

// ---------------------------------------------------------------------------
// Client-to-server: the owner's inbox read, outbox writes, whoami and the
// C2S media upload (Pleroma's `read_inbox`, `update_outbox`, `whoami` and
// `upload_media`).
// ---------------------------------------------------------------------------

/// `GET /users/{nickname}/inbox` as `token`.
async fn read_inbox(
    app: &Router,
    nickname: &str,
    token: Option<&str>,
    query: &str,
) -> common::TestResponse {
    let path = if query.is_empty() {
        format!("/users/{nickname}/inbox")
    } else {
        format!("/users/{nickname}/inbox?{query}")
    };
    let request = common::TestRequest::get(&path);
    let request = match token {
        Some(token) => request.bearer(token),
        None => request,
    };
    request.send(app).await
}

/// The C2S inbox's gates: without a token the read answers Pleroma's
/// `EnsureAuthenticatedPlug` 401, a stranger's token and a scope-less one
/// the 403s — the wrong user the ticket's `{"error":"Forbidden"}`, the
/// missing scope Pleroma's `OAuthScopesPlug` wording — while `read` and
/// the `read:statuses` sub-scope both pass.
#[tokio::test]
async fn the_c2s_inbox_only_answers_its_owner() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, alice_read) = c2s_account(&pool, "c2s-inbox-alice", "read").await;
    let (_bob, bob_token) = c2s_account(&pool, "c2s-inbox-bob", "read").await;
    let (_, alice_write) = c2s_account(&pool, "c2s-inbox-carol", "write").await;

    let anonymous = read_inbox(&app, "c2s-inbox-alice", None, "").await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "inbox, no token");
    common::assert_error(&anonymous, "Invalid credentials", "inbox, no token");

    let stranger = read_inbox(&app, "c2s-inbox-alice", Some(&bob_token), "").await;
    common::assert_answer(&stranger, StatusCode::FORBIDDEN, "inbox of another");
    common::assert_error(&stranger, "Forbidden", "inbox of another");

    let unscoped = read_inbox(&app, "c2s-inbox-alice", Some(&alice_write), "").await;
    common::assert_answer(&unscoped, StatusCode::FORBIDDEN, "inbox, no read scope");
    common::assert_error(
        &unscoped,
        "Insufficient permissions: read | read:statuses.",
        "inbox, no read scope",
    );

    let owner = read_inbox(&app, "c2s-inbox-alice", Some(&alice_read), "").await;
    assert_eq!(
        owner.status,
        StatusCode::OK,
        "the owner reads: {}",
        owner.text()
    );
    assert_eq!(
        owner.header("content-type"),
        Some("application/activity+json; charset=utf-8"),
        "the inbox is an ActivityPub document"
    );

    let (dave, dave_sub) = c2s_account(&pool, "c2s-inbox-dave", "read:statuses").await;
    let sub = read_inbox(&app, &dave.nickname, Some(&dave_sub), "").await;
    assert_eq!(
        sub.status,
        StatusCode::OK,
        "read:statuses suffices as well: {}",
        sub.text()
    );
}

/// `GET /users/{nickname}/inbox` without `?page=true` answers the
/// collection root: the inbox's own address, the `first` page link, and
/// no items or totals of its own (Pleroma's `activity_collection.json`).
#[tokio::test]
async fn the_c2s_inbox_root_is_an_ordered_collection() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = c2s_account(&pool, "c2s-inbox-root", "read").await;

    let response = read_inbox(&app, "c2s-inbox-root", Some(&token), "").await;
    assert_eq!(response.status, StatusCode::OK, "the owner reads");
    let root = response.json_value();
    let keys: Vec<&str> = {
        let mut keys: Vec<&str> = root
            .as_object()
            .expect("a collection is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    };
    assert_eq!(
        keys,
        ["@context", "first", "id", "type"],
        "the root carries the address, the type and the first link"
    );
    assert_eq!(root["id"], format!("{}/inbox", alice.ap_id), "its own id");
    assert_eq!(root["type"], "OrderedCollection", "the collection type");
    assert_eq!(
        root["first"],
        format!("{}/inbox?page=true", alice.ap_id),
        "the first page hangs off the same address"
    );
}

/// Seeds `nickname`'s inbox world: twenty-three public mentions by the
/// pager, then a direct mention and one final public mention (the newest
/// facts), and the two posts that must stay out of the listing — a public
/// post mentioning nobody and one by the owner herself. Answers the
/// mentioned status ids, newest first.
async fn seed_inbox_mentions(pool: &PgPool, owner: &User, pager: &User) -> Vec<Uuid> {
    let mut expected: Vec<Uuid> = Vec::new();
    for i in 1..=23 {
        let row = mention_status(
            pool,
            pager,
            owner,
            &format!("mention {i}"),
            Visibility::Public,
        )
        .await;
        expected.push(row.id);
    }
    let dm = mention_status(pool, pager, owner, "a quiet word", Visibility::Direct).await;
    let late = mention_status(pool, pager, owner, "mention 24", Visibility::Public).await;
    expected.push(dm.id);
    expected.push(late.id);
    let _unmentioned =
        common::create_status(pool, pager, "to nobody in particular", Visibility::Public).await;
    let _own = common::create_status(pool, owner, "my own post", Visibility::Public).await;
    expected.sort_unstable();
    expected.reverse();
    expected
}

/// The paged C2S inbox lists the statuses that mention the owner as their
/// `Create` activities, newest first, twenty to a page: the direct
/// messages and mentions in, the unmentioned posts and the owner's own
/// posts out, and the `next`/`prev` cursors of the keyset walk (decision 4
/// keys it on the status id).
#[tokio::test]
async fn the_c2s_inbox_pages_mentions_as_create_activities() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = c2s_account(&pool, "c2s-inbox-page", "read").await;
    let bob = common::create_local_user(&pool, "c2s-inbox-pager").await;
    let expected = seed_inbox_mentions(&pool, &alice, &bob).await;

    let first = read_inbox(&app, "c2s-inbox-page", Some(&token), "page=true").await;
    assert_eq!(first.status, StatusCode::OK, "the first page");
    let page = first.json_value();
    assert_eq!(page["type"], "OrderedCollectionPage", "the page type");
    assert_eq!(
        page["partOf"],
        format!("{}/inbox", alice.ap_id),
        "the page names its root"
    );
    assert_eq!(
        page["id"],
        format!("{}/inbox?page=true", alice.ap_id),
        "and echoes the address it was requested at"
    );
    let items = page["orderedItems"].as_array().expect("ordered items");
    assert_eq!(items.len(), 20, "the page size is the ticket's twenty");
    for item in items {
        assert_eq!(item["type"], "Create", "every entry is a Create");
        assert_eq!(item["actor"], bob.ap_id.as_str(), "by its author");
    }
    let page_ids: Vec<Uuid> = items.iter().map(status_of_item).collect();
    assert_eq!(
        page_ids,
        expected[..20],
        "newest first, and the unmentioned posts are absent"
    );
    assert!(
        items
            .iter()
            .any(|item| item["to"] == json!([alice.ap_id.as_str()])),
        "the direct message reaches the inbox as a Create addressed to the owner alone"
    );
    let oldest = *page_ids.last().expect("a full page has a last entry");
    assert_eq!(
        page["next"],
        json!(format!("{}/inbox?page=true&max_id={oldest}", alice.ap_id)),
        "the next page walks to the oldest id"
    );
    assert_eq!(
        page["prev"],
        json!(format!(
            "{}/inbox?page=true&min_id={}",
            alice.ap_id, expected[0]
        )),
        "and a page back toward the newest end is keyed on its newest"
    );

    let second = read_inbox(
        &app,
        "c2s-inbox-page",
        Some(&token),
        &format!("page=true&max_id={oldest}"),
    )
    .await;
    assert_eq!(
        second.status,
        StatusCode::OK,
        "the second page: {}",
        second.text()
    );
    let page2 = second.json_value();
    let items2 = page2["orderedItems"].as_array().expect("ordered items");
    assert_eq!(items2.len(), 5, "the tail of the walk holds five");
    assert!(page2.get("next").is_none(), "a short page ends the walk");
    let ids2: Vec<Uuid> = items2.iter().map(status_of_item).collect();
    assert_eq!(ids2, expected[20..], "and continues newest-first");
    assert_eq!(ids2[4], expected[24], "the walk ends at the oldest mention");
}

/// The status id a listed `Create` names, read off its object's
/// `{BASE}/objects/{uuid}` id.
fn status_of_item(item: &Value) -> Uuid {
    let uri = item["object"]["id"].as_str().expect("the object's id");
    Uuid::parse_str(uri.rsplit('/').next().expect("a final path segment"))
        .expect("the tail is the status id")
}

/// A malformed `max_id` cursor is the shared 400 every keyset walk gives.
#[tokio::test]
async fn the_c2s_inbox_refuses_a_malformed_cursor() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = c2s_account(&pool, "c2s-inbox-cursor", "read").await;

    let response = read_inbox(
        &app,
        "c2s-inbox-cursor",
        Some(&token),
        "page=true&max_id=nope",
    )
    .await;
    common::assert_answer(&response, StatusCode::BAD_REQUEST, "a bogus cursor");
    common::assert_error(&response, "Invalid id", "a bogus cursor");
}
