//! The conversation endpoints over HTTP: Mastodon's `GET
//! /api/v1/conversations`, `POST /api/v1/conversations/{id}/read` and
//! `DELETE /api/v1/conversations/{id}`, and Pleroma's
//! `/api/v1/pleroma/conversations` show, statuses, recipient rewrite and
//! read-all (WBE-130).
//!
//! Every route answers the `Conversation` entity — the viewer's own
//! participation id, `unread` the row's flag inverted, `accounts` the
//! thread's other participants, and `last_status` the newest status of the
//! thread rendered for the viewer (`null` when nothing renderable is
//! left). The listing pages by participation ids (default 20, cap 40),
//! newest activity first, under a `Link` header keyed on the ids of the
//! page; `recipients[]` keeps only the threads addressing every named
//! account. Ownership is the id: each account's participations carry their
//! own ids, so a foreign one reads as the Mastodon 404 "Record not found"
//! on the v1 routes and Pleroma's "Unknown conversation id" on the
//! `/pleroma` ones. Withdrawing a participation deletes only this side of
//! the thread — the statuses stay and the other participants keep listing
//! it. The rewrite replaces the recipient list wholesale with the body's
//! accounts (the viewer excluded automatically, an unknown id refused),
//! and the read-all flips every unread participation at once, answering
//! exactly the rows that changed.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        block, conversation,
        status::{self, NewStatus, Status, Visibility},
        user::User,
    },
};

use crate::common;

/// The cap on the recipient array of the rewrite.
mod batch_cap;

/// The scope the three reads demand.
const READ: &str = "read:statuses";
/// The scope the four writes demand.
const WRITE: &str = "write:conversations";
/// Both halves at once: what the acting fixtures carry.
const BOTH: &str = "read:statuses write:conversations";
/// The 404 body of the Mastodon v1 routes (decision 8).
const NOT_FOUND: &str = "Record not found";
/// The 404 body of Pleroma's conversation controller's routes.
const UNKNOWN: &str = "Unknown conversation id";
/// The 401 body `AuthUser` renders when the request carries no token.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body `AuthUser` renders for a token bound to no user.
const NO_USER: &str = "This API requires an authenticated user";
/// The 400 body for a cursor that is not id text.
const INVALID_ID: &str = "Invalid id";
/// An id of a participation nobody holds, for the miss probes.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

/// A local user named `nickname` with a bearer token granting `scopes`.
async fn actor(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// Inserts `author`'s `direct` status addressed to everyone in
/// `mentioned` and runs the create hook's participation write for it —
/// the state a posted DM leaves behind.
async fn dm(pool: &PgPool, author: &User, mentioned: &[&User], text: &str) -> Status {
    let ids: Vec<Uuid> = mentioned.iter().map(|who| who.id).collect();
    let post = status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility: Visibility::Direct,
            mentioned_user_ids: ids,
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("the direct fixture inserts");
    conversation::participate_for_status(pool, &post)
        .await
        .expect("the participation hook runs");
    post
}

/// A `direct` reply by `author` to `parent`, threaded into the parent's
/// conversation by the same hook.
async fn reply(pool: &PgPool, author: &User, parent: &Status, text: &str) -> Status {
    let mentioned = if author.id == parent.user_id {
        Vec::new()
    } else {
        vec![parent.user_id]
    };
    let post = status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility: Visibility::Direct,
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(parent.user_id),
            conversation_id: Some(parent.conversation_id),
            mentioned_user_ids: mentioned,
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("the direct reply fixture inserts");
    conversation::participate_for_status(pool, &post)
        .await
        .expect("the participation hook runs");
    post
}

/// The DM thread `post` was stored into.
async fn thread_of(pool: &PgPool, post: &Status) -> Uuid {
    status::get_by_id(pool, post.id)
        .await
        .expect("the row reads")
        .expect("the fixture is live")
        .direct_conversation_id
        .expect("the hook threaded the fixture")
}

/// `who`'s own participation id in the thread of `post`.
async fn participation_for(pool: &PgPool, who: &User, post: &Status) -> String {
    let thread = thread_of(pool, post).await;
    conversation::participation_ids(pool, who.id, &[thread])
        .await
        .expect("the participation batch reads")
        .get(&thread)
        .expect("the fixture account participates")
        .to_string()
}

/// A request builder for `verb` against `path`.
fn request(verb: &str, path: &str) -> common::TestRequest {
    match verb {
        "GET" => common::TestRequest::get(path),
        "POST" => common::TestRequest::post(path),
        "DELETE" => common::TestRequest::delete(path),
        _ => common::TestRequest::patch(path),
    }
}

/// `GET /api/v1/conversations` with `query` (its leading `?` included or
/// absent) as `token`.
async fn list(app: &Router, token: &str, query: &str) -> common::TestResponse {
    request("GET", &format!("/api/v1/conversations{query}"))
        .bearer(token)
        .send(app)
        .await
}

/// A `GET` of one Pleroma conversation path as `token`.
async fn pleroma_get(app: &Router, token: &str, path: &str) -> common::TestResponse {
    request("GET", &format!("/api/v1/pleroma/conversations{path}"))
        .bearer(token)
        .send(app)
        .await
}

/// The listing answers one entity per owned participation: id, `unread`
/// the inverted row flag, `accounts` the thread's other participants
/// (never the viewer), and `last_status` the rendered newest post — and
/// the thread that saw activity most recently leads the list even though
/// its participation is the oldest row.
#[tokio::test]
async fn the_listing_orders_newest_activity_first_and_renders_the_entity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = actor(&pool, "alice", BOTH).await;
    let (bob, bob_token) = actor(&pool, "bob", BOTH).await;
    let app = common::test_app(pool.clone());
    let first = dm(&pool, &alice, &[&bob], "@bob first").await;
    let second = dm(&pool, &alice, &[&bob], "@bob second").await;
    let echoed = reply(&pool, &bob, &first, "@alice back").await;

    let page = list(&app, &bob_token, "").await;
    common::assert_answer(&page, StatusCode::OK, "bob's listing");
    let entities = page.json_value();
    assert_eq!(
        common::ids_of(&entities),
        vec![
            participation_for(&pool, &bob, &first).await,
            participation_for(&pool, &bob, &second).await,
        ],
        "the thread bob just posted into moved in front of the newer thread"
    );
    let leading = &entities[0];
    assert_eq!(
        leading["last_status"]["id"],
        echoed.id.to_string(),
        "the newest status of the thread is the entity's `last_status`"
    );
    assert_eq!(leading["unread"], false, "bob read his own reply");
    assert_eq!(
        leading["accounts"]
            .as_array()
            .expect("accounts")
            .iter()
            .map(|account| account["id"].as_str().expect("an id"))
            .collect::<Vec<_>>(),
        vec![alice.id.to_string()],
        "the other participant, and never the viewer"
    );
    assert_eq!(
        entities[1]["unread"], true,
        "the thread alice has not answered stands unread"
    );
    assert_eq!(
        entities[1]["last_status"]["content"], "<p>@bob second</p>",
        "the stored post renders through the shared status batch"
    );

    let for_alice = list(&app, &alice_token, "").await.json_value();
    assert_eq!(
        common::ids_of(&for_alice),
        vec![
            participation_for(&pool, &alice, &first).await,
            participation_for(&pool, &alice, &second).await,
        ],
        "alice lists her own participations, with her own ids"
    );
    assert_eq!(
        for_alice[0]["accounts"][0]["id"],
        bob.id.to_string(),
        "and her side of the thread names bob"
    );
}

/// The listing caps at `limit`, keys its `Link` header on the page's
/// participation ids, and walks older pages on `max_id`. A page past the
/// last thread answers `[]` with no `Link` header, and a cursor that is
/// not id text is the pagination 400.
#[tokio::test]
async fn the_listing_pages_by_participation_id_under_a_link_header() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = actor(&pool, "alice", BOTH).await;
    let (bob, bob_token) = actor(&pool, "bob", BOTH).await;
    let app = common::test_app(pool.clone());
    for n in 0..3 {
        let _posted = dm(&pool, &alice, &[&bob], &format!("@bob {n}")).await;
    }

    let page = list(&app, &bob_token, "?limit=2").await;
    let entities = page.json_value();
    assert_eq!(
        entities.as_array().expect("a page").len(),
        2,
        "the cap held, newest participation first"
    );
    let newest = entities[0]["id"].as_str().expect("an id").to_owned();
    let oldest = entities[1]["id"].as_str().expect("an id").to_owned();
    let link = page.header("link").expect("a page links");
    assert!(
        link.contains(&format!("max_id={oldest}")) && link.contains("rel=\"next\""),
        "the next cursor names the page's oldest participation: {link}"
    );
    assert!(
        link.contains(&format!("min_id={newest}")) && link.contains("rel=\"prev\""),
        "the prev cursor names its newest: {link}"
    );

    let older = list(&app, &bob_token, &format!("?limit=2&max_id={oldest}")).await;
    let older_ids = common::ids_of(&older.json_value());
    assert_eq!(older_ids.len(), 1, "one thread walks out past the cursor");
    assert!(older_ids[0] < oldest, "and it is older than the cursor");

    let empty = list(&app, &bob_token, &format!("?max_id={}", older_ids[0])).await;
    common::assert_answer(&empty, StatusCode::OK, "the page past the last");
    assert_eq!(empty.json_value(), json!([]), "and nothing left to list");
    assert!(
        empty.header("link").is_none(),
        "an empty page carries no Link header"
    );

    let default_page = list(&app, &alice_token, "").await.json_value();
    assert_eq!(
        default_page.as_array().expect("a page").len(),
        3,
        "the default 20 fits the whole listing"
    );

    let bad = list(&app, &bob_token, "?max_id=nope").await;
    common::assert_all_refused(
        &[bad],
        StatusCode::BAD_REQUEST,
        INVALID_ID,
        "a malformed cursor",
    );
}

/// `recipients[]` keeps only the conversations whose recipient list
/// addresses every named account; an account nobody is paired with
/// empties the listing.
#[tokio::test]
async fn the_listing_filters_by_recipients() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = actor(&pool, "alice", BOTH).await;
    let (bob, bob_token) = actor(&pool, "bob", BOTH).await;
    let (carol, _) = actor(&pool, "carol", BOTH).await;
    let app = common::test_app(pool.clone());
    let pair = dm(&pool, &alice, &[&bob, &carol], "@bob @carol psst").await;
    let _single = dm(&pool, &alice, &[&bob], "@bob alone").await;
    let pair_id = participation_for(&pool, &bob, &pair).await;

    let filtered = list(&app, &bob_token, &format!("?recipients[]={}", carol.id)).await;
    assert_eq!(
        common::ids_of(&filtered.json_value()),
        vec![pair_id],
        "only the thread whose list also names carol stands"
    );

    let two = list(
        &app,
        &alice_token,
        &format!("?recipients[]={}&recipients[]={}", bob.id, carol.id),
    )
    .await
    .json_value();
    assert_eq!(
        two.as_array().expect("a page").len(),
        1,
        "every named account must be an addressee"
    );

    let stranger = list(&app, &bob_token, &format!("?recipients={UNKNOWN_ID}")).await;
    assert_eq!(
        stranger.json_value().as_array().expect("an answer").len(),
        0,
        "an account nobody addresses leaves nothing"
    );
}

/// `POST /api/v1/conversations/{id}/read` flips the caller's flag and
/// answers the entity; a repeat marks nothing new and answers the same
/// thing. Somebody else's, an unknown and a malformed id are the shared
/// 404.
#[tokio::test]
async fn read_marks_one_participation_and_answers_its_entity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = actor(&pool, "alice", BOTH).await;
    let (bob, bob_token) = actor(&pool, "bob", BOTH).await;
    let app = common::test_app(pool.clone());
    let post = dm(&pool, &alice, &[&bob], "@bob psst").await;
    let mine = participation_for(&pool, &bob, &post).await;
    let hers = participation_for(&pool, &alice, &post).await;

    let marked = request("POST", &format!("/api/v1/conversations/{mine}/read"))
        .bearer(&bob_token)
        .send(&app)
        .await;
    common::assert_answer(&marked, StatusCode::OK, "the mark");
    let entity = marked.json_value();
    assert_eq!(entity["id"], mine, "the entity of the marked row");
    assert_eq!(entity["unread"], false, "now read");
    assert_eq!(entity["accounts"][0]["id"], alice.id.to_string());

    let again = request("POST", &format!("/api/v1/conversations/{mine}/read"))
        .bearer(&bob_token)
        .send(&app)
        .await;
    assert_eq!(again.json_value()["unread"], false, "a repeat restates");

    let misses = [
        request("POST", &format!("/api/v1/conversations/{hers}/read"))
            .bearer(&bob_token)
            .send(&app)
            .await,
        request("POST", &format!("/api/v1/conversations/{UNKNOWN_ID}/read"))
            .bearer(&bob_token)
            .send(&app)
            .await,
        request("POST", "/api/v1/conversations/not-an-id/read")
            .bearer(&bob_token)
            .send(&app)
            .await,
    ];
    common::assert_all_refused(
        &misses,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the v1 read misses",
    );
}

/// `DELETE /api/v1/conversations/{id}` withdraws only the caller's side:
/// the statuses stay live and the other participant keeps listing the
/// thread. A foreign id is the shared 404, and a repeat finds nothing.
#[tokio::test]
async fn delete_withdraws_the_participation_and_keeps_the_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = actor(&pool, "alice", BOTH).await;
    let (bob, bob_token) = actor(&pool, "bob", BOTH).await;
    let app = common::test_app(pool.clone());
    let post = dm(&pool, &alice, &[&bob], "@bob psst").await;
    let mine = participation_for(&pool, &bob, &post).await;
    let hers = participation_for(&pool, &alice, &post).await;

    let foreign = request("DELETE", &format!("/api/v1/conversations/{hers}"))
        .bearer(&bob_token)
        .send(&app)
        .await;
    common::assert_answer(&foreign, StatusCode::NOT_FOUND, "bob deleting alice's row");
    let gone = request("DELETE", &format!("/api/v1/conversations/{mine}"))
        .bearer(&bob_token)
        .send(&app)
        .await;
    common::assert_answer(&gone, StatusCode::OK, "bob's withdrawal");
    assert_eq!(gone.json_value(), json!({}), "the ticket's empty object");

    assert!(
        list(&app, &bob_token, "")
            .await
            .json_value()
            .as_array()
            .expect("an answer")
            .is_empty(),
        "bob no longer lists the thread"
    );
    let still = list(&app, &alice_token, "").await.json_value();
    assert_eq!(
        common::ids_of(&still),
        vec![hers],
        "alice keeps her own side of it"
    );
    assert_eq!(
        still[0]["last_status"]["id"],
        post.id.to_string(),
        "and the status is untouched — still the thread's newest post"
    );
    assert!(
        status::get_by_id(&pool, post.id)
            .await
            .expect("the row reads")
            .is_some(),
        "the statuses survive the withdrawal as rows"
    );

    let repeat = request("DELETE", &format!("/api/v1/conversations/{mine}"))
        .bearer(&bob_token)
        .send(&app)
        .await;
    common::assert_answer(&repeat, StatusCode::NOT_FOUND, "a repeat withdrawal");
}

/// Pleroma's `show` answers the owned entity; a foreign, unknown or
/// malformed id is the controller's own 404 wording.
#[tokio::test]
async fn pleroma_show_answers_the_owned_entity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = actor(&pool, "alice", BOTH).await;
    let (bob, bob_token) = actor(&pool, "bob", BOTH).await;
    let app = common::test_app(pool.clone());
    let post = dm(&pool, &alice, &[&bob], "@bob psst").await;
    let mine = participation_for(&pool, &bob, &post).await;

    let shown = pleroma_get(&app, &bob_token, &format!("/{mine}")).await;
    common::assert_answer(&shown, StatusCode::OK, "the show");
    let entity = shown.json_value();
    assert_eq!(entity["id"], mine);
    assert_eq!(entity["unread"], true, "the untouched side reads unread");
    assert_eq!(entity["accounts"][0]["id"], alice.id.to_string());
    assert_eq!(entity["last_status"]["id"], post.id.to_string());
    assert_eq!(
        entity["last_status"]["pleroma"]["direct_conversation_id"], mine,
        "the embedded status names the viewer's own participation"
    );

    let hers = participation_for(&pool, &alice, &post).await;
    let misses = [
        pleroma_get(&app, &bob_token, &format!("/{hers}")).await,
        pleroma_get(&app, &bob_token, &format!("/{UNKNOWN_ID}")).await,
        pleroma_get(&app, &bob_token, "/not-an-id").await,
        pleroma_get(&app, &alice_token, &format!("/{mine}")).await,
    ];
    common::assert_all_refused(&misses, StatusCode::NOT_FOUND, UNKNOWN, "the show misses");
}

/// Pleroma's `statuses` lists the thread's live direct statuses the
/// caller may see, newest first, one page under a `Link` header keyed on
/// status ids; a participation that is not the caller's is the 404.
#[tokio::test]
async fn pleroma_statuses_page_the_thread_newest_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = actor(&pool, "alice", BOTH).await;
    let (bob, bob_token) = actor(&pool, "bob", BOTH).await;
    let app = common::test_app(pool.clone());
    let root = dm(&pool, &alice, &[&bob], "@bob root").await;
    let answered = reply(&pool, &bob, &root, "@alice answered").await;
    let elsewhere = dm(&pool, &alice, &[&bob], "@bob elsewhere").await;
    let mine = participation_for(&pool, &bob, &root).await;

    let page = pleroma_get(&app, &bob_token, &format!("/{mine}/statuses")).await;
    common::assert_answer(&page, StatusCode::OK, "the statuses page");
    let entities = page.json_value();
    assert_eq!(
        entities
            .as_array()
            .expect("an array")
            .iter()
            .map(|status| status["id"].as_str().expect("an id"))
            .collect::<Vec<_>>(),
        vec![answered.id.to_string(), root.id.to_string()],
        "the thread's two statuses, newest first, other threads out"
    );
    let link = page.header("link").expect("a full page links");
    assert!(
        link.contains(&format!("max_id={}", root.id)) && link.contains("rel=\"next\""),
        "the cursors key on status ids: {link}"
    );

    let short = pleroma_get(&app, &bob_token, &format!("/{mine}/statuses?limit=1")).await;
    assert_eq!(
        short.json_value()[0]["id"],
        answered.id.to_string(),
        "the cap holds and the page starts at the newest"
    );

    let hers = participation_for(&pool, &alice, &elsewhere).await;
    let miss = pleroma_get(&app, &bob_token, &format!("/{hers}/statuses")).await;
    common::assert_all_refused(&[miss], StatusCode::NOT_FOUND, UNKNOWN, "a foreign ask");
}

/// A participant's `direct` status disappears from the statuses page once
/// that author blocks the viewer (the visibility model's rule 2, applied
/// to the whole page in one batch); the statuses left keep their order.
#[tokio::test]
async fn pleroma_statuses_omit_a_status_whose_author_blocked_the_viewer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = actor(&pool, "alice", BOTH).await;
    let (bob, bob_token) = actor(&pool, "bob", BOTH).await;
    let (carol, _) = actor(&pool, "carol", BOTH).await;
    let app = common::test_app(pool.clone());
    let root = dm(&pool, &alice, &[&bob], "@bob root").await;
    let from_carol = status::insert(
        &pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility: Visibility::Direct,
            in_reply_to_id: Some(root.id),
            in_reply_to_account_id: Some(root.user_id),
            conversation_id: Some(root.conversation_id),
            mentioned_user_ids: vec![bob.id],
            ..NewStatus::local(&carol, "@bob from carol")
        },
    )
    .await
    .expect("carol's direct fixture inserts");
    conversation::participate_for_status(&pool, &from_carol)
        .await
        .expect("the participation hook runs");
    let answered = reply(&pool, &bob, &from_carol, "@carol answered").await;
    let mine = participation_for(&pool, &bob, &root).await;
    let path = format!("/{mine}/statuses");

    let before = pleroma_get(&app, &bob_token, &path).await;
    common::assert_answer(&before, StatusCode::OK, "the page before the block");
    assert_eq!(
        common::ids_of(&before.json_value()),
        vec![
            answered.id.to_string(),
            from_carol.id.to_string(),
            root.id.to_string()
        ],
        "all three statuses, newest first, while nobody is blocked"
    );

    let blocked = block::block(&pool, TEST_BASE_URL, &carol, &bob)
        .await
        .expect("carol blocks bob");
    assert_eq!(blocked.blocked_id, bob.id, "the block names bob");
    let after = pleroma_get(&app, &bob_token, &path).await;
    common::assert_answer(&after, StatusCode::OK, "the page after the block");
    assert_eq!(
        common::ids_of(&after.json_value()),
        vec![answered.id.to_string(), root.id.to_string()],
        "carol's status is gone and the others keep their order"
    );
}

/// `PATCH` refuses an account that never authored nor was mentioned in
/// the thread: the 404, the stored list untouched, and the entity never
/// renders the stranger as a participant.
#[tokio::test]
async fn pleroma_update_refuses_an_account_outside_the_thread() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = actor(&pool, "alice", BOTH).await;
    let (bob, _) = actor(&pool, "bob", BOTH).await;
    let (dave, _) = actor(&pool, "dave", BOTH).await;
    let app = common::test_app(pool.clone());
    let post = dm(&pool, &alice, &[&bob], "@bob psst").await;
    let row_id = participation_for(&pool, &alice, &post).await;
    let mine = Uuid::parse_str(&row_id).expect("fixture id");

    let stranger = request("PATCH", &format!("/api/v1/pleroma/conversations/{row_id}"))
        .bearer(&alice_token)
        .json(&json!({"recipients": [bob.id.to_string(), dave.id.to_string()]}))
        .send(&app)
        .await;
    common::assert_all_refused(
        &[stranger],
        StatusCode::NOT_FOUND,
        UNKNOWN,
        "an account that never took part in the thread",
    );
    assert_eq!(
        conversation::recipients(&pool, mine)
            .await
            .expect("the list reads"),
        vec![bob.id],
        "the refused rewrite stored nothing"
    );
    let shown = pleroma_get(&app, &alice_token, &format!("/{row_id}")).await;
    assert_eq!(
        shown.json_value()["accounts"]
            .as_array()
            .expect("accounts")
            .iter()
            .map(|account| account["id"].as_str().expect("an id"))
            .collect::<Vec<_>>(),
        vec![bob.id.to_string()],
        "the stranger never renders as a participant"
    );
}

/// `PATCH` replaces the participation's whole recipient list with the
/// body's accounts — the viewer dropped automatically, a repeat folded to
/// one, the form spelling accepted — and answers the rewritten entity. An
/// id that names no account is the 404 and stores nothing; a body without
/// `recipients` is the extractor's 422.
#[tokio::test]
async fn pleroma_update_replaces_the_recipient_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = actor(&pool, "alice", BOTH).await;
    let (bob, _) = actor(&pool, "bob", BOTH).await;
    let (carol, _) = actor(&pool, "carol", BOTH).await;
    let app = common::test_app(pool.clone());
    let post = dm(&pool, &alice, &[&bob, &carol], "@bob @carol psst").await;
    let row_id = participation_for(&pool, &alice, &post).await;
    let mine = Uuid::parse_str(&row_id).expect("fixture id");

    let rewritten = request("PATCH", &format!("/api/v1/pleroma/conversations/{row_id}"))
        .bearer(&alice_token)
        .json(&json!({
            "recipients": [alice.id.to_string(), carol.id.to_string(), carol.id.to_string()],
        }))
        .send(&app)
        .await;
    common::assert_answer(&rewritten, StatusCode::OK, "the rewrite");
    let entity = rewritten.json_value();
    assert_eq!(
        entity["accounts"]
            .as_array()
            .expect("accounts")
            .iter()
            .map(|account| account["id"].as_str().expect("an id"))
            .collect::<Vec<_>>(),
        vec![carol.id.to_string()],
        "the viewer is excluded, the repeat folded, the old roster gone"
    );
    assert_eq!(
        conversation::recipients(&pool, mine)
            .await
            .expect("the list reads"),
        vec![carol.id],
        "and it is what the row now stores"
    );

    let form = request("PATCH", &format!("/api/v1/pleroma/conversations/{row_id}"))
        .bearer(&alice_token)
        .form(&[("recipients[]", bob.id.to_string().as_str())])
        .send(&app)
        .await;
    assert_eq!(
        form.json_value()["accounts"][0]["id"],
        bob.id.to_string(),
        "the form spelling rewrites the same way"
    );

    let unknown = request("PATCH", &format!("/api/v1/pleroma/conversations/{row_id}"))
        .bearer(&alice_token)
        .json(&json!({"recipients": [UNKNOWN_ID]}))
        .send(&app)
        .await;
    common::assert_all_refused(
        &[unknown],
        StatusCode::NOT_FOUND,
        UNKNOWN,
        "an unknown recipient",
    );
    assert_eq!(
        conversation::recipients(&pool, mine)
            .await
            .expect("the list reads"),
        vec![bob.id],
        "and the refused rewrite stored nothing"
    );

    let missing = request("PATCH", &format!("/api/v1/pleroma/conversations/{row_id}"))
        .bearer(&alice_token)
        .json(&json!({}))
        .send(&app)
        .await;
    common::assert_answer(
        &missing,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a body without recipients",
    );
}

/// `POST /api/v1/pleroma/conversations/read` flips every unread
/// participation of the caller at once and answers exactly the rows that
/// changed, newest first; a second sweep marks nothing, and the other
/// side of each thread keeps its own unread flag.
#[tokio::test]
async fn pleroma_read_all_marks_the_unread_side_of_every_thread() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = actor(&pool, "alice", BOTH).await;
    let (bob, bob_token) = actor(&pool, "bob", BOTH).await;
    let app = common::test_app(pool.clone());
    let first = dm(&pool, &alice, &[&bob], "@bob first").await;
    let second = dm(&pool, &alice, &[&bob], "@bob second").await;
    let own = dm(&pool, &bob, &[&alice], "@alice mine").await;
    let first_id = participation_for(&pool, &bob, &first).await;
    let second_id = participation_for(&pool, &bob, &second).await;

    let swept = request("POST", "/api/v1/pleroma/conversations/read")
        .bearer(&bob_token)
        .send(&app)
        .await;
    common::assert_answer(&swept, StatusCode::OK, "the sweep");
    assert_eq!(
        common::ids_of(&swept.json_value()),
        vec![second_id, first_id],
        "exactly bob's two unread rows, newest first, his read own-post out"
    );
    assert!(
        swept
            .json_value()
            .as_array()
            .expect("an answer")
            .iter()
            .all(|entity| entity["unread"] == json!(false)),
        "every answered entity is read now"
    );

    let again = request("POST", "/api/v1/pleroma/conversations/read")
        .bearer(&bob_token)
        .send(&app)
        .await;
    assert_eq!(
        again.json_value(),
        json!([]),
        "a repeat flips nothing and answers nothing"
    );
    let for_alice = request("POST", "/api/v1/pleroma/conversations/read")
        .bearer(&alice_token)
        .send(&app)
        .await;
    assert_eq!(
        common::ids_of(&for_alice.json_value()),
        vec![participation_for(&pool, &alice, &own).await],
        "alice's only unread side is the thread bob posted into (her own \
         `first`/`second` posts read); bob's sweep touched none of her rows"
    );
}

/// A participation somebody else owns is not there for the asking — on
/// every one of the five id-keyed routes, with the wording of its own
/// controller; a stranger's listing never saw the thread at all.
#[tokio::test]
async fn foreign_participations_are_invisible_on_every_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = actor(&pool, "alice", BOTH).await;
    let (bob, _) = actor(&pool, "bob", BOTH).await;
    let (_, carol_token) = actor(&pool, "carol", BOTH).await;
    let app = common::test_app(pool.clone());
    let post = dm(&pool, &alice, &[&bob], "@bob psst").await;
    let bob_side = participation_for(&pool, &bob, &post).await;

    let v1_misses = [
        request("POST", &format!("/api/v1/conversations/{bob_side}/read"))
            .bearer(&carol_token)
            .send(&app)
            .await,
        request("DELETE", &format!("/api/v1/conversations/{bob_side}"))
            .bearer(&carol_token)
            .send(&app)
            .await,
    ];
    common::assert_all_refused(
        &v1_misses,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the v1 misses",
    );
    let pleroma_misses = [
        pleroma_get(&app, &carol_token, &format!("/{bob_side}")).await,
        pleroma_get(&app, &carol_token, &format!("/{bob_side}/statuses")).await,
        request(
            "PATCH",
            &format!("/api/v1/pleroma/conversations/{bob_side}"),
        )
        .bearer(&carol_token)
        .json(&json!({"recipients": []}))
        .send(&app)
        .await,
    ];
    common::assert_all_refused(
        &pleroma_misses,
        StatusCode::NOT_FOUND,
        UNKNOWN,
        "the pleroma misses",
    );
    assert!(
        list(&app, &carol_token, "")
            .await
            .json_value()
            .as_array()
            .expect("an answer")
            .is_empty(),
        "and the stranger's listing never saw the thread"
    );
}

/// Every route demands a user token — no token is the 401, an app-only
/// credential the 403 — and past that the three reads want
/// `read:statuses` while the four writes want `write:conversations`, each
/// refusal naming its missing scope.
#[tokio::test]
async fn every_route_demands_its_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = actor(&pool, "alice", BOTH).await;
    let (bob, _) = actor(&pool, "bob", BOTH).await;
    let app = common::test_app(pool.clone());
    let post = dm(&pool, &alice, &[&bob], "@bob psst").await;
    let mine = participation_for(&pool, &bob, &post).await;
    let (_, read_token) = actor(&pool, "readie", READ).await;
    let (_, write_token) = actor(&pool, "writey", WRITE).await;
    let app_only = common::create_app_only_token(&pool, BOTH).await;

    let routes: [(&str, String); 7] = [
        ("GET", "/api/v1/conversations".to_owned()),
        ("POST", format!("/api/v1/conversations/{mine}/read")),
        ("DELETE", format!("/api/v1/conversations/{mine}")),
        ("GET", format!("/api/v1/pleroma/conversations/{mine}")),
        (
            "GET",
            format!("/api/v1/pleroma/conversations/{mine}/statuses"),
        ),
        ("PATCH", format!("/api/v1/pleroma/conversations/{mine}")),
        ("POST", "/api/v1/pleroma/conversations/read".to_owned()),
    ];
    let mut no_tokens = Vec::new();
    let mut app_tokens = Vec::new();
    for (verb, path) in &routes {
        no_tokens.push(request(verb, path).send(&app).await);
        app_tokens.push(request(verb, path).bearer(&app_only).send(&app).await);
    }
    common::assert_all_refused(
        &no_tokens,
        StatusCode::UNAUTHORIZED,
        NO_TOKEN,
        "the tokenless asks",
    );
    common::assert_all_refused(
        &app_tokens,
        StatusCode::FORBIDDEN,
        NO_USER,
        "the app-only asks",
    );

    let wrong_for_reads = vec![
        list(&app, &write_token, "").await,
        pleroma_get(&app, &write_token, &format!("/{mine}")).await,
        pleroma_get(&app, &write_token, &format!("/{mine}/statuses")).await,
    ];
    common::assert_all_refused(
        &wrong_for_reads,
        StatusCode::FORBIDDEN,
        "Insufficient permissions: read:statuses.",
        "the write token on the reads",
    );

    let wrong_for_writes = vec![
        request("POST", &format!("/api/v1/conversations/{mine}/read"))
            .bearer(&read_token)
            .send(&app)
            .await,
        request("DELETE", &format!("/api/v1/conversations/{mine}"))
            .bearer(&read_token)
            .send(&app)
            .await,
        request("PATCH", &format!("/api/v1/pleroma/conversations/{mine}"))
            .bearer(&read_token)
            .json(&json!({"recipients": []}))
            .send(&app)
            .await,
        request("POST", "/api/v1/pleroma/conversations/read")
            .bearer(&read_token)
            .send(&app)
            .await,
    ];
    common::assert_all_refused(
        &wrong_for_writes,
        StatusCode::FORBIDDEN,
        "Insufficient permissions: write:conversations.",
        "the read token on the writes",
    );
}
