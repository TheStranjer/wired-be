//! Integration tests for the blocks endpoints (the blocks ticket):
//! `POST /api/v1/accounts/{id}/block` severs whatever follows the pair held
//! between them, fixes the counters those follows fed and answers the
//! caller's relationship with `blocking: true` — and `blocked_by: true` from
//! the other account's view; `…/unblock` lifts the block idempotently
//! without rewinding the pair; blocking yourself is the 422 the ticket pins
//! and any other account miss the shared 404; `GET /api/v1/blocks` answers
//! the caller's blocked accounts as `Account` entities, newest block first,
//! keyset-paginated over the *block row* id with Pleroma's `Link` header and
//! Mastodon's 80-row cap, each entry embedding the viewer's relationship; a
//! blocked pair cannot follow over the model in either direction, with the
//! two exact messages; a block of a REMOTE account publishes its `Block`
//! (and the unblock its `Undo`) through the queued federation, a block of a
//! local account publishes nothing; and every route half of the auth and
//! scope pipelines is pinned: the listing with `read:blocks`, the two writes
//! with `write:blocks | follow`.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        block::{self, Block},
        follow::{self, FollowState},
        user::User,
    },
};

use crate::common;

/// The instance origin the fixtures live on (`Config::for_tests`).
const BASE: &str = "https://test.example";
/// The blocked-listing path, as the `Link` header links back to it.
const BLOCKS: &str = "/api/v1/blocks";
/// The 422 body `POST …/block` answers for the caller's own id: the wording
/// this endpoint's ticket pins.
const SELF_BLOCK: &str = r#"{"error":"Can not block yourself"}"#;
/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body of a write whose token grants neither accepted scope;
/// `require_any` joins the alternatives with the `|` operator.
const NO_WRITE_SCOPE: &str = r#"{"error":"Insufficient permissions: write:blocks | follow."}"#;
/// The 403 body of a listing whose token lacks `read:blocks`.
const NO_READ_SCOPE: &str = r#"{"error":"Insufficient permissions: read:blocks."}"#;
/// The 404 body every account route shares.
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;
/// The 400 body a malformed page cursor gets from the extractor.
const INVALID_ID: &str = r#"{"error":"Invalid id"}"#;
/// The two write actions, so the auth and scope loops drive both.
const ACTIONS: [&str; 2] = ["block", "unblock"];

/// One fixture block: the `blocks` row id the listing keys on, next to the
/// blocked account's id as its entity shows it.
type Listed = (Uuid, String);

/// The relationship JSON for `target` with nothing recorded about them:
/// every flag at the documented default — `showing_reblogs` true, `note`
/// empty, the rest false.
fn relationship_default(target: &Uuid) -> Value {
    json!({
        "id": target.to_string(),
        "following": false,
        "requested": false,
        "endorsed": false,
        "followed_by": false,
        "blocking": false,
        "blocked_by": false,
        "muting": false,
        "muting_notifications": false,
        "notifying": false,
        "showing_reblogs": true,
        "domain_blocking": false,
        "note": "",
        "subscribing": false,
    })
}

/// [`relationship_default`] with the flags in `computed` overwritten: the
/// tests name exactly what the write changed.
fn relationship(target: &Uuid, computed: &Value) -> Value {
    let mut value = relationship_default(target);
    let base = value.as_object_mut().expect("the default is an object");
    let overrides = computed.as_object().expect("the flags are a JSON object");
    for (key, flag) in overrides {
        let replaced = base.insert(key.clone(), flag.clone());
        assert!(
            replaced.is_some(),
            "`{key}` overrides a documented default, not a typo"
        );
    }
    value
}

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A request for one of the ticket's three routes: `GET` the listing, `POST`
/// either action path.
fn request(path: &str) -> common::TestRequest {
    if path == BLOCKS {
        common::TestRequest::get(path)
    } else {
        common::TestRequest::post(path)
    }
}

/// [`request`] with a bearer token.
fn authorized(path: &str, token: &str) -> common::TestRequest {
    request(path).bearer(token)
}

/// A `GET` of `path` with `token` (`None` anonymous).
async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    match token {
        Some(found) => common::TestRequest::get(path).bearer(found).send(app).await,
        None => common::TestRequest::get(path).send(app).await,
    }
}

/// The block action route of account `target`, `block` or `unblock`.
fn action_path(target: &Uuid, action: &str) -> String {
    format!("/api/v1/accounts/{target}/{action}")
}

/// The same route with `id` as the raw path segment, so a test can feed
/// something that is not an id.
fn raw_action_path(id: &str, action: &str) -> String {
    format!("/api/v1/accounts/{id}/{action}")
}

/// Blocks `target` as `token`, asserting the write answered the relationship
/// with `blocking: true`: the fixture every later test starts from.
async fn block_account(app: &Router, token: &str, target: &Uuid) {
    let response = authorized(&action_path(target, "block"), token)
        .send(app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the block is written");
    assert_eq!(
        response.json_value(),
        relationship(target, &json!({ "blocking": true })),
        "the answer states the stored fact"
    );
}

/// The `id` of every entity of a listing response, in response order.
fn listed_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the listing is an array")
        .iter()
        .map(|account| account["id"].as_str().expect("an account id").to_owned())
        .collect()
}

/// The blocked ids of `stored`, in order.
fn blocked_ids(stored: &[Listed]) -> Vec<String> {
    stored.iter().map(|(_, blocked)| blocked.clone()).collect()
}

/// The stored `(id, blocked_id)` rows of `blocker`'s blocks, newest first —
/// read from storage so same-millisecond `UUIDv7`s cannot make an expectation
/// depend on insert timing (decision 4).
async fn stored_blocks(pool: &PgPool, blocker: &User) -> Vec<Listed> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT id, blocked_id FROM blocks WHERE blocker_id = $1 ORDER BY id DESC",
    )
    .bind(blocker.id)
    .fetch_all(pool)
    .await
    .expect("the stored rows read");
    rows.into_iter()
        .map(|(id, blocked)| (id, blocked.to_string()))
        .collect()
}

/// The `Link` header Pleroma's `add_link_headers` writes for a page of the
/// blocked listing: `next` walks to the oldest block row, `prev` back to the
/// newest.
fn expected_link(oldest: &Uuid, newest: &Uuid) -> String {
    format!(
        "<{BASE}{BLOCKS}?max_id={oldest}>; rel=\"next\", \
         <{BASE}{BLOCKS}?min_id={newest}>; rel=\"prev\""
    )
}

/// The relationship `token`'s account holds to `target`, as the batch endpoint
/// answers it — how a test pins the flags from the other end of a pair.
async fn relationship_of(app: &Router, token: &str, target: &Uuid) -> Value {
    let response = get(
        app,
        Some(token),
        &format!("/api/v1/accounts/relationships?id%5B%5D={target}"),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "the batch answers");
    assert_eq!(
        response.json_value().as_array().expect("an array").len(),
        1,
        "one relationship for the one requested id"
    );
    response.json_value()[0].clone()
}

/// Records `follower`'s accepted follow of the open local `target` straight
/// through the model: the follow-actions routes are a later ticket, so the
/// fixture goes to the table (the way the endorsement tests do).
async fn follow_pair(pool: &PgPool, follower: &User, target: &User) {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(
        row.state,
        FollowState::Accepted,
        "an open local account answers on the spot"
    );
}

/// Whether the pair still has any follow row, in either direction.
async fn follows_between(pool: &PgPool, first: &User, second: &User) -> bool {
    follow::get(pool, first.id, second.id)
        .await
        .unwrap()
        .is_some()
        || follow::get(pool, second.id, first.id)
            .await
            .unwrap()
            .is_some()
}

/// Blocking a mutual follow severs it in both directions, steps both pairs of
/// counters down, answers the caller's relationship with `blocking: true` and
/// no follow flags, and lights `blocked_by` from the blocked account's own
/// view.
#[tokio::test]
async fn blocking_severs_the_pair_and_lights_both_flags() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read write").await;
    let (bob, bob_token) = fixture(&pool, "bob", "read").await;
    follow_pair(&pool, &alice, &bob).await;
    follow_pair(&pool, &bob, &alice).await;
    assert_eq!(
        (
            common::reload_user(&pool, alice.id).await.followers_count,
            common::reload_user(&pool, alice.id).await.following_count,
            common::reload_user(&pool, bob.id).await.followers_count,
            common::reload_user(&pool, bob.id).await.following_count
        ),
        (1, 1, 1, 1),
        "the mutual fixture counts both ways"
    );
    let app = common::test_app(pool.clone());

    let response = authorized(&action_path(&bob.id, "block"), &token)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the block is written");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "and JSON"
    );
    assert_eq!(
        response.json_value(),
        relationship(
            &bob.id,
            &json!({ "blocking": true, "following": false, "followed_by": false })
        ),
        "blocking: true, and the severed follows left with them"
    );

    assert!(
        !follows_between(&pool, &alice, &bob).await,
        "neither direction of the pair has a follow row left"
    );
    assert_eq!(
        (
            common::reload_user(&pool, alice.id).await.followers_count,
            common::reload_user(&pool, alice.id).await.following_count,
            common::reload_user(&pool, bob.id).await.followers_count,
            common::reload_user(&pool, bob.id).await.following_count
        ),
        (0, 0, 0, 0),
        "and every counter those two follows fed stepped down"
    );

    assert!(
        block::is_blocking(&pool, alice.id, bob.id).await.unwrap(),
        "the row is stored"
    );
    assert!(
        !block::is_blocking(&pool, bob.id, alice.id).await.unwrap(),
        "and the block never ran the other way"
    );
    assert_eq!(
        relationship_of(&app, &bob_token, &alice.id).await,
        relationship(&alice.id, &json!({ "blocked_by": true })),
        "bob's own relationship reads blocked_by, not blocking"
    );
}

/// Unblocking answers the relationship with both flags back at their default,
/// is idempotent, and does not restore what the block severed; the lifted
/// guard lets the pair start over.
#[tokio::test]
async fn unblocking_lifts_the_block_without_rewinding() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write").await;
    let bob = common::create_local_user(&pool, "bob").await;
    follow_pair(&pool, &alice, &bob).await;
    let app = common::test_app(pool.clone());

    block_account(&app, &token, &bob.id).await;
    assert!(
        !follows_between(&pool, &alice, &bob).await,
        "the block took the follow with it"
    );

    let lifted = authorized(&action_path(&bob.id, "unblock"), &token)
        .send(&app)
        .await;
    assert_eq!(lifted.status, StatusCode::OK, "the unblock answers");
    assert_eq!(
        lifted.json_value(),
        relationship(&bob.id, &json!({})),
        "every flag back at its default"
    );
    assert!(
        !block::is_blocking(&pool, alice.id, bob.id).await.unwrap(),
        "and the row is gone"
    );

    let again = authorized(&action_path(&bob.id, "unblock"), &token)
        .send(&app)
        .await;
    assert_eq!(again.status, StatusCode::OK, "lifting twice is no error");
    assert_eq!(
        again.json_value(),
        relationship(&bob.id, &json!({})),
        "and the same unchanged answer"
    );
    assert!(
        !follows_between(&pool, &alice, &bob).await,
        "unblocking does not bring the severed follow back"
    );

    follow_pair(&pool, &alice, &bob).await;
    assert_eq!(
        common::reload_user(&pool, bob.id).await.followers_count,
        1,
        "the pair may follow again, and that counts"
    );
}

/// A blocked follow attempt is refused from both ends with the two exact
/// messages this ticket pins, as a 403 on the model the follow endpoints run
/// through.
#[tokio::test]
async fn a_blocked_pair_cannot_follow() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let row = block::block(&pool, TEST_BASE_URL, &bob, &alice)
        .await
        .expect("bob blocks alice");
    assert_eq!(row.blocker_id, bob.id, "the fixture block is bob's");

    let err = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
        .await
        .expect_err("the target's block refuses alice's follow");
    assert_eq!(
        err.to_string(),
        "Could not follow user: bob is blocking you.",
        "the message names the account that holds the block"
    );
    let refused = follow::follow(&pool, TEST_BASE_URL, &bob, &alice)
        .await
        .expect_err("and the blocker's own block refuses his follow back");
    assert_eq!(
        refused.to_string(),
        "Could not follow user: You are blocking alice.",
        "the other direction is worded for the blocker"
    );
}

/// Blocking yourself is the 422 the ticket pins, and an unknown or malformed
/// id is the shared 404 on both writes.
#[tokio::test]
async fn a_self_block_is_422_and_any_other_miss_is_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:blocks").await;
    let app = common::test_app(pool.clone());

    let yourself = authorized(&action_path(&alice.id, "block"), &token)
        .send(&app)
        .await;
    assert_eq!(
        yourself.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the caller's own id is the 422"
    );
    assert_eq!(yourself.text(), SELF_BLOCK, "the ticket's wording");
    assert_eq!(
        stored_blocks(&pool, &alice).await.len(),
        0,
        "and the refusal stored nothing"
    );

    let unknown = Uuid::now_v7().to_string();
    for id in [unknown, "not-an-id".to_owned()] {
        for action in ACTIONS {
            let response = authorized(&raw_action_path(&id, action), &token)
                .send(&app)
                .await;
            assert_eq!(
                response.status,
                StatusCode::NOT_FOUND,
                "a `{action}` of `{id}` is the shared 404"
            );
            assert_eq!(response.text(), NOT_FOUND, "the 404 body");
        }
    }
}

/// `GET /api/v1/blocks` answers the caller's blocked accounts as `Account`
/// entities, newest block first, each embedding the viewer's relationship,
/// under a `Link` header keyed on the block row ids; an account with nothing
/// blocked gets the empty array and no header, and another account's blocks
/// are nobody else's list.
#[tokio::test]
async fn the_listing_answers_accounts_newest_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read write").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let app = common::test_app(pool.clone());

    let empty = get(&app, Some(&token), BLOCKS).await;
    assert_eq!(empty.status, StatusCode::OK, "an empty listing is a 200");
    assert_eq!(empty.json_value(), json!([]), "the empty array");
    assert_eq!(empty.header("link"), None, "with nothing to link from");

    for target in [&bob, &carol, &dave] {
        block_account(&app, &token, &target.id).await;
    }
    let stored = stored_blocks(&pool, &alice).await;
    assert_eq!(stored.len(), 3, "three fixture blocks");

    let listed = get(&app, Some(&token), BLOCKS).await;
    assert_eq!(
        listed_ids(&listed.json_value()),
        blocked_ids(&stored),
        "the blocked accounts, newest block first"
    );
    assert_eq!(
        listed.header("link"),
        Some(expected_link(&stored[2].0, &stored[0].0).as_str()),
        "the Link cursors are the block row ids, not the accounts"
    );
    let first = &listed.json_value()[0];
    assert_eq!(
        first["id"],
        json!(stored[0].1),
        "an entry is a rendered `Account`, keyed on the blocked account's id"
    );
    assert!(
        first["username"].is_string(),
        "with the account's own fields beside it"
    );
    assert_eq!(
        first["pleroma"]["relationship"]["blocking"],
        json!(true),
        "and each entry embeds the caller's relationship"
    );

    let (erin, erin_token) = fixture(&pool, "erin", "read").await;
    let theirs = get(&app, Some(&erin_token), BLOCKS).await;
    assert_eq!(
        theirs.json_value(),
        json!([]),
        "another account's listing holds none of alice's blocks"
    );
    assert_eq!(
        stored_blocks(&pool, &erin).await.len(),
        0,
        "and erin blocked nothing to list"
    );
}

/// The listing's page walk: `limit` at a time, `max_id` continuing where a
/// page ended, `min_id` answered newest-first again, the ends of the walk
/// carrying no header, and a malformed cursor the extractor's 400.
#[tokio::test]
async fn the_listing_pages_by_block_row_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read write").await;
    let app = common::test_app(pool.clone());
    for nickname in ["bob", "carol", "dave", "erin"] {
        let blocked = common::create_local_user(&pool, nickname).await;
        block_account(&app, &token, &blocked.id).await;
    }
    let stored = stored_blocks(&pool, &alice).await;

    let first = get(&app, Some(&token), &format!("{BLOCKS}?limit=2")).await;
    assert_eq!(
        listed_ids(&first.json_value()),
        vec![stored[0].1.clone(), stored[1].1.clone()],
        "two newest first"
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link(&stored[1].0, &stored[0].0).as_str()),
        "the cursors are the page's own ends"
    );

    let older = get(
        &app,
        Some(&token),
        &format!("{BLOCKS}?limit=2&max_id={}", stored[1].0),
    )
    .await;
    assert_eq!(
        listed_ids(&older.json_value()),
        vec![stored[2].1.clone(), stored[3].1.clone()],
        "max_id continues where the page ended"
    );

    let newer = get(
        &app,
        Some(&token),
        &format!("{BLOCKS}?min_id={}", stored[2].0),
    )
    .await;
    assert_eq!(
        listed_ids(&newer.json_value()),
        vec![stored[0].1.clone(), stored[1].1.clone()],
        "min_id answers the newest-first page just after the cursor"
    );

    let polled = get(
        &app,
        Some(&token),
        &format!("{BLOCKS}?since_id={}", stored[0].0),
    )
    .await;
    assert_eq!(
        polled.json_value(),
        json!([]),
        "nothing is newer than the head"
    );
    assert_eq!(
        polled.header("link"),
        None,
        "and an empty page links nowhere"
    );

    let past = get(
        &app,
        Some(&token),
        &format!("{BLOCKS}?max_id={}", stored[3].0),
    )
    .await;
    assert_eq!(past.status, StatusCode::OK, "walking off the end is a 200");
    assert_eq!(past.json_value(), json!([]), "with the empty array");
    assert_eq!(past.header("link"), None, "and no header");

    let malformed = get(&app, Some(&token), &format!("{BLOCKS}?max_id=nope")).await;
    assert_eq!(
        malformed.status,
        StatusCode::BAD_REQUEST,
        "a bad cursor is Pleroma's 400"
    );
    assert_eq!(malformed.text(), INVALID_ID);
}

/// The listing's cap is Mastodon's 80, not the global 40: a page of 41 blocks
/// comes back whole, while Pleroma's 20 stays the default when no `limit` is
/// asked for.
#[tokio::test]
async fn the_listing_pages_up_to_eighty_rows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mut seeded: Vec<Block> = Vec::new();
    for n in 0..41 {
        let blocked = common::create_local_user(&pool, &format!("blocked{n}")).await;
        seeded.push(
            block::block(&pool, TEST_BASE_URL, &alice, &blocked)
                .await
                .expect("the fixture block stores"),
        );
    }
    assert_eq!(seeded.len(), 41, "forty-one fixture blocks");
    let stored = stored_blocks(&pool, &alice).await;
    assert_eq!(stored.len(), 41, "and they are all on the books");
    let app = common::test_app(pool.clone());
    let token = common::create_app_and_token(&pool, &alice, "read:blocks").await;

    let default_page = get(&app, Some(&token), BLOCKS).await;
    assert_eq!(
        listed_ids(&default_page.json_value()).len(),
        20,
        "Pleroma's default page of twenty"
    );

    let big = get(&app, Some(&token), &format!("{BLOCKS}?limit=41")).await;
    assert_eq!(
        listed_ids(&big.json_value()).len(),
        41,
        "a page past the global 40 cap gets through: the listing's cap is 80"
    );
    assert_eq!(
        big.header("link"),
        Some(expected_link(&stored[40].0, &stored[0].0).as_str()),
        "and the whole page is one page, linked at both ends"
    );
}

/// All three routes demand a user token: no token is the 401, an invalid one
/// the 401, and an app-only credential the 403.
#[tokio::test]
async fn the_routes_require_a_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_local_user(&pool, "bob").await;
    let app_only = common::create_app_only_token(&pool, "read write follow").await;
    let app = common::test_app(pool);

    for path in [
        BLOCKS.to_owned(),
        action_path(&bob.id, "block"),
        action_path(&bob.id, "unblock"),
    ] {
        let anonymous = request(&path).send(&app).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "no token cannot use `{path}`"
        );
        assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");

        let invalid = authorized(&path, "bogus-token").send(&app).await;
        assert_eq!(
            invalid.status,
            StatusCode::UNAUTHORIZED,
            "an invalid token is not treated as anonymous (`{path}`)"
        );
        assert_eq!(invalid.text(), INVALID_TOKEN, "AuthUser's invalid body");

        let machine = authorized(&path, &app_only).send(&app).await;
        assert_eq!(
            machine.status,
            StatusCode::FORBIDDEN,
            "an app-only token is not a user (`{path}`)"
        );
        assert_eq!(machine.text(), USER_REQUIRED, "the user-required body");
    }
}

/// The listing's scope gate is exactly `read:blocks`: broad `read` and the
/// scope itself list; `follow` (the writes' alternative, not this read side),
/// `write` and an unrelated sub-scope answer the insufficient-permissions 403.
#[tokio::test]
async fn the_listing_requires_the_read_blocks_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;

    for scopes in ["read", "read:blocks"] {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let response = get(&app, Some(&token), BLOCKS).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "a `{scopes}` token can list"
        );
    }

    for scopes in ["follow", "write", "read:accounts", ""] {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let response = get(&app, Some(&token), BLOCKS).await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "a `{scopes}` token cannot list"
        );
        assert_eq!(response.text(), NO_READ_SCOPE, "the denial names the scope");
    }
}

/// Both writes' gate is `write:blocks | follow`: broad `write`, the scope
/// itself and the `follow` root all block and unblock; anything else is the
/// alternatives-joined 403, on both actions.
#[tokio::test]
async fn the_writes_take_write_blocks_or_follow() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;

    for (n, scopes) in ["write", "write:blocks", "follow"].iter().enumerate() {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let target = common::create_local_user(&pool, &format!("granted{n}")).await;
        let response = authorized(&action_path(&target.id, "block"), &token)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "a `{scopes}` token can block"
        );
        assert_eq!(
            response.json_value(),
            relationship(&target.id, &json!({ "blocking": true })),
            "and the answer states it"
        );
        let lifted = authorized(&action_path(&target.id, "unblock"), &token)
            .send(&app)
            .await;
        assert_eq!(
            lifted.status,
            StatusCode::OK,
            "a `{scopes}` token can unblock"
        );
    }

    for scopes in ["read", "read:blocks", "write:accounts"] {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        for action in ACTIONS {
            let response = authorized(&action_path(&bob.id, action), &token)
                .send(&app)
                .await;
            assert_eq!(
                response.status,
                StatusCode::FORBIDDEN,
                "a `{scopes}` token cannot {action}"
            );
            assert_eq!(response.text(), NO_WRITE_SCOPE, "the denial names both");
        }
    }

    assert!(
        stored_blocks(&pool, &alice).await.is_empty(),
        "the granted round trips left nothing behind"
    );
}

/// The arguments of the `federator.publish` jobs queued so far, in the
/// order they landed — what the block endpoints' federation hook writes.
async fn publish_jobs(pool: &PgPool) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>(
        "SELECT args FROM jobs WHERE kind = 'federator.publish' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("the publish jobs read")
    .into_iter()
    .map(|(args,)| args)
    .collect()
}

/// The stored `ap_id` of the pair's block row: the activity id the
/// federation must reuse, read straight off the table.
async fn stored_ap_id(pool: &PgPool, blocker: &User, target: &User) -> String {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT ap_id FROM blocks WHERE blocker_id = $1 AND blocked_id = $2",
    )
    .bind(blocker.id)
    .bind(target.id)
    .fetch_one(pool)
    .await
    .expect("the block row reads")
    .expect("a local blocker's row carries its activity id")
}

/// A block of a REMOTE account federates: the write queues one
/// `federator.publish` job carrying the `Block` under the id the row
/// stores, addressed to the blocked actor alone, and the unblock queues
/// the `Undo` of that same document. A block of a LOCAL account is
/// settled in the database and publishes nothing.
#[tokio::test]
async fn blocking_a_remote_account_publishes_the_block_and_its_undo() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write").await;
    let bob = common::create_remote_user(&pool, "bob", "peer.example").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let app = common::test_app(pool.clone());

    let blocked = authorized(&action_path(&bob.id, "block"), &token)
        .send(&app)
        .await;
    assert_eq!(blocked.status, StatusCode::OK, "the block is written");
    let jobs = publish_jobs(&pool).await;
    assert_eq!(jobs.len(), 1, "and published once");
    let activity = &jobs[0]["activity"];
    assert_eq!(activity["type"], "Block");
    assert_eq!(
        activity["id"],
        stored_ap_id(&pool, &alice, &bob).await,
        "the activity carries the id the row stored, not a freshly minted one"
    );
    assert_eq!(activity["actor"], alice.ap_id);
    assert_eq!(
        activity["to"][0], bob.ap_id,
        "the blocked actor is the only recipient"
    );
    let block_id = activity["id"].as_str().expect("a string id").to_owned();

    let local_block = authorized(&action_path(&carol.id, "block"), &token)
        .send(&app)
        .await;
    assert_eq!(local_block.status, StatusCode::OK, "local blocks work too");
    assert_eq!(
        publish_jobs(&pool).await.len(),
        1,
        "and a local pair publishes nothing"
    );

    let lifted = authorized(&action_path(&bob.id, "unblock"), &token)
        .send(&app)
        .await;
    assert_eq!(lifted.status, StatusCode::OK, "the unblock is written");
    let later = publish_jobs(&pool).await;
    assert_eq!(later.len(), 2, "the withdrawal is published");
    let undo = &later[1]["activity"];
    assert_eq!(undo["type"], "Undo");
    assert_eq!(undo["actor"], alice.ap_id);
    assert_eq!(undo["to"][0], bob.ap_id);
    assert_eq!(
        undo["object"]["id"], block_id,
        "the undo withdraws the very block the row stands for"
    );

    let local_lift = authorized(&action_path(&carol.id, "unblock"), &token)
        .send(&app)
        .await;
    assert_eq!(local_lift.status, StatusCode::OK);
    assert_eq!(
        publish_jobs(&pool).await.len(),
        2,
        "and lifting a local block publishes nothing either"
    );
}
