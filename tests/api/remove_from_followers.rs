//! Integration tests for `POST /api/v1/accounts/{id}/remove_from_followers`
//! (WBE-64, the remove-a-follower ticket): removing an accepted follower
//! deletes its row, steps both denormalised counters down and answers the
//! caller's `Relationship` with `followed_by: false`; a second call and every
//! other non-follower — an unrelated account, one the caller merely follows,
//! a still-pending request — is the unchanged `Relationship` as a 200,
//! writing nothing and counting nothing; a malformed, unknown or deactivated
//! id is the shared 404; the caller's own id is the 422 "Can not remove
//! yourself from followers"; and the route runs `AuthUser` with
//! `write:follows | follow`, so a missing or invalid token is the 401, an
//! app-only credential and a scope-less token the 403.

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

/// The endpoint's path with the follower segment left exactly as given (the
/// miss tests feed raw text there).
fn route_text(target: &str) -> String {
    format!("/api/v1/accounts/{target}/remove_from_followers")
}

/// The endpoint's path for the follower `target`.
fn route(target: &Uuid) -> String {
    route_text(&target.to_string())
}

/// The 404 body every account route shares (decision 8).
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;
/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body for a token without either accepted scope; `require_any`
/// joins the alternatives with the `|` operator.
const NO_SCOPE: &str = r#"{"error":"Insufficient permissions: write:follows | follow."}"#;
/// The 422 body the self guard answers, this endpoint's own wording.
const SELF_REMOVE: &str = r#"{"error":"Can not remove yourself from followers"}"#;

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

/// Inserts a local [`User`] named `nickname` whose account is locked, so
/// every follow of it waits in the queue for an answer.
async fn locked_user(pool: &PgPool, nickname: &str) -> User {
    common::create_local_user_with(pool, nickname, |mut new| {
        new.locked = Flag::new(true);
        new
    })
    .await
}

/// Records `follower`'s follow of the open local `target` and asserts it
/// was granted on the spot — the fixture the endpoint removes.
async fn accepted_follower(pool: &PgPool, follower: &User, target: &User) -> Follow {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(
        row.state,
        FollowState::Accepted,
        "an unlocked local account answers at once"
    );
    row
}

/// Records `follower`'s follow of the locked `target` and asserts it
/// waits — the request the endpoint must leave alone.
async fn pending_follower(pool: &PgPool, follower: &User, target: &User) -> Follow {
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

/// The row of user `id` as it stands now, for reading counters back.
async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
    get_by_id(pool, id).await.unwrap().expect("the user exists")
}

/// `POST /api/v1/accounts/{target}/remove_from_followers` as `token`.
async fn remove(app: &Router, token: &str, target: &Uuid) -> common::TestResponse {
    common::TestRequest::post(&route(target))
        .bearer(token)
        .send(app)
        .await
}

/// A live follower loses its row and both counters step down; the caller's
/// other followers, their counters and the answered `Relationship` say the
/// same, and the repeat call on the now-absent follower is the unchanged
/// answer, not an error.
#[tokio::test]
async fn removing_a_follower_moves_counts_and_answers_the_relationship() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = common::create_local_user(&pool, "carol").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let erin = common::create_local_user(&pool, "erin").await;
    let _gone = accepted_follower(&pool, &alice, &carol).await;
    let _stays = accepted_follower(&pool, &erin, &carol).await;
    let token = common::create_app_and_token(&pool, &carol, "write:follows").await;
    let app = common::test_app(pool.clone());

    let response = remove(&app, &token, &alice.id).await;
    assert_eq!(response.status, StatusCode::OK, "the follower is removed");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the relationship is JSON"
    );
    assert_eq!(
        response.json_value(),
        expected_relationship(&alice.id, &json!({"followed_by": false})),
        "the caller's view of the ex-follower: nothing left, nothing else"
    );

    assert!(
        follow::get(&pool, alice.id, carol.id)
            .await
            .unwrap()
            .is_none(),
        "the accepted row is deleted, not restated"
    );
    assert_eq!(
        fresh_user(&pool, carol.id).await.followers_count,
        1,
        "carol kept exactly erin"
    );
    assert_eq!(
        fresh_user(&pool, alice.id).await.following_count,
        0,
        "alice follows nobody again"
    );
    assert_eq!(
        (
            fresh_user(&pool, erin.id).await.following_count,
            follow::get(&pool, erin.id, carol.id)
                .await
                .unwrap()
                .expect("erin still follows")
                .state,
        ),
        (1, FollowState::Accepted),
        "the other follower is untouched"
    );

    let twice = remove(&app, &token, &alice.id).await;
    assert_eq!(
        twice.status,
        StatusCode::OK,
        "a repeat finds nothing to remove"
    );
    assert_eq!(
        twice.json_value(),
        expected_relationship(&alice.id, &json!({})),
        "and answers the same unchanged relationship"
    );
    assert_eq!(
        fresh_user(&pool, carol.id).await.followers_count,
        1,
        "the no-op moved no counters"
    );
}

/// Accounts that are not the caller's followers are no-ops with the
/// unchanged relationship: an unrelated account, and one the caller follows
/// without being followed back — the caller's own follow is its own to give
/// back through unfollow, never removed from here.
#[tokio::test]
async fn non_followers_answer_the_unchanged_relationship() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let outgoing = accepted_follower(&pool, &carol, &bob).await;
    let token = common::create_app_and_token(&pool, &carol, "follow").await;
    let app = common::test_app(pool.clone());

    let stranger = remove(&app, &token, &dave.id).await;
    assert_eq!(
        stranger.status,
        StatusCode::OK,
        "an unrelated account is a no-op, not an error"
    );
    assert_eq!(
        stranger.json_value(),
        expected_relationship(&dave.id, &json!({})),
        "with the caller's default view of it"
    );

    let followed = remove(&app, &token, &bob.id).await;
    assert_eq!(
        followed.status,
        StatusCode::OK,
        "an account the caller follows is no follower to remove"
    );
    assert_eq!(
        followed.json_value(),
        expected_relationship(&bob.id, &json!({"following": true})),
        "and the relationship says what it said: carol follows bob"
    );
    assert_eq!(
        follow::get(&pool, carol.id, bob.id).await.unwrap(),
        Some(outgoing),
        "the outgoing follow row stands"
    );
    assert_eq!(
        fresh_user(&pool, bob.id).await.followers_count,
        1,
        "and so does its count"
    );
    assert!(
        follow::get(&pool, dave.id, carol.id)
            .await
            .unwrap()
            .is_none(),
        "the no-ops wrote no rows"
    );
}

/// A still-pending follow request is not a follower: the endpoint answers
/// the unchanged relationship, leaves the request in the queue to be
/// answered through `/api/v1/follow_requests`, and moves no counters.
#[tokio::test]
async fn a_pending_request_stays_out_of_reach() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = locked_user(&pool, "carol").await;
    let alice = common::create_local_user(&pool, "alice").await;
    let waiting = pending_follower(&pool, &alice, &carol).await;
    let token = common::create_app_and_token(&pool, &carol, "write:follows").await;
    let app = common::test_app(pool.clone());

    let response = remove(&app, &token, &alice.id).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "a pending requester is no follower to remove"
    );
    assert_eq!(
        response.json_value(),
        expected_relationship(&alice.id, &json!({})),
        "the caller's view stays at its defaults"
    );
    assert_eq!(
        follow::get(&pool, alice.id, carol.id)
            .await
            .unwrap()
            .expect("the request survives"),
        waiting,
        "row for row, the queue holds what it held"
    );
    assert_eq!(
        (
            fresh_user(&pool, carol.id).await.followers_count,
            fresh_user(&pool, alice.id).await.following_count,
        ),
        (0, 0),
        "a request never counted, and still does not"
    );
}

/// Every miss on the id is the shared 404, resolved before anything else
/// the request could mean: garbage text, an id that names no account, and a
/// deactivated follower whose accepted row stays in the table regardless.
#[tokio::test]
async fn unknown_accounts_are_the_shared_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = common::create_local_user(&pool, "carol").await;
    let ghost = common::create_local_user_with(&pool, "ghost", |mut new| {
        new.is_active = Flag::new(false);
        new
    })
    .await;
    let suspended = accepted_follower(&pool, &ghost, &carol).await;
    let missing = Uuid::now_v7().to_string();
    let ghost_id = ghost.id.to_string();
    let token = common::create_app_and_token(&pool, &carol, "write:follows").await;
    let app = common::test_app(pool.clone());

    for target in ["not-an-id", missing.as_str(), ghost_id.as_str()] {
        let response = common::TestRequest::post(&route_text(target))
            .bearer(&token)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "`{target}` resolves to no live account"
        );
        assert_eq!(response.text(), NOT_FOUND, "decision 8's 404 body");
    }
    assert_eq!(
        follow::get(&pool, ghost.id, carol.id)
            .await
            .unwrap()
            .expect("the deactivated follower's row"),
        suspended,
        "and the 404 touched nothing it hides"
    );
}

/// The caller's own id is the documented 422 — a follower is never the
/// followee themself — and the guard runs after the account is resolved, so
/// only a live account id can reach it.
#[tokio::test]
async fn removing_yourself_is_the_documented_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = common::create_local_user(&pool, "carol").await;
    let token = common::create_app_and_token(&pool, &carol, "follow").await;
    let app = common::test_app(pool.clone());

    let response = remove(&app, &token, &carol.id).await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "yourself is no follower to remove"
    );
    assert_eq!(response.text(), SELF_REMOVE, "this endpoint's own wording");
    assert_eq!(
        fresh_user(&pool, carol.id).await.followers_count,
        0,
        "and the refused call wrote nothing"
    );

    let malformed = common::TestRequest::post("/api/v1/accounts/not-an-id/remove_from_followers")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        malformed.status,
        StatusCode::NOT_FOUND,
        "a bad id is still the 404 that comes first"
    );
}

/// The route demands a user token: no token is the 401 "Invalid
/// credentials", an invalid one the 401 "The access token is invalid", and
/// an app-only credential the 403 — before any id or scope is ever looked
/// at.
#[tokio::test]
async fn the_route_requires_a_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app_only = common::create_app_only_token(&pool, "write:follows").await;
    let app = common::test_app(pool);

    let anonymous = common::TestRequest::post(&route(&alice.id))
        .send(&app)
        .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "no token cannot remove a follower"
    );
    assert_eq!(anonymous.text(), NO_TOKEN, "AuthUser's no-token body");

    let invalid = common::TestRequest::post(&route(&alice.id))
        .bearer("bogus-token")
        .send(&app)
        .await;
    assert_eq!(
        invalid.status,
        StatusCode::UNAUTHORIZED,
        "an invalid token is not ignored"
    );
    assert_eq!(invalid.text(), INVALID_TOKEN, "AuthUser's invalid body");

    let app_token = remove(&app, &app_only, &alice.id).await;
    assert_eq!(
        app_token.status,
        StatusCode::FORBIDDEN,
        "an app-only token is not a user"
    );
    assert_eq!(app_token.text(), USER_REQUIRED, "the user-required body");
}

/// The scope gate: anything covering `write:follows` or `follow` removes,
/// everything else is Pleroma's insufficient-permissions 403 naming both
/// alternatives — and another account's token, which passes the gate, is
/// only ever a caller without that follower: a no-op that leaves the row
/// where it is.
#[tokio::test]
async fn the_route_gates_on_the_follow_scopes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let mut followers = Vec::new();
    for n in 0..4 {
        let follower = common::create_local_user(&pool, &format!("follower{n}")).await;
        followers.push(accepted_follower(&pool, &follower, &carol).await);
    }
    let app = common::test_app(pool.clone());

    for (n, scopes) in ["write:follows", "follow", "write"].into_iter().enumerate() {
        let token = common::create_app_and_token(&pool, &carol, scopes).await;
        let target = &followers[n].follower_id;
        let removed = remove(&app, &token, target).await;
        assert_eq!(
            removed.status,
            StatusCode::OK,
            "{scopes} removes a follower"
        );
        assert_eq!(
            removed.json_value(),
            expected_relationship(target, &json!({})),
            "{scopes} answers the post-removal relationship"
        );
    }

    let survivor = &followers[3];
    let bystander = common::create_app_and_token(&pool, &dave, "write:follows").await;
    for scopes in ["read:follows", "read", "write:accounts"] {
        let denied_token = common::create_app_and_token(&pool, &carol, scopes).await;
        let denied = remove(&app, &denied_token, &survivor.follower_id).await;
        assert_eq!(
            denied.status,
            StatusCode::FORBIDDEN,
            "{scopes} does not remove followers"
        );
        assert_eq!(denied.text(), NO_SCOPE, "the denial joins both scopes");
    }
    let stolen = remove(&app, &bystander, &survivor.follower_id).await;
    assert_eq!(
        stolen.status,
        StatusCode::OK,
        "another account's token is just a caller without that follower"
    );
    assert_eq!(
        stolen.json_value(),
        expected_relationship(&survivor.follower_id, &json!({})),
        "answered dave's own unchanged view"
    );
    assert_eq!(
        follow::get(&pool, survivor.follower_id, carol.id)
            .await
            .unwrap()
            .expect("the fourth follower survives"),
        survivor.clone(),
        "every refusal and every no-op left its row in place"
    );
    assert_eq!(
        fresh_user(&pool, carol.id).await.followers_count,
        1,
        "and exactly the three removed follows came off the count"
    );
}
