//! Account aliases and `move_account` (`/api/pleroma/aliases`,
//! `/api/pleroma/move_account`, WBE-137) over the real router.
//!
//! The alias tests walk the full add → list → remove lifecycle and pin
//! the render rules (handle for a mirrored actor, raw URL otherwise,
//! append-if-absent); the move tests pin Pleroma's gate order (password,
//! target resolution, `alsoKnownAs` precondition, 30-day cooldown), the
//! follower transfer (local accepted followers move, opted-out and
//! remote ones stay), the counters that move with it, the entity's
//! nested non-recursive `moved` render, and every route's `AuthUser` +
//! scope pipeline.

#![cfg(test)]

pub mod common;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, FollowState},
        user::{self, Flag, User},
    },
};

/// Password every login-capable fixture shares.
const PASSWORD: &str = "correct horse battery staple";
/// The aliases route.
const ALIASES: &str = "/api/pleroma/aliases";
/// The move endpoint.
const MOVE: &str = "/api/pleroma/move_account";
/// A handle `WebFinger` refuses before any request (single-label
/// domain), so the unresolved-target tests stay network-free.
const UNRESOLVABLE: &str = "ghost@invalid";
/// An actor id no row of the test database mirrors.
const UNSEEN_URL: &str = "https://unseen.example/users/ghost";

/// Inserts a local user named `nickname` that can log in with
/// [`PASSWORD`] plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_user_with_password(pool, nickname, PASSWORD).await;
    let minted = common::create_app_and_token(pool, &created, scopes).await;
    (created, minted)
}

/// Inserts a local user named `nickname` that lists `origin`'s actor id
/// among its aliases: the move precondition's satisfied target.
async fn aliased_target(pool: &PgPool, nickname: &str, origin: &User) -> User {
    let origin_ap_id = origin.ap_id.clone();
    common::create_local_user_with(pool, nickname, |mut new| {
        new.also_known_as = vec![origin_ap_id];
        new
    })
    .await
}

/// `GET /api/pleroma/aliases` with `token`.
async fn get_aliases(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get(ALIASES)
        .bearer(token)
        .send(app)
        .await
}

/// `PUT /api/pleroma/aliases` naming `alias` (as JSON).
async fn put_alias(app: &Router, token: &str, alias: &str) -> common::TestResponse {
    common::TestRequest::put(ALIASES)
        .bearer(token)
        .json(&json!({"alias": alias}))
        .send(app)
        .await
}

/// `DELETE /api/pleroma/aliases` naming `alias` (as a form body, the
/// other spelling decision 10 accepts).
async fn delete_alias(app: &Router, token: &str, alias: &str) -> common::TestResponse {
    common::TestRequest::delete(ALIASES)
        .bearer(token)
        .form(&[("alias", alias)])
        .send(app)
        .await
}

/// `POST /api/pleroma/move_account` (as a form body).
async fn move_account(
    app: &Router,
    token: &str,
    password: &str,
    target: &str,
) -> common::TestResponse {
    common::TestRequest::post(MOVE)
        .bearer(token)
        .form(&[("password", password), ("target_account", target)])
        .send(app)
        .await
}

/// The stored row of user `id`.
async fn stored(pool: &PgPool, id: Uuid) -> User {
    user::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the fixture row exists")
}

/// `follower` follows `who` through the model's normal follow path.
async fn follow_user(pool: &PgPool, follower: &User, who: &User) {
    let _granted = follow::follow(pool, TEST_BASE_URL, follower, who)
        .await
        .expect("the follow fixture is recorded");
}

/// The stored relationship of the pair, which the test expects to exist.
async fn relation(pool: &PgPool, follower: &User, target: &User) -> follow::Follow {
    follow::get(pool, follower.id, target.id)
        .await
        .unwrap()
        .expect("the relationship exists")
}

/// The aliases lifecycle over a mirror-backed alias, a local-account
/// alias and a raw URL this instance cannot resolve to a row.
#[tokio::test]
async fn add_list_and_remove_aliases() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.also_known_as = vec![UNSEEN_URL.to_owned()];
        new
    })
    .await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let erin = common::create_local_user(&pool, "erin").await;
    let token = common::create_app_and_token(&pool, &alice, "read write").await;
    let app = common::test_app(pool.clone());

    let listed = get_aliases(&app, &token).await;
    assert_eq!(listed.status, StatusCode::OK, "the owner may list");
    assert_eq!(
        listed.json_value(),
        json!({"aliases": [UNSEEN_URL]}),
        "an id with no row falls back to the raw URL"
    );

    let added = put_alias(&app, &token, "bob@remote.example").await;
    assert_eq!(
        added.json_value(),
        json!({"status": "success"}),
        "adding a mirrored remote handle succeeds"
    );
    assert_eq!(
        stored(&pool, alice.id).await.also_known_as,
        vec![UNSEEN_URL.to_owned(), bob.ap_id.clone()],
        "the target's actor id is appended, in order"
    );

    let again = put_alias(&app, &token, "bob@remote.example").await;
    assert_eq!(again.status, StatusCode::OK, "adding twice succeeds");
    assert_eq!(
        stored(&pool, alice.id).await.also_known_as.len(),
        2,
        "and appends only once"
    );

    let by_url = put_alias(&app, &token, &erin.ap_id).await;
    assert_eq!(
        by_url.status,
        StatusCode::OK,
        "an alias given as an ActivityPub id resolves too"
    );

    let listing = get_aliases(&app, &token).await.json_value();
    assert_eq!(
        listing,
        json!({"aliases": [UNSEEN_URL, "bob@remote.example", "erin@test.example"]}),
        "mirrored aliases render as full acct handles (Pleroma's \
         User.full_nickname), unknown ids stay raw URLs"
    );

    let removed = delete_alias(&app, &token, "erin@test.example").await;
    assert_eq!(
        removed.json_value(),
        json!({"status": "success"}),
        "removal by the handle the list rendered"
    );
    let also = delete_alias(&app, &token, UNSEEN_URL).await;
    assert_eq!(also.status, StatusCode::OK, "removal by the raw URL");
    assert_eq!(
        stored(&pool, alice.id).await.also_known_as,
        vec![bob.ap_id.clone()],
        "only the untouched alias remains"
    );

    let nothing = delete_alias(&app, &token, UNRESOLVABLE).await;
    assert_eq!(
        nothing.json_value(),
        json!({"status": "success"}),
        "removing an alias that was never there is a no-op success"
    );
    assert_eq!(
        stored(&pool, alice.id).await.also_known_as,
        vec![bob.ap_id.clone()],
        "and changed nothing"
    );
}

/// A target neither the database nor a fast-failing `WebFinger` check
/// can answer is the 404 the ticket fixes; nothing is appended.
#[tokio::test]
async fn an_unresolvable_alias_target_is_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    for alias in [UNRESOLVABLE, "nobody"] {
        let response = put_alias(&app, &token, alias).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "`{alias}` resolves to no account: 404"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Record not found"}),
            "the generic not-found body, `{alias}`"
        );
    }
    assert!(
        stored(&pool, alice.id).await.also_known_as.is_empty(),
        "the refusals appended nothing"
    );
}

/// An `alias` the body does not carry is the extractor's 422.
#[tokio::test]
async fn an_alias_body_without_the_parameter_is_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());
    let response = common::TestRequest::put(ALIASES)
        .bearer(&token)
        .json(&json!({}))
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY, "422");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the rejection is JSON"
    );
}

/// Alice's four followers: two ordinary local accounts, one that opted out of
/// having its following moved, and one remote mirror.
async fn four_followers(pool: &PgPool, alice: &User) -> (User, User, User, User) {
    let carol = common::create_local_user(pool, "carol").await;
    let dave = common::create_local_user(pool, "dave").await;
    let erin = common::create_local_user_with(pool, "erin", |mut new| {
        new.allow_following_move = Flag::new(false);
        new
    })
    .await;
    let frank = common::create_remote_user(pool, "frank", "remote.example").await;
    for follower in [&carol, &dave, &erin, &frank] {
        follow_user(pool, follower, alice).await;
    }
    (carol, dave, erin, frank)
}

/// A follower the move carried over: accepted on the target (the target is
/// local and unlocked), gone from the origin, own counter unchanged.
async fn assert_carried(pool: &PgPool, follower: &User, origin: &User, target: &User) {
    let carried = relation(pool, follower, target).await;
    assert_eq!(
        carried.state,
        FollowState::Accepted,
        "{} followed the target and was accepted on the spot",
        follower.nickname
    );
    assert!(
        follow::get(pool, follower.id, origin.id)
            .await
            .unwrap()
            .is_none(),
        "{} stopped following the origin",
        follower.nickname
    );
    assert_eq!(
        stored(pool, follower.id).await.following_count,
        1,
        "the follower's own count ends unchanged"
    );
}

/// A follower the move left alone: still following the origin, and not
/// following the target.
async fn assert_stays(pool: &PgPool, stay: &User, origin: &User, target: &User) {
    assert_eq!(
        relation(pool, stay, origin).await.state,
        FollowState::Accepted,
        "{} keeps the original relationship",
        stay.nickname
    );
    assert!(
        follow::get(pool, stay.id, target.id)
            .await
            .unwrap()
            .is_none(),
        "{} is not carried over",
        stay.nickname
    );
}

/// The full move: gates pass, the account is marked moved with its
/// cooldown stamp, every local follower that allows the move follows the
/// target (accepted on the spot, the target is local and unlocked) and
/// unfollows the origin — while opted-out and remote followers stay —
/// and the counters end where the relationships are.
#[tokio::test]
async fn move_transfers_local_followers_and_marks_the_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let mut bob = aliased_target(&pool, "bob", &alice).await;
    let zoe = common::create_local_user(&pool, "zoe").await;
    // A target that moved on itself: the render must not follow it (see
    // the entity assertions below).
    let _retargeted = sqlx::query("UPDATE users SET moved_to_ap_id = $2 WHERE id = $1")
        .bind(bob.id)
        .bind(&zoe.ap_id)
        .execute(&pool)
        .await
        .expect("the target's own move stamps in");
    bob = stored(&pool, bob.id).await;

    let (carol, dave, erin, frank) = four_followers(&pool, &alice).await;

    let app = common::test_app(pool.clone());
    let response = move_account(&app, &token, PASSWORD, "bob").await;
    assert_eq!(response.status, StatusCode::OK, "the move answers");
    assert_eq!(
        response.json_value(),
        json!({"status": "success"}),
        "the documented success body"
    );

    let moved = stored(&pool, alice.id).await;
    assert_eq!(
        moved.moved_to_ap_id.as_deref(),
        Some(bob.ap_id.as_str()),
        "the account records its move target"
    );
    assert!(
        moved.last_move_at.is_some(),
        "and the cooldown stamp the next attempt reads"
    );

    for follower in [&carol, &dave] {
        assert_carried(&pool, follower, &alice, &bob).await;
    }
    for stay in [&erin, &frank] {
        assert_stays(&pool, stay, &alice, &bob).await;
    }
    assert_eq!(
        (
            stored(&pool, alice.id).await.followers_count,
            stored(&pool, bob.id).await.followers_count
        ),
        (2, 2),
        "two followers left the origin, two arrived at the target"
    );

    // The entity: the account's `moved` key is now the target's nested
    // account, one level deep.
    let show = common::TestRequest::get(&format!("/api/v1/accounts/{}", alice.id))
        .send(&app)
        .await
        .json_value();
    assert_eq!(
        show["moved"]["id"],
        json!(bob.id.to_string()),
        "moved renders the target's Account entity"
    );
    assert_eq!(show["moved"]["acct"], json!("bob"), "with its handle");
    assert_eq!(
        show["moved"]["moved"],
        Value::Null,
        "the nesting is non-recursive: the target's own move renders null"
    );
}

/// The gates, in Pleroma's order and with Pleroma's bodies: the wrong
/// password is a 200 error, an unresolvable target the endpoint's own
/// 404, and a target that has not listed the origin refuses with the
/// `alsoKnownAs` message. None of them touches the account.
#[tokio::test]
async fn move_refuses_at_every_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let _bob = aliased_target(&pool, "bob", &alice).await;
    let _stranger = common::create_local_user(&pool, "stranger").await;
    let app = common::test_app(pool.clone());

    let wrong = move_account(&app, &token, "not-my-password", "bob").await;
    assert_eq!(wrong.status, StatusCode::OK, "Pleroma answers 200");
    assert_eq!(
        wrong.json_value(),
        json!({"error": "Invalid password."}),
        "the password refusal"
    );

    let missing = move_account(&app, &token, PASSWORD, UNRESOLVABLE).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "404");
    assert_eq!(
        missing.json_value(),
        json!({"error": "Target account not found."}),
        "the endpoint's own not-found message, period included"
    );

    let unaliased = move_account(&app, &token, PASSWORD, "stranger").await;
    assert_eq!(unaliased.status, StatusCode::OK, "Pleroma answers 200");
    assert_eq!(
        unaliased.json_value(),
        json!({"error": "Target account must have the origin in `alsoKnownAs`"}),
        "the target has not listed the origin"
    );

    let alice = stored(&pool, alice.id).await;
    assert!(
        alice.moved_to_ap_id.is_none() && alice.last_move_at.is_none(),
        "no refusal marked the account moved"
    );
}

/// A second move inside the window is refused with the cooldown message;
/// after 31 days the same account may move again.
#[tokio::test]
async fn move_is_limited_to_once_per_thirty_days() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let first = aliased_target(&pool, "first_target", &alice).await;
    let second = aliased_target(&pool, "second_target", &alice).await;
    let app = common::test_app(pool.clone());

    let ok = move_account(&app, &token, PASSWORD, "first_target").await;
    assert_eq!(
        ok.json_value(),
        json!({"status": "success"}),
        "the first move succeeds"
    );

    let throttled = move_account(&app, &token, PASSWORD, "second_target").await;
    assert_eq!(throttled.status, StatusCode::OK, "Pleroma answers 200");
    assert_eq!(
        throttled.json_value(),
        json!({"error": "You can't move your account more than once in 30 days"}),
        "the cooldown refusal, and it runs after the alias gate"
    );
    assert_eq!(
        stored(&pool, alice.id).await.moved_to_ap_id.as_deref(),
        Some(first.ap_id.as_str()),
        "the refused attempt kept the original target"
    );

    let _aged: sqlx::postgres::PgQueryResult =
        sqlx::query("UPDATE users SET last_move_at = now() - interval '31 days' WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the backdate runs");
    let later = move_account(&app, &token, PASSWORD, "second_target").await;
    assert_eq!(
        later.json_value(),
        json!({"status": "success"}),
        "outside the window the account may move again"
    );
    assert_eq!(
        stored(&pool, alice.id).await.moved_to_ap_id.as_deref(),
        Some(second.ap_id.as_str()),
        "and the target updates"
    );
}

/// Every route requires a user token, `GET` the `read:accounts` scope
/// and the mutations `write:accounts`: no token is the extractor's 401,
/// a narrow or app-only credential one of the 403s, and a dead token
/// the invalid-token 401.
#[tokio::test]
async fn every_route_enforces_auth_and_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let read_only = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let wrong_scope = common::create_app_and_token(&pool, &alice, "read:chats").await;
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let app = common::test_app(pool.clone());

    let anonymous = get_aliases(&app, "").await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "no token");
    assert_eq!(
        anonymous.json_value(),
        json!({"error": "Invalid credentials"}),
        "the AuthUser no-token body"
    );

    let lapsed = common::TestRequest::get(ALIASES)
        .bearer("not-a-stored-token")
        .send(&app)
        .await;
    assert_eq!(lapsed.status, StatusCode::UNAUTHORIZED, "an invalid token");

    let read_gets = get_aliases(&app, &read_only).await;
    assert_eq!(
        read_gets.status,
        StatusCode::OK,
        "read:accounts may list aliases"
    );
    for verb in [
        common::TestRequest::put(ALIASES),
        common::TestRequest::delete(ALIASES),
    ] {
        let denied = verb
            .bearer(&read_only)
            .json(&json!({"alias": "x"}))
            .send(&app)
            .await;
        assert_eq!(
            denied.status,
            StatusCode::FORBIDDEN,
            "read:accounts is not enough to change aliases"
        );
        assert_eq!(
            denied.json_value(),
            json!({"error": "Insufficient permissions: write:accounts."}),
            "the scope denial names the missing grant"
        );
    }

    let wrong = get_aliases(&app, &wrong_scope).await;
    assert_eq!(
        wrong.json_value(),
        json!({"error": "Insufficient permissions: read:accounts."}),
        "read:chats reaches neither read nor write of accounts"
    );
    let moved = move_account(&app, &wrong_scope, PASSWORD, "whoever").await;
    assert_eq!(moved.status, StatusCode::FORBIDDEN, "and the move gate");

    let app_token = get_aliases(&app, &app_only).await;
    assert_eq!(app_token.status, StatusCode::FORBIDDEN, "an app token");
    assert_eq!(
        app_token.json_value(),
        json!({"error": "This API requires an authenticated user"}),
        "is refused the user endpoints"
    );
}
