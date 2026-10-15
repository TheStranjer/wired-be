//! Integration tests for `GET /api/v1/follow_requests` and the two answer
//! routes `POST /api/v1/follow_requests/{id}/authorize` and `…/reject`:
//! the pending queue as `Account` entities newest request first, keyset
//! pagination whose cursors are the `follows` row ids with Pleroma's
//! `Link` header over them (and no header on an empty page), the
//! account-list page sizes (default 40, cap 80); authorizing flips both
//! denormalised counters exactly once and answers the caller's
//! `Relationship` with `followed_by: true`; rejecting deletes the row,
//! moves nothing, and answers `followed_by: false`; every miss — an
//! unknown, malformed or already-answered request, and another account's
//! queue — is the same 404; a malformed cursor is the page's 400; and
//! `AuthUser` in front of everything with `read:follows` for the listing
//! and `write:follows | follow` for the answers.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, Follow, FollowState},
        user::{Flag, User, get_by_id},
    },
};

use crate::common;

/// The instance origin the fixtures live on (`Config::for_tests`).
const BASE: &str = "https://test.example";
/// The queue listing's path, as the `Link` header links back to it.
const QUEUE: &str = "/api/v1/follow_requests";
/// The body every miss on the answer routes carries (decision 8).
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;
/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body for a listing token without `read:follows`.
const NO_READ_SCOPE: &str = r#"{"error":"Insufficient permissions: read:follows."}"#;
/// The 403 body for an answer token without either accepted scope;
/// `require_any` joins the alternatives with the `|` operator.
const NO_WRITE_SCOPE: &str = r#"{"error":"Insufficient permissions: write:follows | follow."}"#;
/// The authorize route's action segment.
const AUTHORIZE: &str = "authorize";
/// The reject route's action segment.
const REJECT: &str = "reject";

/// One fixture request: the requester's id as the entity shows it, next
/// to the id of the `follows` row the queue keys on.
type Request = (String, Uuid);

/// `GET /api/v1/follow_requests{query}` as `token` (`None` anonymous).
async fn queue(app: &Router, token: Option<&str>, query: &str) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("{QUEUE}{query}"));
    let request = match token {
        Some(found) => request.bearer(found),
        None => request,
    };
    request.send(app).await
}

/// `POST /api/v1/follow_requests/{requester}/{action}` as `token`.
async fn answer(app: &Router, token: &str, requester: &Uuid, action: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("{QUEUE}/{requester}/{action}"))
        .bearer(token)
        .send(app)
        .await
}

/// [`answer`] with the requester path segment left exactly as given (the
/// miss tests feed garbage there).
async fn raw_answer(
    app: &Router,
    token: &str,
    requester: &str,
    action: &str,
) -> common::TestResponse {
    common::TestRequest::post(&format!("{QUEUE}/{requester}/{action}"))
        .bearer(token)
        .send(app)
        .await
}

/// Inserts a local [`User`] named `nickname` whose account is locked, so
/// every follow of it waits in the queue for an answer.
async fn locked_user(pool: &PgPool, nickname: &str) -> User {
    common::create_local_user_with(pool, nickname, |mut new| {
        new.locked = Flag::new(true);
        new
    })
    .await
}

/// Records `follower`'s follow of the locked `target` and asserts it
/// waits — the fixture every test below answers or lists.
async fn requested(pool: &PgPool, follower: &User, target: &User) -> Follow {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the fixture request records");
    assert_eq!(
        row.state,
        FollowState::Pending,
        "a locked account answers its own requests"
    );
    row
}

/// Records `follower`'s request and grants it through [`follow::accept`],
/// the way an authorization (or a remote `Accept`) would: the row is
/// accepted and joins the follower list, not the queue.
async fn granted(pool: &PgPool, follower: &User, target: &User) -> Follow {
    let row = requested(pool, follower, target).await;
    let accepted = follow::accept(pool, row.id).await.expect("granted");
    assert_eq!(accepted.state, FollowState::Accepted, "the row is granted");
    accepted
}

/// The pair of `user` behind the request row `row`.
fn pair(user: &User, row: &Follow) -> Request {
    (user.id.to_string(), row.id)
}

/// `requests` sorted newest follow row first — the order the queue answers
/// in. `UUIDv7` is only millisecond precise, so the expectation is sorted
/// from the stored ids, never from the order the fixtures were written in
/// (decision 4).
fn sorted_newest_first(requests: &[Request]) -> Vec<Request> {
    let mut sorted = requests.to_vec();
    sorted.sort_by_key(|(_, follow_id)| std::cmp::Reverse(*follow_id));
    sorted
}

/// The requester ids of `requests`, newest first.
fn wanted_requesters(requests: &[Request]) -> Vec<String> {
    sorted_newest_first(requests)
        .into_iter()
        .map(|(user, _)| user)
        .collect()
}

/// The `follows` row ids of `requests`, newest first — the cursors the
/// `Link` header and the walking requests must carry.
fn wanted_follows(requests: &[Request]) -> Vec<Uuid> {
    sorted_newest_first(requests)
        .into_iter()
        .map(|(_, follow_id)| follow_id)
        .collect()
}

/// The account ids of every entity of a listing response, in response
/// order.
fn listed_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the queue is an array")
        .iter()
        .map(|account| account["id"].as_str().expect("an account id").to_owned())
        .collect()
}

/// The entity of account `id` in a listing body, however deep in the page
/// the id's millisecond landed it.
fn listed_entry<'a>(body: &'a Value, id: &Uuid) -> &'a Value {
    body.as_array()
        .expect("the queue is an array")
        .iter()
        .find(|entry| entry["id"] == id.to_string())
        .unwrap_or_else(|| panic!("account {id} is on the page"))
}

/// The `Link` header Pleroma's `add_link_headers` writes for a page of
/// the queue: `next` walks to the oldest request row, `prev` back to the
/// newest.
fn expected_link(oldest: &Uuid, newest: &Uuid) -> String {
    format!(
        "<{BASE}{QUEUE}?max_id={oldest}>; rel=\"next\", \
         <{BASE}{QUEUE}?min_id={newest}>; rel=\"prev\""
    )
}

/// The full relationship JSON for `target` with every flag at the
/// documented default, overwritten by `computed` — the expectation shape
/// `tests/relationships.rs` established for the entity.
fn expected_relationship(target: &Uuid, computed: &Value) -> Value {
    let mut value = json!({
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
    });
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

/// The row of user `id` as it stands now, for reading counters back.
async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
    get_by_id(pool, id).await.unwrap().expect("the user exists")
}

/// The queue answers the accounts waiting on the caller — full `Account`
/// entities, newest request first, local and remote alike — while accepted
/// followers, the caller's own outgoing requests and every other account's
/// queue stay out; an empty queue is the empty array with no `Link` header,
/// and the entries carry Pleroma's empty embedded relationship.
#[tokio::test]
async fn the_queue_lists_pending_requests_newest_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = locked_user(&pool, "carol").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let erin = locked_user(&pool, "erin").await;

    let waiting = [
        pair(&alice, &requested(&pool, &alice, &carol).await),
        pair(&mallory, &requested(&pool, &mallory, &carol).await),
    ];
    // bob's answered request is a follower, not a request …
    let _follower: Follow = granted(&pool, &bob, &carol).await;
    // … and carol's own request to erin is erin's to answer.
    let _outgoing: Follow = requested(&pool, &carol, &erin).await;

    let carol_token = common::create_app_and_token(&pool, &carol, "read:follows").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let dave_token = common::create_app_and_token(&pool, &dave, "read:follows").await;
    let app = common::test_app(pool);

    let listed = queue(&app, Some(&carol_token), "").await;
    assert_eq!(listed.status, StatusCode::OK, "the owner sees the queue");
    assert_eq!(
        listed.header("content-type"),
        Some("application/json"),
        "the entities are JSON"
    );
    let body = listed.json_value();
    assert_eq!(
        listed_ids(&body),
        wanted_requesters(&waiting),
        "the two waiting requests, newest row first; the accepted follower, \
         the outgoing request and the other accounts' rows stay out"
    );
    let alice_entry = listed_entry(&body, &alice.id);
    assert_eq!(
        alice_entry["acct"], "alice",
        "the local requester renders as a full account with its handle"
    );
    let mallory_entry = listed_entry(&body, &mallory.id);
    assert_eq!(
        mallory_entry["acct"], "mallory@remote.example",
        "the remote mirror keeps what federation stored"
    );
    assert_eq!(mallory_entry["uri"], mallory.ap_id, "and its actor id");
    for entry in body.as_array().expect("the array") {
        assert_eq!(
            entry["pleroma"]["relationship"],
            json!({}),
            "Pleroma's queue index embeds no relationship"
        );
    }

    let empty = queue(&app, Some(&dave_token), "").await;
    assert_eq!(empty.status, StatusCode::OK, "an empty queue is a 200");
    assert_eq!(
        empty.json_value(),
        json!([]),
        "nobody waits on dave — every account's queue is its own"
    );
    assert_eq!(
        empty.header("link"),
        None,
        "an empty page sends no Link header"
    );
}

/// The page walks by `follows` row id: the `Link` header's cursors, the
/// `max_id` continuation, the `min_id` page (answered newest-first again),
/// and the empty ends of the walk, which carry no header.
#[tokio::test]
async fn the_queue_pages_key_on_follow_row_ids_and_link() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = locked_user(&pool, "carol").await;
    let mut waiting = Vec::new();
    for n in 0..5 {
        let follower = common::create_local_user(&pool, &format!("follower{n}")).await;
        let row = requested(&pool, &follower, &carol).await;
        waiting.push(pair(&follower, &row));
    }
    let token = common::create_app_and_token(&pool, &carol, "read:follows").await;
    let app = common::test_app(pool);

    let wanted_users = wanted_requesters(&waiting);
    let wanted_rows = wanted_follows(&waiting);

    let first = queue(&app, Some(&token), "?limit=2").await;
    assert_eq!(
        listed_ids(&first.json_value()),
        wanted_users[..2],
        "two newest first"
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link(&wanted_rows[1], &wanted_rows[0]).as_str()),
        "the Link cursors are the page's follow row ids, not requester ids"
    );

    let older = queue(
        &app,
        Some(&token),
        &format!("?limit=2&max_id={}", wanted_rows[1]),
    )
    .await;
    assert_eq!(
        listed_ids(&older.json_value()),
        wanted_users[2..4],
        "max_id continues where the page ended"
    );

    let newer = queue(&app, Some(&token), &format!("?min_id={}", wanted_rows[3])).await;
    assert_eq!(
        listed_ids(&newer.json_value()),
        wanted_users[..3],
        "min_id answers the newest-first page just after the cursor"
    );

    let polled = queue(&app, Some(&token), &format!("?since_id={}", wanted_rows[0])).await;
    assert_eq!(
        polled.json_value(),
        json!([]),
        "nothing is newer than the head"
    );
    assert_eq!(
        polled.header("link"),
        None,
        "and an empty page sends no header"
    );

    let past = queue(&app, Some(&token), &format!("?max_id={}", wanted_rows[4])).await;
    assert_eq!(past.status, StatusCode::OK, "walking off the end is a 200");
    assert_eq!(past.json_value(), json!([]), "with the empty array");
    assert_eq!(past.header("link"), None, "and no header");
}

/// The queue sizes pages like Mastodon's account listings, not Pleroma's
/// global pair: forty rows by default, eighty at most.
#[tokio::test]
async fn the_default_page_is_forty_and_the_cap_is_eighty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = locked_user(&pool, "carol").await;
    for n in 0..85 {
        let follower = common::create_local_user(&pool, &format!("follower{n}")).await;
        let _waiting: Follow = requested(&pool, &follower, &carol).await;
    }
    let token = common::create_app_and_token(&pool, &carol, "read:follows").await;
    let app = common::test_app(pool);

    let default = queue(&app, Some(&token), "").await;
    assert_eq!(
        listed_ids(&default.json_value()).len(),
        40,
        "the account-list default, not Pleroma's 20"
    );
    let greedy = queue(&app, Some(&token), "?limit=100").await;
    assert_eq!(
        listed_ids(&greedy.json_value()).len(),
        80,
        "and the account-list cap, never more"
    );
    let between = queue(&app, Some(&token), "?limit=60").await;
    assert_eq!(
        listed_ids(&between.json_value()).len(),
        60,
        "a page above the global cap but under 80 goes through"
    );
}

/// Authorizing accepts the requester's row, steps both counters exactly
/// once, answers the caller's relationship with `followed_by: true`, and
/// empties the queue — and the answered request is then a 404, not an
/// idempotent repeat.
#[tokio::test]
async fn authorize_grants_flips_counters_and_answers_the_relationship() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = locked_user(&pool, "carol").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let request = requested(&pool, &alice, &carol).await;
    let token = common::create_app_and_token(&pool, &carol, "write:follows").await;
    // The queue check below needs a reading grant: a `write:follows` token
    // is exactly what the scope test proves cannot list.
    let read_token = common::create_app_and_token(&pool, &carol, "read:follows").await;
    let app = common::test_app(pool.clone());

    let response = answer(&app, &token, &alice.id, AUTHORIZE).await;
    assert_eq!(response.status, StatusCode::OK, "the request is granted");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the relationship is JSON"
    );
    assert_eq!(
        response.json_value(),
        expected_relationship(&alice.id, &json!({"followed_by": true})),
        "the caller's view of the requester: followed_by, nothing else"
    );

    let stored = follow::get(&pool, alice.id, carol.id)
        .await
        .unwrap()
        .expect("the relationship still exists");
    assert_eq!(
        stored.state,
        FollowState::Accepted,
        "the row became accepted"
    );
    assert_eq!(stored.id, request.id, "the same row, not a new one");
    assert_eq!(
        fresh_user(&pool, carol.id).await.followers_count,
        1,
        "the followee gained a follower"
    );
    assert_eq!(
        fresh_user(&pool, alice.id).await.following_count,
        1,
        "the requester gained a follow"
    );

    let empty = queue(&app, Some(&read_token), "").await;
    assert_eq!(
        empty.json_value(),
        json!([]),
        "an answered request leaves the queue"
    );

    let twice = answer(&app, &token, &alice.id, AUTHORIZE).await;
    assert_eq!(
        twice.status,
        StatusCode::NOT_FOUND,
        "nothing left to answer"
    );
    assert_eq!(twice.text(), NOT_FOUND, "decision 8's 404 body");
    assert_eq!(
        fresh_user(&pool, carol.id).await.followers_count,
        1,
        "and the refused repeat moved no counters"
    );
}

/// Rejecting deletes the row, moves no counters (a request never counted),
/// answers the caller's default relationship with `followed_by: false` —
/// through a `follow`-scoped token, the second accepted scope — and leaves
/// nothing behind for a repeat.
#[tokio::test]
async fn reject_removes_the_row_and_answers_the_relationship() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = locked_user(&pool, "carol").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let _waiting: Follow = requested(&pool, &alice, &carol).await;
    let token = common::create_app_and_token(&pool, &carol, "follow").await;
    let app = common::test_app(pool.clone());

    let response = answer(&app, &token, &alice.id, REJECT).await;
    assert_eq!(response.status, StatusCode::OK, "the request is refused");
    assert_eq!(
        response.json_value(),
        expected_relationship(&alice.id, &json!({"followed_by": false})),
        "with nothing recorded after it, every flag sits at its default"
    );

    assert!(
        follow::get(&pool, alice.id, carol.id)
            .await
            .unwrap()
            .is_none(),
        "the row is removed, not stored as rejected"
    );
    assert_eq!(
        fresh_user(&pool, carol.id).await.followers_count,
        0,
        "a refused request never counted, so nothing moved"
    );
    assert_eq!(
        fresh_user(&pool, alice.id).await.following_count,
        0,
        "on either side"
    );

    let twice = answer(&app, &token, &alice.id, REJECT).await;
    assert_eq!(
        twice.status,
        StatusCode::NOT_FOUND,
        "nothing left to refuse"
    );
    assert_eq!(twice.text(), NOT_FOUND, "decision 8's 404 body");
}

/// The queue and both answers are the target's alone: another account's
/// token lists nothing and cannot answer a request that is not its own —
/// every miss is the same 404.
#[tokio::test]
async fn only_the_target_user_sees_and_answers_requests() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = locked_user(&pool, "carol").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let _waiting: Follow = requested(&pool, &alice, &carol).await;
    let carol_token = common::create_app_and_token(&pool, &carol, "read:follows").await;
    let dave_token = common::create_app_and_token(&pool, &dave, "read:follows").await;
    let dave_write = common::create_app_and_token(&pool, &dave, "write:follows").await;
    let app = common::test_app(pool.clone());

    let seen = queue(&app, Some(&carol_token), "").await;
    assert_eq!(
        listed_ids(&seen.json_value()),
        vec![alice.id.to_string()],
        "carol sees alice's request"
    );
    let unseen = queue(&app, Some(&dave_token), "").await;
    assert_eq!(
        unseen.json_value(),
        json!([]),
        "dave sees no one else's queue"
    );

    for action in [AUTHORIZE, REJECT] {
        let stolen = answer(&app, &dave_write, &alice.id, action).await;
        assert_eq!(
            stolen.status,
            StatusCode::NOT_FOUND,
            "dave cannot answer carol's request, {action}"
        );
        assert_eq!(stolen.text(), NOT_FOUND, "the same 404, {action}");
    }
    assert_eq!(
        follow::get(&pool, alice.id, carol.id)
            .await
            .unwrap()
            .expect("the request")
            .state,
        FollowState::Pending,
        "and the refused answers changed nothing"
    );
}

/// Every miss on the answer routes is one 404: an id that is not uuid
/// text, an id that names no user, and an accepted incoming follow —
/// granted by answering its request — which is a follower, not a request
/// to answer.
#[tokio::test]
async fn every_answer_miss_is_the_same_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = locked_user(&pool, "carol").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _follower: Follow = granted(&pool, &bob, &carol).await;
    let token = common::create_app_and_token(&pool, &carol, "write:follows").await;
    let missing = Uuid::now_v7().to_string();
    let app = common::test_app(pool.clone());

    for requester in [missing.as_str(), "not-an-id", &bob.id.to_string()] {
        for action in [AUTHORIZE, REJECT] {
            let response = raw_answer(&app, &token, requester, action).await;
            assert_eq!(
                response.status,
                StatusCode::NOT_FOUND,
                "`{requester}` offers no request to answer, {action}"
            );
            assert_eq!(response.text(), NOT_FOUND, "decision 8's 404 body");
        }
    }
    assert_eq!(
        follow::get(&pool, bob.id, carol.id)
            .await
            .unwrap()
            .expect("the accepted relationship")
            .state,
        FollowState::Accepted,
        "an accepted follower is not a request to (re-)answer"
    );
}

/// All three routes demand a user token: no token is the 401 "Invalid
/// credentials", an invalid one the 401 "The access token is invalid", and
/// an app-only credential the 403 — before any scope or id is ever looked
/// at.
#[tokio::test]
async fn every_route_requires_a_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app_only = common::create_app_only_token(&pool, "write:follows").await;
    let app = common::test_app(pool);
    let requester = alice.id;
    let answer_path = format!("{QUEUE}/{requester}/{AUTHORIZE}");

    let anonymous = queue(&app, None, "").await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "no token cannot list the queue"
    );
    assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");
    let anonymous_post = common::TestRequest::post(&answer_path).send(&app).await;
    assert_eq!(
        anonymous_post.status,
        StatusCode::UNAUTHORIZED,
        "no token cannot answer a request"
    );
    assert_eq!(anonymous_post.text(), NO_TOKEN, "the same 401 body");

    let invalid = queue(&app, Some("bogus-token"), "").await;
    assert_eq!(
        invalid.status,
        StatusCode::UNAUTHORIZED,
        "an invalid token is not treated as anonymous"
    );
    assert_eq!(invalid.text(), INVALID_TOKEN, "AuthUser's invalid body");
    let invalid_post = common::TestRequest::post(&answer_path)
        .bearer("bogus-token")
        .send(&app)
        .await;
    assert_eq!(
        invalid_post.status,
        StatusCode::UNAUTHORIZED,
        "and the answers demand it too"
    );
    assert_eq!(
        invalid_post.text(),
        INVALID_TOKEN,
        "AuthUser's invalid body"
    );

    let app_token = queue(&app, Some(&app_only), "").await;
    assert_eq!(
        app_token.status,
        StatusCode::FORBIDDEN,
        "an app-only token is not a user"
    );
    assert_eq!(app_token.text(), USER_REQUIRED, "the user-required body");
    let app_post = common::TestRequest::post(&answer_path)
        .bearer(&app_only)
        .send(&app)
        .await;
    assert_eq!(
        app_post.status,
        StatusCode::FORBIDDEN,
        "and the answers refuse it alike"
    );
    assert_eq!(app_post.text(), USER_REQUIRED, "the user-required body");
}

/// The scope gates: the listing takes anything covering `read:follows`,
/// the answers anything covering `write:follows` or `follow`; everything
/// else is Pleroma's insufficient-permissions 403, naming the missing
/// alternatives.
#[tokio::test]
async fn the_routes_gate_on_the_follow_scopes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = locked_user(&pool, "carol").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let mut waiting = requested(&pool, &alice, &carol).await;
    let app = common::test_app(pool.clone());

    for scopes in ["read", "follow", "read:follows"] {
        let good = common::create_app_and_token(&pool, &carol, scopes).await;
        let listed = queue(&app, Some(&good), "").await;
        assert_eq!(listed.status, StatusCode::OK, "{scopes} reads the queue");
    }
    for scopes in ["read:accounts", "write:follows", "write"] {
        let bad = common::create_app_and_token(&pool, &carol, scopes).await;
        let denied = queue(&app, Some(&bad), "").await;
        assert_eq!(
            denied.status,
            StatusCode::FORBIDDEN,
            "{scopes} does not read the queue"
        );
        assert_eq!(
            denied.text(),
            NO_READ_SCOPE,
            "the denial names read:follows"
        );
    }

    for scopes in ["write", "follow", "write:follows"] {
        let good = common::create_app_and_token(&pool, &carol, scopes).await;
        let answered = answer(&app, &good, &alice.id, AUTHORIZE).await;
        assert_eq!(answered.status, StatusCode::OK, "{scopes} answers");
        // Take the answer back for the next round: rejecting the accepted
        // row removes it, and a fresh request waits again.
        let withdrawn = follow::reject(&pool, waiting.id).await.unwrap();
        assert_eq!(
            withdrawn.state,
            FollowState::Accepted,
            "the granted row is withdrawn for {scopes}"
        );
        waiting = requested(&pool, &alice, &carol).await;
    }
    for scopes in ["read:follows", "read", "write:accounts"] {
        let bad = common::create_app_and_token(&pool, &carol, scopes).await;
        let denied = answer(&app, &bad, &alice.id, REJECT).await;
        assert_eq!(
            denied.status,
            StatusCode::FORBIDDEN,
            "{scopes} does not answer requests"
        );
        assert_eq!(
            denied.text(),
            NO_WRITE_SCOPE,
            "the denial joins both scopes"
        );
    }
    assert_eq!(
        follow::get(&pool, alice.id, carol.id)
            .await
            .unwrap()
            .expect("the waiting request")
            .id,
        waiting.id,
        "the refused answers left the pending row alone"
    );
}

/// A malformed cursor is the page parser's 400 with its message, and auth
/// still runs first: a bad token alongside a bad cursor is the 401.
#[tokio::test]
async fn a_malformed_cursor_is_a_bad_request() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = locked_user(&pool, "carol").await;
    let token = common::create_app_and_token(&pool, &carol, "read:follows").await;
    let app = common::test_app(pool);

    for query in ["?max_id=nope", "?min_id=01890b9a-nope", "?since_id=x%2y"] {
        let response = queue(&app, Some(&token), query).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "`{query}` is refused before any listing"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Invalid id"}),
            "with the page parser's message"
        );
    }

    let denied = queue(&app, Some("bogus-token"), "?max_id=nope").await;
    assert_eq!(
        denied.status,
        StatusCode::UNAUTHORIZED,
        "the 401 comes before the 400"
    );
}
