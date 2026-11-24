//! Integration tests for the follow actions (WBE-57):
//! `POST /api/v1/accounts/{id}/follow` records the follow and answers the
//! caller's `Relationship` — `following: true` for a local account this
//! instance grants on the spot, `requested: true` for a locked or remote one
//! whose answer is not ours to give — and stores the two toggles its `reblogs`
//! / `notify` parameters name, from a JSON body, a form body or the query
//! string alike, on a relationship it just created *or* one that already
//! existed (which is how a repeat follow rewrites its flags); a parameter the
//! request left out keeps the stored flag, and a JSON `null` is the absent
//! one. Following yourself is the 422 "Can not follow yourself" the model
//! states, a malformed, unknown or deactivated id the shared 404, and a toggle
//! that is not a boolean the extractor's 422. `POST …/unfollow` takes the
//! follow (or the caller's own waiting request) back, steps the counters down
//! and answers the relationship without it, and is content with nothing to
//! remove: an account the caller never followed, one that only follows them,
//! and a repeat call are the unchanged relationship with 200. Both routes are
//! `AuthUser` with `write:follows | follow` — no or invalid token 401,
//! app-only credential and scope-less token 403 — and both count into the
//! limiter's one `relations_actions` bucket, which decorates their responses
//! with the `x-ratelimit-*` headers and refuses the request that spends it.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, Follow, FollowState},
        user::{Flag, User},
    },
};

use crate::common;

/// The 404 body every account route shares (decision 8).
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;
/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body for a token without either accepted scope; `require_any` joins
/// the alternatives with the `|` operator.
const NO_SCOPE: &str = r#"{"error":"Insufficient permissions: write:follows | follow."}"#;
/// The 422 body of the self guard, the model's (Pleroma's) wording.
const SELF_FOLLOW: &str = r#"{"error":"Can not follow yourself"}"#;
/// The 429 body of the spent bucket.
const THROTTLED: &str = r#"{"error":"Throttled"}"#;
/// The prefix of the 422 body a toggle of the wrong shape gets from the
/// extractor (decision 10), whatever detail serde added.
const INVALID_PARAMS: &str = r#"{"error":"Invalid parameters: "#;
/// The two write actions, so the auth, miss and bucket loops drive both.
const ACTIONS: [&str; 2] = ["follow", "unfollow"];

/// The follow action path of account `target`: `action` is `follow` or
/// `unfollow`.
fn route(target: &Uuid, action: &str) -> String {
    format!("/api/v1/accounts/{target}/{action}")
}

/// The same path with the id segment left exactly as given, for the miss tests
/// that feed raw text there.
fn route_text(target: &str, action: &str) -> String {
    format!("/api/v1/accounts/{target}/{action}")
}

/// The full relationship JSON for `target` with every flag at the documented
/// default, overwritten by `computed` — the expectation shape the relationship
/// batch's tests established for the entity.
fn relationship(target: &Uuid, computed: &Value) -> Value {
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

/// The answer of a granted follow whose request hid reblogs and turned
/// notifications on, for `target`: `subscribing` rides along as Pleroma's
/// alias of `notifying`.
fn flipped_follow(target: &Uuid) -> Value {
    relationship(
        target,
        &json!({
            "following": true,
            "showing_reblogs": false,
            "notifying": true,
            "subscribing": true,
        }),
    )
}

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` whose account is locked, so every follow of it
/// waits for its answer.
async fn locked_user(pool: &PgPool, nickname: &str) -> User {
    common::create_local_user_with(pool, nickname, |mut new| {
        new.locked = Flag::new(true);
        new
    })
    .await
}

/// The stored relationship of the pair, which the test expects to exist.
async fn relation(pool: &PgPool, follower: &User, target: &User) -> Follow {
    follow::get(pool, follower.id, target.id)
        .await
        .unwrap()
        .expect("the relationship exists")
}

/// The number of `follows` rows in the table, for the tests that must see
/// nothing written.
async fn row_count(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM follows")
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// `POST`s `path` with `token` and no body at all.
async fn post(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .send(app)
        .await
}

/// `POST`s `path` with `token` and `body` as a JSON document.
async fn post_json(app: &Router, token: &str, path: &str, body: &Value) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// Records `follower`'s granted follow of the open local `target` through the
/// model: the fixture the endpoint takes back or restates.
async fn accepted_follow(pool: &PgPool, follower: &User, target: &User) {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(
        row.state,
        FollowState::Accepted,
        "an unlocked local account answers at once"
    );
}

/// Following an open local account is granted on the spot: `following: true`,
/// the row accepted with the default toggles, both counters stepped up, and a
/// second call the same relationship over the same single row.
#[tokio::test]
async fn following_an_open_account_is_granted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:follows").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let response = post(&app, &token, &route(&bob.id, "follow")).await;
    assert_eq!(response.status, StatusCode::OK, "the follow is granted");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the relationship is JSON"
    );
    assert_eq!(
        response.json_value(),
        relationship(&bob.id, &json!({"following": true})),
        "the caller now follows the account, both toggles at their defaults"
    );

    let row = relation(&pool, &alice, &bob).await;
    assert_eq!(row.state, FollowState::Accepted, "granted, not waiting");
    assert!(row.show_reblogs, "reblogs show by default");
    assert!(!row.notify, "and notifying is off by default");
    assert!(
        row.ap_id
            .as_deref()
            .is_some_and(|ap_id| ap_id.starts_with(&format!("{TEST_BASE_URL}/activities/"))),
        "a local follow mints its own activity id (decision 6)"
    );
    assert_eq!(
        (
            common::reload_user(&pool, bob.id).await.followers_count,
            common::reload_user(&pool, alice.id).await.following_count
        ),
        (1, 1),
        "both denormalised counters moved once"
    );

    let twice = post(&app, &token, &route(&bob.id, "follow")).await;
    assert_eq!(twice.status, StatusCode::OK, "a repeat follow is no error");
    assert_eq!(
        twice.json_value(),
        relationship(&bob.id, &json!({"following": true})),
        "and answers the same relationship"
    );
    assert_eq!(
        relation(&pool, &alice, &bob).await,
        row,
        "the stored row is untouched, counters and all"
    );
    assert_eq!(
        (
            common::reload_user(&pool, bob.id).await.followers_count,
            common::reload_user(&pool, alice.id).await.following_count
        ),
        (1, 1),
        "following twice is not following twice"
    );
}

/// Following an account whose answer is not this instance's to give — locked
/// here, or owned elsewhere — files a request: `requested: true`,
/// `following: false`, and nothing counted on either end.
#[tokio::test]
async fn following_a_locked_or_remote_account_is_requested() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "follow").await;
    let carol = locked_user(&pool, "carol").await;
    let erin = common::create_remote_user(&pool, "erin", "remote.example").await;
    let app = common::test_app(pool.clone());

    let locked = post(&app, &token, &route(&carol.id, "follow")).await;
    assert_eq!(locked.status, StatusCode::OK, "the request is filed");
    assert_eq!(
        locked.json_value(),
        relationship(&carol.id, &json!({"requested": true})),
        "waiting is not following"
    );
    assert_eq!(
        relation(&pool, &alice, &carol).await.state,
        FollowState::Pending,
        "the row waits in the queue"
    );

    let remote = post(&app, &token, &route(&erin.id, "follow")).await;
    assert_eq!(remote.status, StatusCode::OK, "the remote request is filed");
    assert_eq!(
        remote.json_value(),
        relationship(&erin.id, &json!({"requested": true})),
        "and its answer belongs to `remote.example`"
    );
    assert_eq!(
        (
            common::reload_user(&pool, carol.id).await.followers_count,
            common::reload_user(&pool, erin.id).await.followers_count,
            common::reload_user(&pool, alice.id).await.following_count,
        ),
        (0, 0, 0),
        "requests count for nothing, on either end"
    );
}

/// The two toggles a follow names are stored on the relationship it creates
/// and read back as `showing_reblogs` / `notifying` (with Pleroma's
/// `subscribing` alias beside it).
#[tokio::test]
async fn follow_toggles_are_stored_with_the_follow() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:follows").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let first = post_json(
        &app,
        &token,
        &route(&bob.id, "follow"),
        &json!({"reblogs": false, "notify": true}),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "the toggles ride the follow");
    assert_eq!(
        first.json_value(),
        flipped_follow(&bob.id),
        "the answer states what was stored, not what the row defaults to"
    );
    let stored = relation(&pool, &alice, &bob).await;
    assert!(
        !stored.show_reblogs && stored.notify,
        "both columns hold the request's flags"
    );
    assert_eq!(row_count(&pool).await, 1, "one pair, one row");
    assert_eq!(
        (
            common::reload_user(&pool, bob.id).await.followers_count,
            common::reload_user(&pool, alice.id).await.following_count
        ),
        (1, 1),
        "and the toggled follow counted exactly once"
    );
}

/// A repeat follow of an existing pair is how a client rewrites its flags: the
/// parameter a request names moves, the one it leaves out keeps what is
/// stored, and a request that names neither restates the relationship over the
/// same single row.
#[tokio::test]
async fn a_repeat_follow_updates_the_toggles_it_names() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:follows").await;
    let bob = common::create_local_user(&pool, "bob").await;
    accepted_follow(&pool, &alice, &bob).await;
    let stored = relation(&pool, &alice, &bob).await;
    assert!(
        stored.show_reblogs && !stored.notify,
        "the fixture row starts on both defaults"
    );
    let app = common::test_app(pool.clone());

    let partial = post_json(
        &app,
        &token,
        &route(&bob.id, "follow"),
        &json!({"reblogs": false}),
    )
    .await;
    assert_eq!(
        partial.status,
        StatusCode::OK,
        "a repeat follow updates the pair that already exists"
    );
    assert_eq!(
        partial.json_value(),
        relationship(
            &bob.id,
            &json!({
                "following": true,
                "showing_reblogs": false,
                "notifying": false,
            })
        ),
        "`reblogs` moved; the `notify` the request left out kept its stored flag"
    );

    let notifying = post_json(
        &app,
        &token,
        &route(&bob.id, "follow"),
        &json!({"notify": true}),
    )
    .await;
    assert_eq!(
        notifying.json_value(),
        flipped_follow(&bob.id),
        "and the next call moves only the flag it names, leaving `reblogs` hidden"
    );

    let bare = post(&app, &token, &route(&bob.id, "follow")).await;
    assert_eq!(
        bare.status,
        StatusCode::OK,
        "a follow naming no toggle is a plain no-op"
    );
    assert_eq!(
        bare.json_value(),
        flipped_follow(&bob.id),
        "and it rewrites neither flag"
    );
    assert_eq!(
        relation(&pool, &alice, &bob).await.id,
        stored.id,
        "the whole sequence restated one row and never wrote a second"
    );
    assert_eq!(row_count(&pool).await, 1, "one pair, one row");
}

/// The parameters arrive from every shape the extractor speaks (decision 10): a
/// JSON body, an `application/x-www-form-urlencoded` one, and the query string
/// — including the `"1"` / `"0"` spellings a form or a query sends for
/// booleans.
#[tokio::test]
async fn follow_toggles_arrive_as_json_form_or_query() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "follow").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let erin = common::create_local_user(&pool, "erin").await;
    let app = common::test_app(pool.clone());

    let json_call = post_json(
        &app,
        &token,
        &route(&bob.id, "follow"),
        &json!({"reblogs": false, "notify": true}),
    )
    .await;
    let form_call = common::TestRequest::post(&route(&dave.id, "follow"))
        .bearer(&token)
        .form(&[("reblogs", "false"), ("notify", "true")])
        .send(&app)
        .await;
    let query_call = post(
        &app,
        &token,
        &format!("{}?reblogs=0&notify=1", route(&erin.id, "follow")),
    )
    .await;

    for (who, response, target) in [
        ("json", json_call, &bob),
        ("form", form_call, &dave),
        ("query", query_call, &erin),
    ] {
        assert_eq!(
            response.status,
            StatusCode::OK,
            "the {who} spelling of the toggles is accepted"
        );
        assert_eq!(
            response.json_value(),
            flipped_follow(&target.id),
            "and {who} stored the same two flags"
        );
        let row = relation(&pool, &alice, target).await;
        assert!(
            !row.show_reblogs && row.notify,
            "the {who} row holds both columns"
        );
    }
    assert_eq!(row_count(&pool).await, 3, "three pairs, three rows");
}

/// A request that is still waiting carries the toggles it was filed with: the
/// answer reports them, and the stored row holds them for whenever the account
/// grants it.
#[tokio::test]
async fn a_pending_follow_is_filed_with_its_flags() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:follows").await;
    let carol = locked_user(&pool, "carol").await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(&route(&carol.id, "follow"))
        .bearer(&token)
        .form(&[("reblogs", "false"), ("notify", "true")])
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the request is filed");
    assert_eq!(
        response.json_value(),
        relationship(
            &carol.id,
            &json!({
                "requested": true,
                "showing_reblogs": false,
                "notifying": true,
                "subscribing": true,
            })
        ),
        "a waiting request still reports the toggles it was filed with"
    );
    let row = relation(&pool, &alice, &carol).await;
    assert_eq!(row.state, FollowState::Pending, "still waiting");
    assert!(
        !row.show_reblogs && row.notify,
        "the flags are stored for whenever it is granted"
    );
}

/// Unfollowing takes the follow back: the row is gone, both counters step down
/// and the answered relationship holds nothing. A waiting request of the
/// caller's own is withdrawn the same way, a follower of the caller is left
/// alone, and a pair that never followed is content with nothing to remove.
#[tokio::test]
async fn unfollowing_takes_the_follow_back() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "follow").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = locked_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    accepted_follow(&pool, &alice, &bob).await;
    let waiting = follow::follow(&pool, TEST_BASE_URL, &alice, &carol)
        .await
        .expect("the request to the locked account waits");
    assert_eq!(waiting.state, FollowState::Pending, "the fixture waits");
    accepted_follow(&pool, &dave, &alice).await;
    let app = common::test_app(pool.clone());

    let response = post(&app, &token, &route(&bob.id, "unfollow")).await;
    assert_eq!(response.status, StatusCode::OK, "the follow is withdrawn");
    assert_eq!(
        response.json_value(),
        relationship(&bob.id, &json!({})),
        "nothing is left of the relationship"
    );
    assert!(
        follow::get(&pool, alice.id, bob.id)
            .await
            .unwrap()
            .is_none(),
        "the row is deleted, not restated"
    );
    assert_eq!(
        (
            common::reload_user(&pool, bob.id).await.followers_count,
            common::reload_user(&pool, alice.id).await.following_count
        ),
        (0, 0),
        "the removed follow came off both counters, and carol's standing request never counted"
    );

    let twice = post(&app, &token, &route(&bob.id, "unfollow")).await;
    assert_eq!(twice.status, StatusCode::OK, "a repeat finds nothing");
    assert_eq!(
        twice.json_value(),
        relationship(&bob.id, &json!({})),
        "and answers the same unchanged relationship"
    );

    let request = post(&app, &token, &route(&carol.id, "unfollow")).await;
    assert_eq!(
        request.status,
        StatusCode::OK,
        "the caller's own waiting request is hers to withdraw"
    );
    assert_eq!(
        request.json_value(),
        relationship(&carol.id, &json!({})),
        "so `requested` is gone with it"
    );
    assert!(
        follow::get(&pool, alice.id, carol.id)
            .await
            .unwrap()
            .is_none(),
        "and the row left the queue"
    );

    let follower = post(&app, &token, &route(&dave.id, "unfollow")).await;
    assert_eq!(
        follower.status,
        StatusCode::OK,
        "an account that follows the caller is nothing to unfollow"
    );
    assert_eq!(
        follower.json_value(),
        relationship(&dave.id, &json!({"followed_by": true})),
        "so its side of the pair stands"
    );
    assert_eq!(
        (
            common::reload_user(&pool, alice.id).await.followers_count,
            common::reload_user(&pool, dave.id).await.following_count
        ),
        (1, 1),
        "counted as it was, and still is"
    );
}

/// Following yourself is the 422 with Pleroma's wording on the follow route,
/// body or no body; the unfollow route has no such row to refuse and answers
/// its own id as the no-op it is.
#[tokio::test]
async fn following_yourself_is_the_documented_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:follows").await;
    let app = common::test_app(pool.clone());

    let response = post(&app, &token, &route(&alice.id, "follow")).await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "yourself is no account to follow"
    );
    assert_eq!(response.text(), SELF_FOLLOW, "the model's own wording");

    let toggle = post_json(
        &app,
        &token,
        &route(&alice.id, "follow"),
        &json!({"reblogs": false, "notify": true}),
    )
    .await;
    assert_eq!(
        toggle.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a body does not make a self follow legal"
    );
    assert_eq!(row_count(&pool).await, 0, "and neither refusal wrote a row");

    let unfollow = post(&app, &token, &route(&alice.id, "unfollow")).await;
    assert_eq!(
        unfollow.status,
        StatusCode::OK,
        "unfollowing yourself withdraws the follow that cannot exist"
    );
    assert_eq!(
        unfollow.json_value(),
        relationship(&alice.id, &json!({})),
        "answered as the caller's own default relationship"
    );
    assert_eq!(
        common::reload_user(&pool, alice.id).await.following_count,
        0,
        "the whole sequence counted nothing"
    );
}

/// Every miss on the id is the shared 404, on both routes and ahead of anything
/// else the request could mean: garbage text, an id that names no account, and
/// a deactivated account whose stored row stays exactly where it was.
#[tokio::test]
async fn unknown_accounts_are_the_shared_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "follow").await;
    let ghost = common::create_local_user_with(&pool, "ghost", |mut new| {
        new.is_active = Flag::new(false);
        new
    })
    .await;
    accepted_follow(&pool, &ghost, &alice).await;
    let missing = Uuid::now_v7().to_string();
    let ghost_id = ghost.id.to_string();
    let app = common::test_app(pool.clone());

    for action in ACTIONS {
        for target in ["not-an-id", missing.as_str(), ghost_id.as_str()] {
            let response = post_json(
                &app,
                &token,
                &route_text(target, action),
                &json!({"reblogs": false, "notify": true}),
            )
            .await;
            assert_eq!(
                response.status,
                StatusCode::NOT_FOUND,
                "`{target}` resolves to no live account on {action}"
            );
            assert_eq!(response.text(), NOT_FOUND, "decision 8's 404 body");
        }
    }
    assert_eq!(
        row_count(&pool).await,
        1,
        "the six 404s wrote nothing, not even a toggle on the hidden ghost's row"
    );
    assert!(
        follow::get(&pool, alice.id, ghost.id)
            .await
            .unwrap()
            .is_none(),
        "and none of them followed anybody"
    );
}

/// A toggle that is not a boolean is the extractor's 422 (decision 10),
/// whatever the rest of the request asked for — while a JSON `null` is the
/// absent one and leaves the follow to the column defaults.
#[tokio::test]
async fn a_malformed_toggle_is_the_extractors_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "follow").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    for body in [
        json!({"reblogs": "maybe"}),
        json!({"notify": 3}),
        json!({"reblogs": true, "notify": "sometimes"}),
    ] {
        let response = post_json(&app, &token, &route(&bob.id, "follow"), &body).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a toggle that is neither a boolean nor a boolean string is refused"
        );
        assert!(
            response.text().starts_with(INVALID_PARAMS),
            "the extractor words its own refusal: {}",
            response.text()
        );
    }
    assert_eq!(row_count(&pool).await, 0, "nothing was filed for them");

    let null = post_json(
        &app,
        &token,
        &route(&bob.id, "follow"),
        &json!({"reblogs": null, "notify": null}),
    )
    .await;
    assert_eq!(
        null.status,
        StatusCode::OK,
        "a `null` toggle is the absent one (decision 10)"
    );
    assert_eq!(
        null.json_value(),
        relationship(&bob.id, &json!({"following": true})),
        "so the follow is the plain default one"
    );
    assert_eq!(
        relation(&pool, &alice, &bob).await.state,
        FollowState::Accepted,
        "granted, with both columns at their defaults"
    );
}

/// Both routes demand a user token: no token is the 401 "Invalid credentials",
/// an invalid one the 401 "The access token is invalid", and an app-only
/// credential the 403 — before any id or scope is ever looked at.
#[tokio::test]
async fn the_routes_require_a_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", "follow").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app_only = common::create_app_only_token(&pool, "write:follows").await;
    let app = common::test_app(pool.clone());

    for action in ACTIONS {
        let path = route(&bob.id, action);
        let anonymous = common::TestRequest::post(&path).send(&app).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "no token at all"
        );
        assert_eq!(anonymous.text(), NO_TOKEN, "`AuthUser`'s no-token body");

        let invalid = common::TestRequest::post(&path)
            .bearer("bogus-token")
            .send(&app)
            .await;
        assert_eq!(
            invalid.status,
            StatusCode::UNAUTHORIZED,
            "an invalid token is not ignored"
        );
        assert_eq!(invalid.text(), INVALID_TOKEN, "the invalid-token body");

        let app_token = post(&app, &app_only, &path).await;
        assert_eq!(app_token.status, StatusCode::FORBIDDEN, "an app-only token");
        assert_eq!(app_token.text(), USER_REQUIRED, "the user-required body");
    }
    assert_eq!(row_count(&pool).await, 0, "none of them reached the write");
    assert_eq!(
        common::reload_user(&pool, alice.id).await.following_count,
        0,
        "and none of them counted anything"
    );
}

/// The scope gate: `write:follows`, the `follow` root and the `write` root all
/// pass on both routes, and anything narrower is Pleroma's
/// insufficient-permissions 403 naming both accepted alternatives.
#[tokio::test]
async fn the_routes_gate_on_the_follow_scopes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", "follow").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    for (n, scopes) in ["write:follows", "follow", "write"].into_iter().enumerate() {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let target = common::create_local_user(&pool, &format!("granted{n}")).await;
        let followed = post(&app, &token, &route(&target.id, "follow")).await;
        common::assert_answer(&followed, StatusCode::OK, &format!("{scopes} follows"));
        assert_eq!(
            followed.json_value(),
            relationship(&target.id, &json!({"following": true})),
            "{scopes} got the relationship"
        );
        let dropped = post(&app, &token, &route(&target.id, "unfollow")).await;
        common::assert_answer(&dropped, StatusCode::OK, &format!("{scopes} unfollows"));
    }

    for scopes in ["read:follows", "read", "write:accounts", "read:accounts"] {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        for action in ACTIONS {
            let denied = post(&app, &token, &route(&bob.id, action)).await;
            common::assert_answer(
                &denied,
                StatusCode::FORBIDDEN,
                &format!("{scopes} on {action}"),
            );
            assert_eq!(denied.text(), NO_SCOPE, "the denial joins both scopes");
        }
    }
    assert!(
        follow::get(&pool, alice.id, bob.id)
            .await
            .unwrap()
            .is_none(),
        "and every 403 wrote nothing"
    );
}

/// Both routes count into the one `relations_actions` bucket: their responses
/// carry the `x-ratelimit-*` headers, the budget is shared across the two
/// paths, and the request that spends it is refused 429 before it can reach the
/// write.
#[tokio::test]
async fn both_routes_share_the_relations_actions_bucket() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "follow").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("relations_actions=2:600"),
    );

    for (n, action) in ACTIONS.into_iter().enumerate() {
        let response = post(&app, &token, &route(&bob.id, action)).await;
        common::assert_answer(&response, StatusCode::OK, "inside the budget");
        assert_eq!(
            response.header("x-ratelimit-limit"),
            Some("2"),
            "the bucket's budget on {action}"
        );
        assert_eq!(
            response.header("x-ratelimit-remaining"),
            Some(if n == 0 { "1" } else { "0" }),
            "the window counts down across both paths"
        );
        assert!(
            response.header("x-ratelimit-reset").is_some(),
            "and says when the window reopens"
        );
    }

    let throttled = post(&app, &token, &route(&bob.id, "follow")).await;
    common::assert_answer(
        &throttled,
        StatusCode::TOO_MANY_REQUESTS,
        "the spent window",
    );
    assert_eq!(
        throttled.text(),
        THROTTLED,
        "decision 8's body for a throttle"
    );
    assert_eq!(
        throttled.header("retry-after"),
        throttled.header("x-ratelimit-reset"),
        "the refusal says when the window reopens"
    );
    assert_eq!(
        throttled.header("x-ratelimit-remaining"),
        Some("0"),
        "and states the spent budget"
    );
    assert_eq!(
        row_count(&pool).await,
        0,
        "the follow, its unfollow and the refused retry left no row"
    );
    assert_eq!(
        common::reload_user(&pool, alice.id).await.following_count,
        0,
        "and nothing moved the counters either"
    );
}

/// The default test configuration switches the limiter off: the routes still
/// answer normally and carry no `x-ratelimit-*` headers at all.
#[tokio::test]
async fn the_bucket_headers_are_absent_when_the_limiter_is_off() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "follow").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool);

    for action in ACTIONS {
        let response = post(&app, &token, &route(&bob.id, action)).await;
        common::assert_answer(&response, StatusCode::OK, "a request on the plain app");
        for name in [
            "x-ratelimit-limit",
            "x-ratelimit-remaining",
            "x-ratelimit-reset",
        ] {
            assert_eq!(
                response.header(name),
                None,
                "an uncounted route answers no `{name}` on {action}"
            );
        }
    }
}
