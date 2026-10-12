//! Integration tests for the Pleroma account extensions merged at
//! `/api/v1/pleroma`: `POST /api/v1/pleroma/accounts/{id}/subscribe` and
//! `…/unsubscribe` (both halves of the ticket's one column, with the
//! `notifying` / `subscribing` flags of the answered `Relationship` and the
//! `follows.notify` fact behind them, refused with the ticket's 422 wherever
//! the caller has no accepted follow of the target, the shared 404 for every
//! account miss), and `GET /api/v1/pleroma/outgoing_follow_requests` (the
//! caller's own pending asks as `Account` entities newest request first, with
//! keyset pagination whose cursors are the `follows` row ids under Pleroma's
//! global page sizes, and no `Link` header on an empty page); `AuthUser` in
//! front of all three, `follow | write:follows` for the two writes and
//! `read:follows` for the listing, and the page parser's 400 for a malformed
//! cursor.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::cmp::Reverse;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, Follow, FollowState},
        user::{Flag, User, get_by_id},
    },
};

/// The instance origin the fixtures live on (`Config::for_tests`).
const BASE: &str = "https://test.example";
/// The outgoing listing's path, as the `Link` header links back to it.
const OUTGOING: &str = "/api/v1/pleroma/outgoing_follow_requests";
/// The body every miss on the subscription routes carries (decision 8).
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;
/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body a subscription token without either accepted scope gets;
/// `require_any` joins the alternatives with the `|` operator, in the order
/// Pleroma's plug lists them.
const NO_WRITE_SCOPE: &str = r#"{"error":"Insufficient permissions: follow | write:follows."}"#;
/// The 403 body a listing token without `read:follows` gets.
const NO_READ_SCOPE: &str = r#"{"error":"Insufficient permissions: read:follows."}"#;
/// The 422 body both subscription routes answer without an accepted follow.
const NOT_FOLLOWING: &str = r#"{"error":"Could not subscribe: You are not following this user"}"#;

/// One fixture request: the requested account's id as the entity shows it,
/// next to the id of the `follows` row the page keys on.
type Ask = (String, Uuid);

/// The subscribe route for `target`.
fn subscribe_path(target: &Uuid) -> String {
    format!("/api/v1/pleroma/accounts/{target}/subscribe")
}

/// The unsubscribe route for `target`.
fn unsubscribe_path(target: &Uuid) -> String {
    format!("/api/v1/pleroma/accounts/{target}/unsubscribe")
}

/// `POST` one subscription route as `token` (the two actions differ only by
/// their path segment, so both run through here).
async fn subscription(
    app: &Router,
    token: &str,
    target: &Uuid,
    unsubscribe: bool,
) -> common::TestResponse {
    let path = if unsubscribe {
        unsubscribe_path(target)
    } else {
        subscribe_path(target)
    };
    common::TestRequest::post(&path)
        .bearer(token)
        .send(app)
        .await
}

/// [`subscription`] with the account segment left exactly as `target` spells
/// it (the miss tests feed garbage there).
async fn raw_subscription(
    app: &Router,
    token: &str,
    target: &str,
    action: &str,
) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/pleroma/accounts/{target}/{action}"))
        .bearer(token)
        .send(app)
        .await
}

/// `GET /api/v1/pleroma/outgoing_follow_requests{query}` as `token`
/// (`None` anonymous).
async fn outgoing(app: &Router, token: Option<&str>, query: &str) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("{OUTGOING}{query}"));
    let request = match token {
        Some(found) => request.bearer(found),
        None => request,
    };
    request.send(app).await
}

/// `GET /api/v1/accounts/relationships?id[]={target}` as `token`: the read
/// that proves which column the writes touched.
async fn relationship_batch(app: &Router, token: &str, target: &Uuid) -> Value {
    let response =
        common::TestRequest::get(&format!("/api/v1/accounts/relationships?id%5B%5D={target}"))
            .bearer(token)
            .send(app)
            .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the batch read answers the caller's relationship"
    );
    response
        .json_value()
        .as_array()
        .expect("the batch is an array")
        .first()
        .expect("one target was requested")
        .clone()
}

/// Inserts a local [`User`] named `nickname` whose account is locked, so
/// every follow of it waits for an answer.
async fn locked_user(pool: &PgPool, nickname: &str) -> User {
    common::create_local_user_with(pool, nickname, |mut new| {
        new.locked = Flag::new(true);
        new
    })
    .await
}

/// Inserts a local [`User`] named `nickname` that is deactivated: the id
/// every account route treats as absent.
async fn deactivated_user(pool: &PgPool, nickname: &str) -> User {
    common::create_local_user_with(pool, nickname, |mut new| {
        new.is_active = Flag::new(false);
        new
    })
    .await
}

/// Records `follower`'s follow of the unlocked `target` and asserts it was
/// granted — the accepted relationship the subscription routes require.
async fn following(pool: &PgPool, follower: &User, target: &User) -> Follow {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(
        row.state,
        FollowState::Accepted,
        "an open local account answers on the spot"
    );
    row
}

/// Records `follower`'s follow of the locked `target` and asserts it waits —
/// a request, which is not a subscription.
async fn asking(pool: &PgPool, follower: &User, target: &User) -> Follow {
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

/// The stored relationship of the pair, for reading the toggle back.
async fn stored(pool: &PgPool, follower: &User, target: &User) -> Follow {
    follow::get(pool, follower.id, target.id)
        .await
        .unwrap()
        .expect("the fixture relationship exists")
}

/// The row of user `id` as it stands now, for reading counters back.
async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
    get_by_id(pool, id).await.unwrap().expect("the user exists")
}

/// The pair of `user` behind the request row `row`.
fn ask(user: &User, row: &Follow) -> Ask {
    (user.id.to_string(), row.id)
}

/// `asks` sorted newest follow row first — the order the listing answers in.
/// `UUIDv7` is only millisecond precise, so the expectation is sorted from the
/// stored ids, never from the order the fixtures were written in (decision 4).
fn sorted_newest_first(asks: &[Ask]) -> Vec<Ask> {
    let mut sorted = asks.to_vec();
    sorted.sort_by_key(|(_, follow_id)| Reverse(*follow_id));
    sorted
}

/// The requested-account ids of `asks`, newest first.
fn wanted_targets(asks: &[Ask]) -> Vec<String> {
    sorted_newest_first(asks)
        .into_iter()
        .map(|(user, _)| user)
        .collect()
}

/// The `follows` row ids of `asks`, newest first — the cursors the `Link`
/// header and the walking requests must carry.
fn wanted_follows(asks: &[Ask]) -> Vec<Uuid> {
    sorted_newest_first(asks)
        .into_iter()
        .map(|(_, follow_id)| follow_id)
        .collect()
}

/// The account ids of every entity of a listing response, in response order.
fn listed_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the listing is an array")
        .iter()
        .map(|account| account["id"].as_str().expect("an account id").to_owned())
        .collect()
}

/// The entity of account `id` in a listing body, however deep in the page the
/// id's millisecond landed it.
fn listed_entry<'a>(body: &'a Value, id: &Uuid) -> &'a Value {
    body.as_array()
        .expect("the listing is an array")
        .iter()
        .find(|entry| entry["id"] == id.to_string())
        .unwrap_or_else(|| panic!("account {id} is on the page"))
}

/// The `Link` header Pleroma's `add_link_headers` writes for a page of the
/// outgoing list: `next` walks to the oldest request row, `prev` back to the
/// newest.
fn expected_link(oldest: &Uuid, newest: &Uuid) -> String {
    format!(
        "<{BASE}{OUTGOING}?max_id={oldest}>; rel=\"next\", \
         <{BASE}{OUTGOING}?min_id={newest}>; rel=\"prev\""
    )
}

/// The full relationship JSON for `target` with every flag at the documented
/// default, overwritten by `computed` — the expectation shape the relationship
/// tests established.
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

/// Subscribing needs a granted follow of the target: no relationship at all,
/// a request nobody answered, and the other direction of a relationship are
/// the same 422 with the ticket's wording, and none of them writes anything.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn subscribe_requires_an_accepted_follow(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = locked_user(&pool, "carol").await;
    let erin = locked_user(&pool, "erin").await;
    let _waiting: Follow = asking(&pool, &alice, &carol).await;
    // bob follows alice, which is bob's subscription to have, not alice's.
    let _incoming: Follow = following(&pool, &bob, &alice).await;
    let token = common::create_app_and_token(&pool, &alice, "follow").await;
    let app = common::test_app(pool.clone());

    // erin: a live account alice never asked to follow; carol: a request
    // nobody answered; bob: the other end of a granted follow.
    for target in [erin.id, carol.id, bob.id] {
        let refused = subscription(&app, &token, &target, false).await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "no accepted follow of {target} means no subscription"
        );
        assert_eq!(
            refused.header("content-type"),
            Some("application/json"),
            "errors are JSON"
        );
        assert_eq!(refused.text(), NOT_FOLLOWING, "the ticket's refusal body");
    }

    assert!(
        !stored(&pool, &alice, &carol).await.notify,
        "the pending row keeps its stored flag"
    );
    assert!(
        !stored(&pool, &bob, &alice).await.notify,
        "and so does the incoming relationship"
    );
    assert!(
        follow::get(&pool, alice.id, erin.id)
            .await
            .unwrap()
            .is_none(),
        "nothing was created for the account that was never asked about"
    );
    assert_eq!(
        stored(&pool, &alice, &carol).await.state,
        FollowState::Pending,
        "the refused write neither answered the request nor changed its row"
    );
    assert_eq!(
        follow::pending_count(&pool, carol.id).await.unwrap(),
        1,
        "the request is still in the queue it belongs to"
    );
}

/// A successful subscribe answers the caller's full `Relationship` with both
/// `notifying` and its `subscribing` alias `true`, stores the flag on the
/// follow row, leaves everything else about the relationship alone, and reads
/// back the same way from the Mastodon relationship batch; subscribing twice
/// is idempotent.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn subscribe_sets_both_flags_and_answers_the_relationship(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let granted = following(&pool, &alice, &bob).await;
    let token = common::create_app_and_token(&pool, &alice, "write:follows").await;
    let read_token = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let app = common::test_app(pool.clone());

    let response = subscription(&app, &token, &bob.id, false).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the subscription goes through"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the relationship is JSON"
    );
    assert_eq!(
        response.json_value(),
        expected_relationship(
            &bob.id,
            &json!({
                "following": true,
                "notifying": true,
                "subscribing": true,
            })
        ),
        "the caller's view of the target: following, and subscribed on both \
         aliases, with every other flag at its default"
    );

    let updated = stored(&pool, &alice, &bob).await;
    assert!(updated.notify, "the follows column holds the subscription");
    assert_eq!(updated.id, granted.id, "the same row, not a new one");
    assert_eq!(
        updated.state,
        FollowState::Accepted,
        "subscribing does not change the relationship's state"
    );
    assert!(updated.show_reblogs, "and leaves the reblog toggle alone");
    assert!(
        updated.updated_at >= updated.inserted_at,
        "the write refreshes the row's timestamp"
    );
    assert_eq!(
        (
            fresh_user(&pool, bob.id).await.followers_count,
            fresh_user(&pool, alice.id).await.following_count
        ),
        (1, 1),
        "subscribing moves no counters: the follow already counted"
    );
    assert_eq!(
        follow::pending_count(&pool, bob.id).await.unwrap(),
        0,
        "no request was created underneath"
    );

    let batch = relationship_batch(&app, &read_token, &bob.id).await;
    assert_eq!(batch["notifying"], true, "the batch read sees the flag");
    assert_eq!(batch["subscribing"], true, "under its alias too");

    let twice = subscription(&app, &token, &bob.id, false).await;
    assert_eq!(twice.status, StatusCode::OK, "subscribing twice is allowed");
    assert_eq!(
        twice.json_value(),
        expected_relationship(
            &bob.id,
            &json!({
                "following": true,
                "notifying": true,
                "subscribing": true,
            })
        ),
        "and answers the same relationship"
    );
}

/// Unsubscribing clears the same column and answers both flags `false`, the
/// caller stays following, and the write needs the same accepted follow the
/// subscribe route does (a relationship the caller never granted is the 422,
/// an id that names no account is the shared 404).
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn unsubscribe_clears_the_flag_again(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = locked_user(&pool, "carol").await;
    let _granted: Follow = following(&pool, &alice, &bob).await;
    let _waiting: Follow = asking(&pool, &alice, &carol).await;
    let token = common::create_app_and_token(&pool, &alice, "follow").await;
    let read_token = common::create_app_and_token(&pool, &alice, "read").await;
    let app = common::test_app(pool.clone());

    let subscribed = subscription(&app, &token, &bob.id, false).await;
    assert_eq!(subscribed.status, StatusCode::OK, "the fixture subscribes");
    assert!(
        stored(&pool, &alice, &bob).await.notify,
        "and the flag is stored"
    );

    let response = subscription(&app, &token, &bob.id, true).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the un-subscription goes through"
    );
    assert_eq!(
        response.json_value(),
        expected_relationship(
            &bob.id,
            &json!({
                "following": true,
                "notifying": false,
                "subscribing": false,
            })
        ),
        "both flags are off again, while the follow itself stands"
    );
    assert!(
        !stored(&pool, &alice, &bob).await.notify,
        "the follows column holds the answer"
    );
    assert_eq!(
        stored(&pool, &alice, &bob).await.state,
        FollowState::Accepted,
        "and the relationship is still granted"
    );

    let batch = relationship_batch(&app, &read_token, &bob.id).await;
    assert_eq!(batch["notifying"], false, "the batch read agrees");
    assert_eq!(batch["subscribing"], false, "on both aliases");

    let twice = subscription(&app, &token, &bob.id, true).await;
    assert_eq!(
        twice.status,
        StatusCode::OK,
        "unsubscribing twice is allowed"
    );
    assert_eq!(
        twice.json_value()["subscribing"],
        false,
        "and answers the stored flag again"
    );

    let refused = subscription(&app, &token, &carol.id, true).await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a request nobody answered cannot be un-subscribed either"
    );
    assert_eq!(refused.text(), NOT_FOLLOWING, "the same refusal body");
    assert!(
        !stored(&pool, &alice, &carol).await.notify,
        "and nothing was written"
    );
}

/// The outgoing listing answers the accounts the caller asked to follow and
/// that have not answered — full `Account` entities, newest request first,
/// local and remote alike — while granted follows, other accounts' queues and
/// the caller's incoming requests stay out; an empty list is the empty array
/// with no `Link` header, and the entries carry Pleroma's empty embedded
/// relationship.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_outgoing_list_answers_the_callers_own_asks(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let carol = locked_user(&pool, "carol").await;
    let dave = locked_user(&pool, "dave").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let erin = locked_user(&pool, "erin").await;

    let asks = [
        ask(&carol, &asking(&pool, &alice, &carol).await),
        ask(&mallory, &asking(&pool, &alice, &mallory).await),
    ];
    // bob is granted, so he is a follow and not an ask …
    let bob = common::create_local_user(&pool, "bob").await;
    let _granted: Follow = following(&pool, &alice, &bob).await;
    // … and dave's request to erin is erin's incoming queue, not alice's.
    let _other: Follow = asking(&pool, &dave, &erin).await;

    let token = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let bob_token = common::create_app_and_token(&pool, &bob, "read:follows").await;
    let app = common::test_app(pool);

    let listed = outgoing(&app, Some(&token), "").await;
    assert_eq!(
        listed.status,
        StatusCode::OK,
        "the caller sees their own asks"
    );
    assert_eq!(
        listed.header("content-type"),
        Some("application/json"),
        "the entities are JSON"
    );
    let body = listed.json_value();
    assert_eq!(
        listed_ids(&body),
        wanted_targets(&asks),
        "the two waiting asks, newest row first; the granted follow and the \
         other account's request stay out"
    );
    let carol_entry = listed_entry(&body, &carol.id);
    assert_eq!(
        carol_entry["acct"], "carol",
        "the local target renders as a full account with its handle"
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
            "Pleroma's outgoing index embeds no relationship"
        );
    }

    let empty = outgoing(&app, Some(&bob_token), "").await;
    assert_eq!(empty.status, StatusCode::OK, "an empty list is a 200");
    assert_eq!(
        empty.json_value(),
        json!([]),
        "bob was asked and asked nobody — alice's granted follow of him and \
         dave's ask to erin are not his outgoing requests"
    );
    assert_eq!(
        empty.header("link"),
        None,
        "an empty page sends no Link header"
    );
}

/// The list walks by `follows` row id: the `Link` header's cursors, the
/// `max_id` continuation, the `min_id` page (answered newest-first again) and
/// the empty ends of the walk, which carry no header.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_outgoing_list_pages_key_on_follow_row_ids_and_link(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let mut asks = Vec::new();
    for n in 0..5 {
        let target = locked_user(&pool, &format!("target{n}")).await;
        let row = asking(&pool, &alice, &target).await;
        asks.push(ask(&target, &row));
    }
    let token = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let app = common::test_app(pool);

    let wanted_users = wanted_targets(&asks);
    let wanted_rows = wanted_follows(&asks);

    let first = outgoing(&app, Some(&token), "?limit=2").await;
    assert_eq!(
        listed_ids(&first.json_value()),
        wanted_users[..2],
        "two newest first"
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link(&wanted_rows[1], &wanted_rows[0]).as_str()),
        "the Link cursors are the page's follow row ids, not account ids"
    );

    let older = outgoing(
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

    let newer = outgoing(&app, Some(&token), &format!("?min_id={}", wanted_rows[3])).await;
    assert_eq!(
        listed_ids(&newer.json_value()),
        wanted_users[..3],
        "min_id answers the newest-first page just after the cursor"
    );

    let polled = outgoing(&app, Some(&token), &format!("?since_id={}", wanted_rows[0])).await;
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

    let past = outgoing(&app, Some(&token), &format!("?max_id={}", wanted_rows[4])).await;
    assert_eq!(past.status, StatusCode::OK, "walking off the end is a 200");
    assert_eq!(past.json_value(), json!([]), "with the empty array");
    assert_eq!(past.header("link"), None, "and no header");
}

/// The listing pages with Pleroma's global pair, which is what the plain
/// [`PageParams`] extractor gives: twenty rows by default, forty at most.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_outgoing_default_page_is_twenty_and_the_cap_is_forty(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    for n in 0..45 {
        let target = locked_user(&pool, &format!("target{n}")).await;
        let _waiting: Follow = asking(&pool, &alice, &target).await;
    }
    let token = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let app = common::test_app(pool);

    let default = outgoing(&app, Some(&token), "").await;
    assert_eq!(
        listed_ids(&default.json_value()).len(),
        20,
        "Pleroma's default page"
    );
    let greedy = outgoing(&app, Some(&token), "?limit=100").await;
    assert_eq!(
        listed_ids(&greedy.json_value()).len(),
        40,
        "and the global cap, never more"
    );
    let between = outgoing(&app, Some(&token), "?limit=30").await;
    assert_eq!(
        listed_ids(&between.json_value()).len(),
        30,
        "a page under the cap goes through"
    );
}

/// All three routes demand a user token: no token is the 401 "Invalid
/// credentials", an invalid one the 401 "The access token is invalid", and an
/// app-only credential the 403 — before any scope or id is ever looked at.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn every_route_requires_a_user_token(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let app_only = common::create_app_only_token(&pool, "follow").await;
    let app = common::test_app(pool);
    let subscribe = subscribe_path(&alice.id);
    let unsubscribe = unsubscribe_path(&alice.id);
    let posts = [subscribe.as_str(), unsubscribe.as_str()];

    for path in posts {
        let anonymous = common::TestRequest::post(path).send(&app).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "no token cannot write {path}"
        );
        assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");

        let invalid = common::TestRequest::post(path)
            .bearer("bogus-token")
            .send(&app)
            .await;
        assert_eq!(
            invalid.status,
            StatusCode::UNAUTHORIZED,
            "an invalid token is not treated as anonymous"
        );
        assert_eq!(invalid.text(), INVALID_TOKEN, "AuthUser's invalid body");

        let app_token = common::TestRequest::post(path)
            .bearer(&app_only)
            .send(&app)
            .await;
        assert_eq!(
            app_token.status,
            StatusCode::FORBIDDEN,
            "an app-only token is not a user"
        );
        assert_eq!(app_token.text(), USER_REQUIRED, "the user-required body");
    }

    let anonymous = outgoing(&app, None, "").await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "no token cannot list the asks"
    );
    assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");
    let invalid = outgoing(&app, Some("bogus-token"), "").await;
    assert_eq!(
        invalid.status,
        StatusCode::UNAUTHORIZED,
        "and the listing demands a real one"
    );
    assert_eq!(invalid.text(), INVALID_TOKEN, "AuthUser's invalid body");
    let app_token = outgoing(&app, Some(&app_only), "").await;
    assert_eq!(
        app_token.status,
        StatusCode::FORBIDDEN,
        "the listing refuses app-only credentials alike"
    );
    assert_eq!(app_token.text(), USER_REQUIRED, "the user-required body");
}

/// The scope gates: the two writes take anything covering `follow` or
/// `write:follows`, the listing anything covering `read:follows`; everything
/// else is Pleroma's insufficient-permissions 403, naming the missing scopes
/// the way each route's plug does.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_routes_gate_on_the_follow_scopes(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _granted: Follow = following(&pool, &alice, &bob).await;
    let carol = locked_user(&pool, "carol").await;
    let mut waiting = asking(&pool, &alice, &carol).await;
    let app = common::test_app(pool.clone());

    for scopes in ["follow", "write:follows", "write"] {
        let good = common::create_app_and_token(&pool, &alice, scopes).await;
        for unsubscribe in [false, true] {
            let action = if unsubscribe {
                "unsubscribe"
            } else {
                "subscribe"
            };
            let response = subscription(&app, &good, &bob.id, unsubscribe).await;
            assert_eq!(response.status, StatusCode::OK, "{scopes} may {action}");
            assert_eq!(
                response.json_value()["subscribing"],
                Value::from(!unsubscribe),
                "and the write goes through, {action}"
            );
        }
    }
    for scopes in ["read:follows", "read", "write:accounts"] {
        let bad = common::create_app_and_token(&pool, &alice, scopes).await;
        for unsubscribe in [false, true] {
            let denied = subscription(&app, &bad, &bob.id, unsubscribe).await;
            assert_eq!(
                denied.status,
                StatusCode::FORBIDDEN,
                "{scopes} does not write subscriptions"
            );
            assert_eq!(
                denied.text(),
                NO_WRITE_SCOPE,
                "the denial joins both scopes"
            );
        }
    }

    for scopes in ["read", "read:follows", "follow"] {
        let good = common::create_app_and_token(&pool, &alice, scopes).await;
        let listed = outgoing(&app, Some(&good), "").await;
        assert_eq!(listed.status, StatusCode::OK, "{scopes} reads the asks");
        // `follow` is one of the listing's alternatives too, and the write it
        // grants is what takes the fixture ask out of the list again.
        let answered = follow::accept(&pool, waiting.id).await.expect("granted");
        assert_eq!(
            answered.state,
            FollowState::Accepted,
            "the ask is granted for the next round"
        );
        let empty = outgoing(&app, Some(&good), "").await;
        assert_eq!(
            empty.json_value(),
            json!([]),
            "{scopes} now reads an answered ask as gone"
        );
        // Take the answer back for the next round: rejecting the granted row
        // removes it, and a fresh request waits again.
        let withdrawn = follow::reject(&pool, answered.id).await.expect("withdrawn");
        assert_eq!(
            withdrawn.state,
            FollowState::Accepted,
            "the granted ask is withdrawn for {scopes}"
        );
        waiting = asking(&pool, &alice, &carol).await;
    }
    for scopes in ["write:follows", "write", "read:accounts"] {
        let bad = common::create_app_and_token(&pool, &alice, scopes).await;
        let denied = outgoing(&app, Some(&bad), "").await;
        assert_eq!(
            denied.status,
            StatusCode::FORBIDDEN,
            "{scopes} does not read the asks"
        );
        assert_eq!(
            denied.text(),
            NO_READ_SCOPE,
            "the denial names read:follows"
        );
    }
}

/// Every account miss on the two writes is the same 404: an id that is not
/// uuid text, an id that names no user, and a deactivated account — and the
/// refusal comes before the follow check, so a caller who is not following
/// the id still sees the 404.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn every_unknown_account_is_the_same_404(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let ghost = deactivated_user(&pool, "ghost").await;
    let token = common::create_app_and_token(&pool, &alice, "follow").await;
    let missing = Uuid::now_v7().to_string();
    let app = common::test_app(pool.clone());

    for action in ["subscribe", "unsubscribe"] {
        for target in [missing.as_str(), "not-an-id", &ghost.id.to_string()] {
            let response = raw_subscription(&app, &token, target, action).await;
            assert_eq!(
                response.status,
                StatusCode::NOT_FOUND,
                "`{target}` names no account, {action}"
            );
            assert_eq!(response.text(), NOT_FOUND, "decision 8's 404 body");
        }
    }
    assert_eq!(
        follow::get(&pool, alice.id, ghost.id)
            .await
            .unwrap()
            .map(|row| row.notify),
        None,
        "the refused writes stored no relationship with the deactivated account"
    );
}

/// A malformed cursor is the page parser's 400 with its message, and auth
/// still runs first: a bad token alongside a bad cursor is the 401.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_malformed_cursor_is_a_bad_request(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:follows").await;
    let app = common::test_app(pool);

    for query in ["?max_id=nope", "?min_id=01890b9a-nope", "?since_id=x%2y"] {
        let response = outgoing(&app, Some(&token), query).await;
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

    let denied = outgoing(&app, Some("bogus-token"), "?max_id=nope").await;
    assert_eq!(
        denied.status,
        StatusCode::UNAUTHORIZED,
        "the 401 comes before the 400"
    );
}
