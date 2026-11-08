//! Integration tests for the Pleroma account extensions merged at
//! `/api/v1/pleroma`: `POST /api/v1/pleroma/accounts/{id}/subscribe` and
//! `…/unsubscribe` (both halves of the ticket's one column, with the
//! `notifying` / `subscribing` flags of the answered `Relationship` and the
//! `follows.notify` fact behind them, refused with the ticket's 422 wherever
//! the caller has no accepted follow of the target, the shared 404 for every
//! account miss), `GET /api/v1/pleroma/outgoing_follow_requests` (the
//! caller's own pending asks as `Account` entities newest request first, with
//! keyset pagination whose cursors are the `follows` row ids under Pleroma's
//! global page sizes, and no `Link` header on an empty page); `AuthUser` in
//! front of those three, `follow | write:follows` for the two writes and
//! `read:follows` for the listing, and the page parser's 400 for a malformed
//! cursor; and `GET /api/v1/pleroma/accounts/{id}/favourites` (the target's
//! favourited statuses newest favourite first as `Status` entities, optional
//! auth with `read:favourites` demanded of a presented user token, the
//! `hide_favorites` 403 for every viewer but the target, the page narrowed
//! to what the viewer may see, `Link` cursors keyed on the favourite row
//! ids under Pleroma's default 20 and cap 40, and the shared 404).

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::cmp::Reverse;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        favourite,
        follow::{self, Follow, FollowState},
        status::{self, Status, Visibility},
        user::{Flag, User, get_by_id},
    },
};

use crate::common;

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
/// The 403 body the favourites route answers when the target hides their
/// favourites from the viewer — Pleroma's exact (American-spelled) wording.
const HIDDEN_FAVOURITES: &str = r#"{"error":"Can't get favorites"}"#;
/// The 403 body a presented token without `read:favourites` gets from the
/// optional gate.
const NO_FAV_SCOPE: &str = r#"{"error":"Insufficient permissions: read:favourites."}"#;
/// The 400 body of a malformed pagination cursor.
const BAD_ID: &str = r#"{"error":"Invalid id"}"#;

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

/// The subscription route segment `unsubscribe` picks — also the word the
/// assertions name the write by.
#[must_use]
fn subscription_verb(unsubscribe: bool) -> &'static str {
    if unsubscribe {
        "unsubscribe"
    } else {
        "subscribe"
    }
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
#[tokio::test]
async fn subscribe_requires_an_accepted_follow() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn subscribe_sets_both_flags_and_answers_the_relationship() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn unsubscribe_clears_the_flag_again() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn the_outgoing_list_answers_the_callers_own_asks() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn the_outgoing_list_pages_key_on_follow_row_ids_and_link() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn the_outgoing_default_page_is_twenty_and_the_cap_is_forty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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

/// The subscription routes and the outgoing listing demand a user token: no
/// token is the 401 "Invalid credentials", an invalid one the 401 "The
/// access token is invalid", and an app-only credential the 403 — before any
/// scope or id is ever looked at. (The favourites route is the optional-auth
/// exception, tested in
/// `the_favourites_gate_demands_the_scope_of_a_presented_user_token`.)
#[tokio::test]
async fn every_route_requires_a_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn the_routes_gate_on_the_follow_scopes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _granted: Follow = following(&pool, &alice, &bob).await;
    let carol = locked_user(&pool, "carol").await;
    let mut waiting = asking(&pool, &alice, &carol).await;
    let app = common::test_app(pool.clone());

    for scopes in ["follow", "write:follows", "write"] {
        let good = common::create_app_and_token(&pool, &alice, scopes).await;
        for unsubscribe in [false, true] {
            let action = subscription_verb(unsubscribe);
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
#[tokio::test]
async fn every_unknown_account_is_the_same_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn a_malformed_cursor_is_a_bad_request() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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

/// The favourites route for account `{target}` — spelled exactly as handed,
/// so the miss tests can feed garbage in the id segment.
fn favourites_path(target: &str) -> String {
    format!("/api/v1/pleroma/accounts/{target}/favourites")
}

/// `GET …/favourites{query}` as `token` (`None`: an anonymous request).
async fn favourites(
    app: &Router,
    token: Option<&str>,
    target: &str,
    query: &str,
) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("{}{query}", favourites_path(target)));
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// A local account that shares its favourites — `hide_favorites` off, the
/// one difference from the default — with a token granting `scopes`.
async fn open_account(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let user = common::create_local_user_with(pool, nickname, |mut new| {
        new.hide_favorites = Flag::new(false);
        new
    })
    .await;
    let token = common::create_app_and_token(pool, &user, scopes).await;
    (user, token)
}

/// A fresh local account named `nickname` with a bearer token granting
/// `scopes`, for the viewers the favourites tests read the lists through.
async fn viewer(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, scopes).await;
    (user, token)
}

/// A `visibility` post by `author`.
async fn post(pool: &PgPool, author: &User, visibility: Visibility, text: &str) -> Status {
    common::create_status(pool, author, text, visibility).await
}

/// A public post by a fresh local author named `nickname`.
async fn public_post_by(pool: &PgPool, nickname: &str) -> Status {
    let author = common::create_local_user(pool, nickname).await;
    post(pool, &author, Visibility::Public, "a public post").await
}

/// `who` favourites `status_id`, straight through the model. The write
/// answers nothing: the like rows are read back newest-first through
/// [`liked_pairs`], so no expectation depends on the write order.
async fn like(pool: &PgPool, who: &User, status_id: Uuid) {
    let created = favourite::favourite(pool, TEST_BASE_URL, who.id, status_id)
        .await
        .expect("the fixture favourite stores");
    assert!(created.is_some(), "the fixture pair had not favourited yet");
}

/// `who`'s likes as `(like id, status id)` pairs, newest like first — the
/// order the list answers in, read back from the table rather than from the
/// order the fixtures were written in (decision 4).
async fn liked_pairs(pool: &PgPool, who: &User) -> Vec<(Uuid, Uuid)> {
    sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT f.id, f.status_id FROM favourites f \
          WHERE f.user_id = $1 ORDER BY f.id DESC",
    )
    .bind(who.id)
    .fetch_all(pool)
    .await
    .expect("the like rows read")
}

/// The ids of every entity of a `Status` array answer, in response order.
fn listed_status_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the listing is an array")
        .iter()
        .map(|status| status["id"].as_str().expect("a status id").to_owned())
        .collect()
}

/// The status ids of `pairs`, newest first, keeping only those whose status
/// is one of `visible` — the expectation a narrowed favourites page must
/// answer.
fn wanted_status_ids(pairs: &[(Uuid, Uuid)], visible: &[Uuid]) -> Vec<String> {
    pairs
        .iter()
        .filter(|(_, status_id)| visible.contains(status_id))
        .map(|(_, status_id)| status_id.to_string())
        .collect()
}

/// The `Link` header of a favourites page over the route path `path`,
/// keyed on like row ids: `next` walks to the oldest visible like, `prev`
/// back to the newest.
fn expected_like_link(path: &str, newest: &Uuid, oldest: &Uuid) -> String {
    format!(
        "<{BASE}{path}?max_id={oldest}>; rel=\"next\", \
         <{BASE}{path}?min_id={newest}>; rel=\"prev\""
    )
}

/// The open account's favourites answer as `Status` entities, newest
/// favourite first, with the `Link` header keyed on the favourite row ids —
/// and the per-viewer flags render for whoever reads the list: `false` for
/// a stranger who likes none of these, `true` on the target's own read.
#[tokio::test]
async fn the_favourites_list_answers_the_open_account_s_likes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (bob, bob_token) = open_account(&pool, "bob", "read:favourites").await;
    let (_alice, alice_token) = viewer(&pool, "alice", "read:favourites").await;
    let author = common::create_local_user(&pool, "author").await;
    let mut statuses = Vec::new();
    for n in 0..3 {
        statuses.push(post(&pool, &author, Visibility::Public, &format!("post {n}")).await);
    }
    for status in &statuses {
        like(&pool, &bob, status.id).await;
    }
    let all = liked_pairs(&pool, &bob).await;
    let path = favourites_path(&bob.id.to_string());

    let listed = favourites(&app, Some(&alice_token), &bob.id.to_string(), "").await;
    common::assert_answer(&listed, StatusCode::OK, "an open list reads for a stranger");
    assert_eq!(
        listed.header("content-type"),
        Some("application/json"),
        "the entities are JSON"
    );
    let body = listed.json_value();
    let want: Vec<String> = all
        .iter()
        .map(|(_, status_id)| status_id.to_string())
        .collect();
    assert_eq!(listed_status_ids(&body), want, "newest favourite first");
    assert_eq!(
        body[0]["account"]["id"],
        author.id.to_string(),
        "the entries are full status entities with their author"
    );
    assert!(
        body.as_array()
            .expect("the array")
            .iter()
            .all(|entry| entry["favourited"] == Value::Bool(false)),
        "the flags answer the viewer, who favourited none of these"
    );
    assert_eq!(
        listed.header("link"),
        Some(expected_like_link(&path, &all[0].0, &all[2].0).as_str()),
        "the Link cursors are like row ids, not status ids"
    );

    let own = favourites(&app, Some(&bob_token), &bob.id.to_string(), "").await;
    common::assert_answer(&own, StatusCode::OK, "the target reads their own list");
    assert!(
        own.json_value()
            .as_array()
            .expect("the array")
            .iter()
            .all(|entry| entry["favourited"] == Value::Bool(true)),
        "every entry is the target's own favourite"
    );
}

/// `hide_favorites = true` — the default for every new account — is the
/// ticket's exact 403 for every viewer but the target: another account,
/// the anonymous request and an app-only token are all refused, while the
/// target's own read answers the list, and an account that turned the flag
/// off answers everybody.
#[tokio::test]
async fn hidden_favorites_refuse_everybody_but_the_target() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let carol = common::create_local_user(&pool, "carol").await;
    assert!(
        carol.hide_favorites.get(),
        "a new account hides its favourites by default"
    );
    let carol_token = common::create_app_and_token(&pool, &carol, "read:favourites").await;
    let (_alice, alice_token) = viewer(&pool, "alice", "read").await;
    let app_only = common::create_app_only_token(&pool, "read").await;
    let status = public_post_by(&pool, "author").await;
    like(&pool, &carol, status.id).await;
    let hidden = carol.id.to_string();

    for token in [Some(alice_token.as_str()), None, Some(app_only.as_str())] {
        let refused = favourites(&app, token, &hidden, "").await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            "hidden favourites refuse this viewer",
        );
        assert_eq!(refused.text(), HIDDEN_FAVOURITES, "the ticket's exact body");
    }

    let own = favourites(&app, Some(&carol_token), &hidden, "").await;
    common::assert_answer(
        &own,
        StatusCode::OK,
        "the target reads their own hidden list",
    );
    assert_eq!(
        listed_status_ids(&own.json_value()),
        vec![status.id.to_string()],
        "and sees the like the stranger was refused"
    );

    let (bob, _) = open_account(&pool, "bob", "read").await;
    assert!(
        !bob.hide_favorites.get(),
        "the fixture shares its favourites"
    );
    like(&pool, &bob, status.id).await;
    let shared = favourites(&app, Some(&alice_token), &bob.id.to_string(), "").await;
    common::assert_answer(
        &shared,
        StatusCode::OK,
        "the same viewer reads an open list",
    );
    assert_eq!(
        listed_status_ids(&shared.json_value()),
        vec![status.id.to_string()],
        "and gets the page"
    );
}

/// The listed statuses are narrowed to what the viewer may see: the
/// target's likes of a private post leave the stranger's page and reappear
/// when the stranger follows the author, a tombstoned post leaves for
/// everybody, and the `Link` edges are the likes that came back visible.
#[tokio::test]
async fn the_favourites_list_shows_only_what_the_viewer_may_see() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (bob, _) = open_account(&pool, "bob", "read").await;
    let (alice, alice_token) = viewer(&pool, "alice", "read:favourites").await;
    let author = common::create_local_user(&pool, "author").await;
    let open = post(&pool, &author, Visibility::Public, "open").await;
    let quiet = post(&pool, &author, Visibility::Unlisted, "quiet").await;
    let secret = post(&pool, &author, Visibility::Private, "followers only").await;
    let gone = post(&pool, &author, Visibility::Public, "about to go").await;
    for status in [&open, &quiet, &secret, &gone] {
        like(&pool, &bob, status.id).await;
    }
    let tombstone = status::soft_delete(&pool, gone.id)
        .await
        .expect("the delete runs")
        .expect("the post was live");
    assert!(tombstone.deleted_at.is_some(), "gone is a tombstone");
    let all = liked_pairs(&pool, &bob).await;
    let path = favourites_path(&bob.id.to_string());

    let listed = favourites(&app, Some(&alice_token), &bob.id.to_string(), "").await;
    common::assert_answer(&listed, StatusCode::OK, "the narrowed page answers");
    let visible = [open.id, quiet.id];
    let body = listed.json_value();
    assert_eq!(
        listed_status_ids(&body),
        wanted_status_ids(&all, &visible),
        "the private post and the tombstone are off a stranger's page"
    );
    let shown: Vec<Uuid> = all
        .iter()
        .filter(|(_, status_id)| visible.contains(status_id))
        .map(|(like_id, _)| *like_id)
        .collect();
    assert_eq!(
        listed.header("link"),
        Some(expected_like_link(&path, &shown[0], &shown[1]).as_str()),
        "only the visible likes name the page edges"
    );

    let following = follow::follow(&pool, TEST_BASE_URL, &alice, &author)
        .await
        .expect("alice follows the author");
    assert_eq!(
        following.state,
        FollowState::Accepted,
        "the unlocked author answers on the spot"
    );
    let after = favourites(&app, Some(&alice_token), &bob.id.to_string(), "").await;
    let visible_now = [open.id, quiet.id, secret.id];
    assert_eq!(
        listed_status_ids(&after.json_value()),
        wanted_status_ids(&all, &visible_now),
        "the follower's page carries the private favourite too"
    );

    let anonymous = favourites(&app, None, &bob.id.to_string(), "").await;
    assert_eq!(
        listed_status_ids(&anonymous.json_value()),
        wanted_status_ids(&all, &visible),
        "and the anonymous viewer keeps the pre-follow narrowing"
    );
}

/// The page walks the favourite row ids: a sized first page, the `max_id`
/// continuation, the `min_id` page answered newest-first again, and the
/// empty ends of the walk without a `Link` header.
#[tokio::test]
async fn the_favourites_list_pages_on_like_row_ids() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (bob, _) = open_account(&pool, "bob", "read").await;
    let (_alice, token) = viewer(&pool, "alice", "read:favourites").await;
    for n in 0..3 {
        let status = public_post_by(&pool, &format!("author{n}")).await;
        like(&pool, &bob, status.id).await;
    }
    let all = liked_pairs(&pool, &bob).await;
    let target = bob.id.to_string();
    let path = favourites_path(&target);

    let first = favourites(&app, Some(&token), &target, "?limit=2").await;
    common::assert_answer(&first, StatusCode::OK, "a sized page answers");
    assert_eq!(
        listed_status_ids(&first.json_value()),
        vec![all[0].1.to_string(), all[1].1.to_string()],
        "the two newest likes"
    );
    assert_eq!(
        first.header("link"),
        Some(expected_like_link(&path, &all[0].0, &all[1].0).as_str()),
        "the cursors carry the page's own like ids"
    );

    let older = favourites(
        &app,
        Some(&token),
        &target,
        &format!("?max_id={}", all[1].0),
    )
    .await;
    common::assert_answer(&older, StatusCode::OK, "the continuation answers");
    assert_eq!(
        listed_status_ids(&older.json_value()),
        vec![all[2].1.to_string()],
        "max_id walks to the older like, keyed on the like id"
    );
    assert_eq!(
        older.header("link"),
        Some(expected_like_link(&path, &all[2].0, &all[2].0).as_str()),
        "a one-entry page edges itself"
    );

    let newer = favourites(
        &app,
        Some(&token),
        &target,
        &format!("?min_id={}", all[2].0),
    )
    .await;
    assert_eq!(
        listed_status_ids(&newer.json_value()),
        vec![all[0].1.to_string(), all[1].1.to_string()],
        "min_id answers the newer page, still newest-first"
    );

    let polled = favourites(
        &app,
        Some(&token),
        &target,
        &format!("?since_id={}", all[0].0),
    )
    .await;
    common::assert_answer(&polled, StatusCode::OK, "polling past the head answers");
    assert_eq!(polled.json_value(), json!([]), "with the empty array");
    assert_eq!(polled.header("link"), None, "and no Link header");

    let past = favourites(
        &app,
        Some(&token),
        &target,
        &format!("?max_id={}", all[2].0),
    )
    .await;
    assert_eq!(
        past.json_value(),
        json!([]),
        "walking off the end is the empty page"
    );
    assert_eq!(past.header("link"), None, "which links nowhere");
}

/// A malformed favourites cursor is the page parser's 400 with its message,
/// and the extractor still runs first: a bad token alongside a bad cursor
/// is the 401, not a silent anonymous read.
#[tokio::test]
async fn the_favourites_cursor_mistakes_answer_400_behind_the_401() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (bob, _) = open_account(&pool, "bob", "read").await;
    let (_alice, token) = viewer(&pool, "alice", "read:favourites").await;
    let target = bob.id.to_string();

    for query in ["?max_id=nope", "?min_id=01890b9a-nope"] {
        let refused = favourites(&app, Some(&token), &target, query).await;
        common::assert_answer(
            &refused,
            StatusCode::BAD_REQUEST,
            &format!("`{query}` is refused"),
        );
        assert_eq!(refused.text(), BAD_ID, "the page parser's message");
    }
    let denied = favourites(&app, Some("bogus-token"), &target, "?max_id=nope").await;
    common::assert_answer(
        &denied,
        StatusCode::UNAUTHORIZED,
        "the 401 comes before the 400",
    );
    assert_eq!(denied.text(), INVALID_TOKEN, "AuthUser's invalid body");
}

/// Pleroma's global page sizes over the favourites walk: twenty rows by
/// default, forty at most — even though every one of the forty-one seeded
/// favourites is visible to the viewer.
#[tokio::test]
async fn the_favourites_default_page_is_twenty_and_the_cap_is_forty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (bob, _) = open_account(&pool, "bob", "read").await;
    let (_alice, token) = viewer(&pool, "alice", "read:favourites").await;
    let author = common::create_local_user(&pool, "author").await;
    for n in 0..41 {
        let status = post(&pool, &author, Visibility::Public, &format!("post {n}")).await;
        like(&pool, &bob, status.id).await;
    }
    let target = bob.id.to_string();

    let default = favourites(&app, Some(&token), &target, "").await;
    common::assert_answer(&default, StatusCode::OK, "the default page answers");
    assert_eq!(
        listed_status_ids(&default.json_value()).len(),
        20,
        "Pleroma's default page"
    );
    let greedy = favourites(&app, Some(&token), &target, "?limit=100").await;
    assert_eq!(
        listed_status_ids(&greedy.json_value()).len(),
        40,
        "and the global cap, never more — 41 likes were seeded"
    );
    let between = favourites(&app, Some(&token), &target, "?limit=30").await;
    assert_eq!(
        listed_status_ids(&between.json_value()).len(),
        30,
        "a page under the cap goes through"
    );
}

/// Every account miss on the favourites route is the same 404, whoever
/// presents: an id that is not uuid text, an id that names no user, and a
/// deactivated account — token or no token.
#[tokio::test]
async fn every_favourites_account_miss_is_the_same_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = viewer(&pool, "alice", "read:favourites").await;
    let ghost = deactivated_user(&pool, "ghost").await;
    let missing = Uuid::now_v7().to_string();

    for target in [missing.as_str(), "not-an-id", &ghost.id.to_string()] {
        for who in [Some(token.as_str()), None] {
            let response = favourites(&app, who, target, "").await;
            common::assert_answer(
                &response,
                StatusCode::NOT_FOUND,
                &format!("`{target}` names no account"),
            );
            assert_eq!(response.text(), NOT_FOUND, "decision 8's 404 body");
        }
    }
}

/// The optional gate: no token and an app-only token read the open list as
/// unauthenticated viewers; a presented *user* token must grant
/// `read:favourites` — anything else is Pleroma's insufficient-permissions
/// 403, checked before the account is ever looked at or its hidden-favourites
/// flag consulted; and a token that does not resolve is the 401.
#[tokio::test]
async fn the_favourites_gate_demands_the_scope_of_a_presented_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (bob, _) = open_account(&pool, "bob", "read").await;
    let (alice, _) = viewer(&pool, "alice", "read").await;
    let status = public_post_by(&pool, "author").await;
    like(&pool, &bob, status.id).await;
    let target = bob.id.to_string();

    let anonymous = favourites(&app, None, &target, "").await;
    common::assert_answer(&anonymous, StatusCode::OK, "no token reads an open list");
    assert_eq!(
        anonymous.json_value()[0]["favourited"],
        false,
        "rendered for the anonymous eye"
    );
    let app_only = common::create_app_only_token(&pool, "read").await;
    let headless = favourites(&app, Some(&app_only), &target, "").await;
    common::assert_answer(
        &headless,
        StatusCode::OK,
        "an app-only token proceeds unauthenticated",
    );

    for scopes in ["read", "read:favourites"] {
        let good = common::create_app_and_token(&pool, &alice, scopes).await;
        let listed = favourites(&app, Some(&good), &target, "").await;
        common::assert_answer(
            &listed,
            StatusCode::OK,
            &format!("`{scopes}` reads the list"),
        );
    }
    for scopes in ["write", "write:favourites", "read:statuses"] {
        let bad = common::create_app_and_token(&pool, &alice, scopes).await;
        let refused = favourites(&app, Some(&bad), &target, "").await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("`{scopes}` does not read favourites"),
        );
        assert_eq!(
            refused.text(),
            NO_FAV_SCOPE,
            "the denial names read:favourites"
        );
    }

    // The scopes plug runs before `assign_account_by_id` and before the
    // privacy arm: a scopeless probe stays on the scope refusal whether the
    // account is missing or its favourites hidden.
    let bad_token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let missing = Uuid::now_v7().to_string();
    let probe = favourites(&app, Some(&bad_token), &missing, "").await;
    common::assert_answer(
        &probe,
        StatusCode::FORBIDDEN,
        "the scope gate precedes the 404",
    );
    assert_eq!(probe.text(), NO_FAV_SCOPE, "the scope refusal, not a 404");
    let carol = common::create_local_user(&pool, "carol").await;
    let hidden_probe = favourites(&app, Some(&bad_token), &carol.id.to_string(), "").await;
    common::assert_answer(
        &hidden_probe,
        StatusCode::FORBIDDEN,
        "the scope gate precedes privacy",
    );
    assert_eq!(
        hidden_probe.text(),
        NO_FAV_SCOPE,
        "the scope refusal, not the privacy one"
    );

    let invalid = favourites(&app, Some("bogus-token"), &target, "").await;
    common::assert_answer(
        &invalid,
        StatusCode::UNAUTHORIZED,
        "a token that does not resolve",
    );
    assert_eq!(
        invalid.text(),
        INVALID_TOKEN,
        "is the 401, never a silent anonymous read"
    );
}
