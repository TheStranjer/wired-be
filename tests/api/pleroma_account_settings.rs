//! Account utilities (`/api/pleroma/aliases`, `/api/pleroma/move_account`
//! from WBE-137; `/api/pleroma/change_password`, `/api/pleroma/change_email`,
//! `/api/pleroma/delete_account`, `/api/pleroma/disable_account` from
//! WBE-136) over the real router, plus the Mastodon twin of the move,
//! `POST /api/v1/accounts/move` — same gates in the same order, same
//! `Move`/`Update` federation, answered with the caller's `Relationship`
//! entity and proper 4xx refusals.
//!
//! The alias tests walk the full add → list → remove lifecycle and pin
//! the render rules (handle for a mirrored actor, raw URL otherwise,
//! append-if-absent); the move tests pin Pleroma's gate order (password,
//! target resolution, `alsoKnownAs` precondition, 30-day cooldown), the
//! follower transfer (local accepted followers move, opted-out and
//! remote ones stay), the counters that move with it, the entity's
//! nested non-recursive `moved` render, the `Move` activity the completed
//! move queues for the federation fan-out (a refusal queues nothing), and
//! every route's `AuthUser` + scope pipeline. The credential tests pin the
//! 200-with-error shape of every password/validation refusal, which tokens
//! survive which action, the email-confirmation branch and its queued
//! letter, and the delete endpoint's config gate, `deleted_at` mark and
//! enqueued `users.delete` sweep (the sweep itself is exercised
//! handler-direct in `src/jobs/deletion.rs`).

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    federation::publisher,
    mailer::KIND_SEND,
    models::{
        follow::{self, FollowState},
        status::Visibility,
        user::{self, Flag, User},
    },
    util::password::verify_password,
};

use crate::common;

/// Password every login-capable fixture shares.
const PASSWORD: &str = "correct horse battery staple";
/// The new password the `change_password` fixtures submit (past the
/// default 8-character minimum).
const NEW_PASSWORD: &str = "a-brand-new-secret";
/// The aliases route.
const ALIASES: &str = "/api/pleroma/aliases";
/// The move endpoint.
const MOVE: &str = "/api/pleroma/move_account";
/// The password-change endpoint.
const CHANGE_PASSWORD: &str = "/api/pleroma/change_password";
/// The email-change endpoint.
const CHANGE_EMAIL: &str = "/api/pleroma/change_email";
/// The account-deletion endpoint.
const DELETE_ACCOUNT: &str = "/api/pleroma/delete_account";
/// The account-disable endpoint.
const DISABLE_ACCOUNT: &str = "/api/pleroma/disable_account";
/// A handle `WebFinger` refuses before any request (single-label
/// domain), so the unresolved-target tests stay network-free.
const UNRESOLVABLE: &str = "ghost@invalid";
/// An actor id no row of the test database mirrors.
const UNSEEN_URL: &str = "https://unseen.example/users/ghost";
/// The job kind `delete_account` queues (mirrors
/// [`wired_be::jobs::deletion::KIND_DELETE`], asserted to agree).
const USERS_DELETE: &str = "users.delete";

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

    // The federation half: the move is announced as a `Move`.
    assert_move_queued(&pool, &alice, &bob).await;
}

/// The federation half of a completed move: exactly one `federator.publish`
/// job — the bare fixture's origin has no followers collection column, so
/// the profile `Update` arm stays quiet — carrying the `Move` that names
/// the origin as both actor and object and the destination as target.
async fn assert_move_queued(pool: &PgPool, origin: &User, destination: &User) {
    let announcements = queued_jobs(pool, publisher::KIND_PUBLISH).await;
    assert_eq!(announcements.len(), 1, "the move is announced once");
    let (queue, args) = &announcements[0];
    assert_eq!(
        queue,
        wired_be::jobs::QUEUE_FEDERATOR_OUTGOING,
        "the fan-out rides the outgoing federation queue"
    );
    assert_eq!(
        args["actor_id"],
        origin.id.to_string(),
        "published as the origin"
    );
    let activity = &args["activity"];
    assert_eq!(activity["type"], "Move");
    assert_eq!(activity["actor"], origin.ap_id);
    assert_eq!(activity["object"], origin.ap_id);
    assert_eq!(activity["target"], destination.ap_id);
    assert_eq!(
        activity["to"],
        json!([format!("{}/followers", origin.ap_id)]),
        "the followers collection is the only address"
    );
    assert_eq!(activity["cc"], json!([]));
}

/// A refused move federates nothing: no gate that answers before the
/// write is reached queues a publish job.
#[tokio::test]
async fn a_refused_move_announces_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let _bob = aliased_target(&pool, "bob", &alice).await;
    let app = common::test_app(pool.clone());

    let wrong = move_account(&app, &token, "not-my-password", "bob").await;
    assert_eq!(wrong.status, StatusCode::OK, "the refusal body");
    assert_eq!(
        wrong.json_value(),
        json!({"error": "Invalid password."}),
        "the password refusal"
    );
    assert!(
        queued_jobs(&pool, publisher::KIND_PUBLISH).await.is_empty(),
        "a refused move announces nothing"
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

/// The four credential/departure endpoints take a JSON body; this posts it
/// with `token` as the bearer credential.
async fn post_json(app: &Router, path: &str, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// The `oauth_tokens` rows owned by user `user_id`.
async fn token_count(pool: &PgPool, user_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_tokens WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("the tokens count")
}

/// The `(queue, args)` of every live-or-done job row of `kind`.
async fn queued_jobs(pool: &PgPool, kind: &str) -> Vec<(String, Value)> {
    sqlx::query_as::<_, (String, Value)>("SELECT queue, args FROM jobs WHERE kind = $1 ORDER BY id")
        .bind(kind)
        .fetch_all(pool)
        .await
        .expect("the jobs read")
}

/// Whether the given request still authenticates as alice's own view of
/// the account (`verify_credentials` is the cheapest `AuthUser` route).
async fn signed_in(app: &Router, token: &str) -> StatusCode {
    common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(token)
        .send(app)
        .await
        .status
}

/// `change_password` success: the new secret verifies, the old one does
/// not, the presenting token still authenticates and every other token of
/// the account is gone.
#[tokio::test]
async fn change_password_stores_the_new_secret_and_revokes_the_others() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, current) = fixture(&pool, "alice", "write:accounts read:accounts").await;
    let other = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    let response = post_json(
        &app,
        CHANGE_PASSWORD,
        &current,
        json!({
            "password": PASSWORD,
            "new_password": NEW_PASSWORD,
            "new_password_confirmation": NEW_PASSWORD,
        }),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "the change answers");
    assert_eq!(
        response.json_value(),
        json!({"status": "success"}),
        "the documented success body"
    );

    let hash = stored(&pool, alice.id).await.password_hash.expect("a hash");
    assert!(
        verify_password(NEW_PASSWORD, &hash),
        "the new password verifies"
    );
    assert!(!verify_password(PASSWORD, &hash), "the old one is dead");

    assert_eq!(
        token_count(&pool, alice.id).await,
        1,
        "only the presenting token survives"
    );
    assert_eq!(
        signed_in(&app, &current).await,
        StatusCode::OK,
        "and it still authenticates"
    );
    assert_eq!(
        signed_in(&app, &other).await,
        StatusCode::UNAUTHORIZED,
        "the other credential revoked with the old password"
    );
}

/// The three `change_password` refusals are the 200 error bodies Pleroma
/// renders, and none of them touches the hash or the tokens.
#[tokio::test]
async fn change_password_refusals_are_200_bodies_and_change_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let app = common::test_app(pool.clone());

    let wrong = post_json(
        &app,
        CHANGE_PASSWORD,
        &token,
        json!({
            "password": "not-my-password",
            "new_password": NEW_PASSWORD,
            "new_password_confirmation": NEW_PASSWORD,
        }),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::OK, "Pleroma answers 200");
    assert_eq!(
        wrong.json_value(),
        json!({"error": "Invalid password."}),
        "the password refusal"
    );

    let mismatched = post_json(
        &app,
        CHANGE_PASSWORD,
        &token,
        json!({
            "password": PASSWORD,
            "new_password": NEW_PASSWORD,
            "new_password_confirmation": "typed-it-differently",
        }),
    )
    .await;
    assert_eq!(mismatched.status, StatusCode::OK, "also a 200");
    assert_eq!(
        mismatched.json_value(),
        json!({"error": "New password does not match confirmation."}),
        "the confirmation refusal"
    );

    let short = post_json(
        &app,
        CHANGE_PASSWORD,
        &token,
        json!({
            "password": PASSWORD,
            "new_password": "short",
            "new_password_confirmation": "short",
        }),
    )
    .await;
    assert_eq!(short.status, StatusCode::OK, "also a 200");
    assert_eq!(
        short.json_value(),
        json!({"error": "Password too short."}),
        "the length refusal (below MIN_PASSWORD_LENGTH's default 8)"
    );

    let hash = stored(&pool, alice.id).await.password_hash.expect("a hash");
    assert!(
        verify_password(PASSWORD, &hash),
        "no refusal replaced the stored hash"
    );
    assert_eq!(
        token_count(&pool, alice.id).await,
        1,
        "no token was revoked"
    );
}

/// `change_email` success: a free address lands, trimmed but as given,
/// confirmed (the instance demands no confirmation by default) — and no
/// token goes with it.
#[tokio::test]
async fn change_email_lands_a_free_address() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let app = common::test_app(pool.clone());

    let moved = post_json(
        &app,
        CHANGE_EMAIL,
        &token,
        json!({"password": PASSWORD, "email": "  Alice-New@Example.ORG  "}),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "the change answers");
    assert_eq!(
        moved.json_value(),
        json!({"status": "success"}),
        "the documented success body"
    );
    let alice_now = stored(&pool, alice.id).await;
    assert_eq!(
        alice_now.email.as_deref(),
        Some("Alice-New@Example.ORG"),
        "the trimmed address is stored as given (citext folds the lookups)"
    );
    assert!(
        alice_now.email_confirmed.get(),
        "no confirmation is required by default"
    );
    assert_eq!(
        token_count(&pool, alice.id).await,
        1,
        "an email change revokes nothing"
    );
}

/// The three `change_email` refusals are the endpoint's 200 error bodies
/// — the password's, the format's and the uniqueness' (citext folds the
/// foreign case) — and none of them touches the row.
#[tokio::test]
async fn change_email_refusals_are_200_bodies() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let _bob = common::create_user_with_password(&pool, "bob", PASSWORD).await;
    let app = common::test_app(pool.clone());

    let wrong = post_json(
        &app,
        CHANGE_EMAIL,
        &token,
        json!({"password": "nope", "email": "third@example.org"}),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::OK, "the password refusal is 200");
    assert_eq!(
        wrong.json_value(),
        json!({"error": "Invalid password."}),
        "the password body"
    );

    let malformed = post_json(
        &app,
        CHANGE_EMAIL,
        &token,
        json!({"password": PASSWORD, "email": "plainaddress"}),
    )
    .await;
    assert_eq!(
        malformed.status,
        StatusCode::OK,
        "the format refusal is 200"
    );
    assert_eq!(
        malformed.json_value(),
        json!({"error": "Email has invalid format."}),
        "the format refusal"
    );

    let foreign = post_json(
        &app,
        CHANGE_EMAIL,
        &token,
        json!({"password": PASSWORD, "email": "BOB@example.org"}),
    )
    .await;
    assert_eq!(
        foreign.status,
        StatusCode::OK,
        "the uniqueness refusal is 200"
    );
    assert_eq!(
        foreign.json_value(),
        json!({"error": "Email has already been taken"}),
        "the ticket's wording, and citext catches the foreign case"
    );
    assert_eq!(
        stored(&pool, alice.id).await.email,
        Some("alice@example.org".to_owned()),
        "the refusals changed nothing"
    );
}

/// With `ACCOUNT_ACTIVATION_REQUIRED` on and an SMTP relay configured,
/// the address change lands unconfirmed and queues the confirmation
/// letter for the new address.
#[tokio::test]
async fn change_email_queues_a_confirmation_letter_when_required() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let mut config = common::test_config();
    config.account_activation_required = wired_be::config::Toggle::new(true);
    config.smtp_host = Some("smtp.test.example".to_owned());
    let app = common::test_app_with_config(pool.clone(), config);

    let response = post_json(
        &app,
        CHANGE_EMAIL,
        &token,
        json!({"password": PASSWORD, "email": "alice-new@example.org"}),
    )
    .await;
    assert_eq!(
        response.json_value(),
        json!({"status": "success"}),
        "the change still succeeds"
    );
    let changed = stored(&pool, alice.id).await;
    assert_eq!(
        changed.email.as_deref(),
        Some("alice-new@example.org"),
        "the address landed"
    );
    assert!(
        !changed.email_confirmed.get(),
        "and the account must confirm it"
    );

    let letters = queued_jobs(&pool, KIND_SEND).await;
    assert_eq!(letters.len(), 1, "one mailer job queued");
    assert_eq!(letters[0].0, "mailer", "on the mail queue");
    assert_eq!(
        letters[0].1["to"],
        json!("alice-new@example.org"),
        "to the new address"
    );
    assert!(
        letters[0].1["subject"]
            .as_str()
            .is_some_and(|s| s.contains("account confirmation")),
        "the confirmation subject: {}",
        letters[0].1["subject"]
    );
    assert!(
        letters[0].1["text_body"]
            .as_str()
            .is_some_and(|body| body.contains(&format!("confirm_email/{}", alice.id))),
        "the letter links to the account's confirmation path"
    );
}

/// Confirmation required without a relay still resets the flag — the
/// dropped letter is a log line, not a failure (see `crate::mailer`).
#[tokio::test]
async fn confirmation_resets_the_flag_even_with_no_relay() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let mut config = common::test_config();
    config.account_activation_required = wired_be::config::Toggle::new(true);
    let app = common::test_app_with_config(pool.clone(), config);

    let response = post_json(
        &app,
        CHANGE_EMAIL,
        &token,
        json!({"password": PASSWORD, "email": "alice-new@example.org"}),
    )
    .await;
    assert_eq!(
        response.json_value(),
        json!({"status": "success"}),
        "a disabled mailer never fails the request"
    );
    assert!(
        !stored(&pool, alice.id).await.email_confirmed.get(),
        "the account still awaits confirmation"
    );
    assert!(
        queued_jobs(&pool, KIND_SEND).await.is_empty(),
        "and nothing was queued without SMTP_HOST"
    );
}

/// `delete_account` success: the account goes inactive with `deleted_at`
/// stamped, every token — the presenting one included — dies, and the
/// `users.delete` sweep waits on the background queue.
#[tokio::test]
async fn delete_account_marks_revokes_and_queues_the_sweep() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let _second = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    let response = post_json(&app, DELETE_ACCOUNT, &token, json!({"password": PASSWORD})).await;
    assert_eq!(response.status, StatusCode::OK, "the start answers");
    assert_eq!(
        response.json_value(),
        json!({"status": "success"}),
        "the documented success body"
    );

    let deleted = stored(&pool, alice.id).await;
    assert!(!deleted.is_active.get(), "the account is deactivated");
    assert!(
        deleted.deleted_at.is_some(),
        "and carries the deletion stamp"
    );

    assert_eq!(token_count(&pool, alice.id).await, 0, "every token revoked");
    assert_eq!(
        signed_in(&app, &token).await,
        StatusCode::UNAUTHORIZED,
        "including the one that asked"
    );

    assert_eq!(
        wired_be::jobs::deletion::KIND_DELETE,
        USERS_DELETE,
        "the test's mirror of the job kind"
    );
    assert_eq!(
        queued_jobs(&pool, USERS_DELETE).await,
        vec![(
            "background".to_owned(),
            json!({"user_id": alice.id.to_string()})
        )],
        "the sweep waits on the background queue, keyed to the account"
    );
}

/// The `delete_account` refusals: the wrong password is the 200 error
/// body; on an instance with deletion disabled the endpoint is 403 ahead
/// of even the password gate. Neither marks the account.
#[tokio::test]
async fn delete_account_refusals_leave_the_account_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let app = common::test_app(pool.clone());

    let wrong = post_json(&app, DELETE_ACCOUNT, &token, json!({"password": "nope"})).await;
    assert_eq!(wrong.status, StatusCode::OK, "Pleroma answers 200");
    assert_eq!(
        wrong.json_value(),
        json!({"error": "Invalid password."}),
        "the password refusal"
    );

    let untouched = stored(&pool, alice.id).await;
    assert!(untouched.is_active.get(), "still active");
    assert!(untouched.deleted_at.is_none(), "no deletion stamp");
    assert_eq!(token_count(&pool, alice.id).await, 1, "its token lives");
    assert!(
        queued_jobs(&pool, USERS_DELETE).await.is_empty(),
        "and no sweep was queued"
    );

    let mut closed = common::test_config();
    closed.allow_account_deletion = wired_be::config::Toggle::new(false);
    let closed_app = common::test_app_with_config(pool.clone(), closed);
    let refused = post_json(
        &closed_app,
        DELETE_ACCOUNT,
        &token,
        json!({"password": PASSWORD}),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "the gate is 403");
    assert_eq!(
        refused.json_value(),
        json!({"error": "Account deletion is disabled"}),
        "the ticket's body"
    );
    let wrong_too = post_json(
        &closed_app,
        DELETE_ACCOUNT,
        &token,
        json!({"password": "nope"}),
    )
    .await;
    assert_eq!(
        wrong_too.status,
        StatusCode::FORBIDDEN,
        "and it answers before the password gate"
    );
    assert!(
        stored(&pool, alice.id).await.deleted_at.is_none(),
        "the closed gate marked nothing"
    );
    assert!(
        queued_jobs(&pool, USERS_DELETE).await.is_empty(),
        "or queued anything"
    );
}

/// `disable_account`: inactive, all tokens gone, no `deleted_at` — and
/// the account's statuses stay live (that is the whole difference from
/// deletion). The wrong password refuses with the 200 body, unchanged.
#[tokio::test]
async fn disable_account_deactivates_but_keeps_the_data() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let kept =
        common::create_status(&pool, &alice, "kept through disable", Visibility::Public).await;
    let app = common::test_app(pool.clone());

    let wrong = post_json(&app, DISABLE_ACCOUNT, &token, json!({"password": "nope"})).await;
    assert_eq!(
        wrong.json_value(),
        json!({"error": "Invalid password."}),
        "the password refusal is 200"
    );
    assert!(
        stored(&pool, alice.id).await.is_active.get(),
        "and changed nothing"
    );

    let response = post_json(&app, DISABLE_ACCOUNT, &token, json!({"password": PASSWORD})).await;
    assert_eq!(response.status, StatusCode::OK, "the disable answers");
    assert_eq!(
        response.json_value(),
        json!({"status": "success"}),
        "the documented success body"
    );

    let disabled = stored(&pool, alice.id).await;
    assert!(!disabled.is_active.get(), "the account is deactivated");
    assert!(disabled.deleted_at.is_none(), "but no deletion was started");
    assert_eq!(token_count(&pool, alice.id).await, 0, "every token revoked");
    assert_eq!(
        signed_in(&app, &token).await,
        StatusCode::UNAUTHORIZED,
        "including the presenting one"
    );
    assert!(disabled.email.is_some(), "the email stays on the row");
    assert!(
        disabled.password_hash.is_some(),
        "and so does the password hash: disable is not deletion"
    );
    let status_kept: bool =
        sqlx::query_scalar::<_, bool>("SELECT deleted_at IS NULL FROM statuses WHERE id = $1")
            .bind(kept.id)
            .fetch_one(&pool)
            .await
            .expect("the status reads");
    assert!(status_kept, "disable touches no content");
    assert!(
        queued_jobs(&pool, USERS_DELETE).await.is_empty(),
        "and queues no sweep"
    );
}

/// All four utilities: no token is the extractor's 401, a wrong-scope or
/// app-only credential one of the 403s, a dead token the invalid 401, and
/// a body without the required parameters the extractor's 422.
#[tokio::test]
async fn the_four_utilities_enforce_auth_and_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let read_only = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, "write:accounts").await;
    let (_writer, token) = fixture(&pool, "writer", "write:accounts").await;
    let app = common::test_app(pool.clone());

    // One body that satisfies every endpoint's required parameters (serde
    // ignores the surplus keys), so a gated route answers on the gate —
    // not the extractor's 422 — when the credential is refused.
    let full_body = json!({
        "password": PASSWORD,
        "new_password": NEW_PASSWORD,
        "new_password_confirmation": NEW_PASSWORD,
        "email": "someone@example.org",
    });

    for path in [
        CHANGE_PASSWORD,
        CHANGE_EMAIL,
        DELETE_ACCOUNT,
        DISABLE_ACCOUNT,
    ] {
        let anonymous = post_json(&app, path, "", full_body.clone()).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "{path}: no token"
        );
        assert_eq!(
            anonymous.json_value(),
            json!({"error": "Invalid credentials"}),
            "{path}: the AuthUser no-token body"
        );

        let dead = post_json(&app, path, "not-a-stored-token", full_body.clone()).await;
        assert_eq!(
            dead.status,
            StatusCode::UNAUTHORIZED,
            "{path}: a dead token"
        );

        let unscoped = post_json(&app, path, &read_only, full_body.clone()).await;
        assert_eq!(
            unscoped.status,
            StatusCode::FORBIDDEN,
            "{path}: read:accounts"
        );
        assert_eq!(
            unscoped.json_value(),
            json!({"error": "Insufficient permissions: write:accounts."}),
            "{path}: the scope denial"
        );

        let no_user = post_json(&app, path, &app_only, full_body.clone()).await;
        assert_eq!(no_user.status, StatusCode::FORBIDDEN, "{path}: app-only");

        let short_body = post_json(&app, path, &token, json!({})).await;
        assert_eq!(
            short_body.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path}: a body without the required parameters"
        );
    }
}

// --- `POST /api/v1/accounts/move` ----------------------------------------------------------

/// `POST /api/v1/accounts/move` with the JSON body of `to_account` and
/// `password`.
async fn move_v1(app: &Router, token: &str, body: Value) -> common::TestResponse {
    post_json(app, "/api/v1/accounts/move", token, body).await
}

/// The Mastodon twin moves the same account the Pleroma route moves: the
/// caller's `Relationship` to the destination comes back as the 200 body
/// (where Pleroma answers `{"status": "success"}`), the account is marked
/// moved, and the same `Move` lands on the federation queue.
#[tokio::test]
async fn the_mastodon_move_route_moves_the_account_and_queues_the_move() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let bob = aliased_target(&pool, "bob", &alice).await;
    let app = common::test_app(pool.clone());

    let moved = move_v1(
        &app,
        &token,
        json!({"to_account": bob.id.to_string(), "password": PASSWORD}),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "the move succeeds");
    let relationship = moved.json_value();
    assert_eq!(
        relationship["id"],
        json!(bob.id.to_string()),
        "the answer is the relationship entity, not the Pleroma body"
    );
    assert_eq!(
        relationship["following"],
        json!(false),
        "alice holds no follow of her own destination"
    );
    assert!(
        stored(&pool, alice.id).await.last_move_at.is_some(),
        "the account wears the move and its cooldown stamp"
    );
    assert_move_queued(&pool, &alice, &bob).await;
}

/// The twin's refusals are proper statuses, not Pleroma's 200 bodies, in
/// the same gate order: the password first, then the target (404 "Target
/// account not found."), then the alias precondition, then the cooldown —
/// and the 401/403/422 pipeline of the extractor.
#[tokio::test]
async fn the_mastodon_move_route_refuses_in_the_gate_order() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let bob = aliased_target(&pool, "bob", &alice).await;
    let stranger = common::create_local_user(&pool, "stranger").await;
    let (_other, read_only) = fixture(&pool, "reader", "read:accounts").await;
    let app = common::test_app(pool.clone());
    let bob_id = bob.id.to_string();

    let wrong_password = move_v1(
        &app,
        &token,
        json!({"to_account": bob_id, "password": "not-my-password"}),
    )
    .await;
    common::assert_answer(
        &wrong_password,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the password",
    );
    common::assert_error(&wrong_password, "Invalid password.", "the password");

    let unknown = move_v1(
        &app,
        &token,
        json!({"to_account": Uuid::now_v7().to_string(), "password": PASSWORD}),
    )
    .await;
    common::assert_answer(&unknown, StatusCode::NOT_FOUND, "an unknown destination");
    common::assert_error(
        &unknown,
        "Target account not found.",
        "an unknown destination",
    );
    let malformed = move_v1(
        &app,
        &token,
        json!({"to_account": "not-a-uuid", "password": PASSWORD}),
    )
    .await;
    common::assert_answer(&malformed, StatusCode::NOT_FOUND, "a malformed id");

    let no_alias = move_v1(
        &app,
        &token,
        json!({"to_account": stranger.id.to_string(), "password": PASSWORD}),
    )
    .await;
    common::assert_answer(
        &no_alias,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the alias gate",
    );
    common::assert_error(
        &no_alias,
        "Target account must have the origin in `alsoKnownAs`",
        "the alias gate",
    );

    let dead = move_v1(
        &app,
        "not-a-stored-token",
        json!({"to_account": bob_id, "password": PASSWORD}),
    )
    .await;
    common::assert_answer(&dead, StatusCode::UNAUTHORIZED, "a dead token");
    let unscoped = move_v1(
        &app,
        &read_only,
        json!({"to_account": bob_id, "password": PASSWORD}),
    )
    .await;
    common::assert_answer(
        &unscoped,
        StatusCode::FORBIDDEN,
        "read:accounts cannot move",
    );
    let short = move_v1(&app, &token, json!({})).await;
    common::assert_answer(&short, StatusCode::UNPROCESSABLE_ENTITY, "a bare body");
    assert!(
        queued_jobs(&pool, publisher::KIND_PUBLISH).await.is_empty(),
        "and no refusal announced a move"
    );
}

/// After the twin moves an account, the cooldown bites the PLEROMA route
/// too — one `last_move_at`, shared gates: a second move inside the window
/// answers Pleroma's 200-body refusal.
#[tokio::test]
async fn the_mastodon_move_route_stands_in_the_shared_cooldown() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let first = aliased_target(&pool, "first_target", &alice).await;
    let _second = aliased_target(&pool, "second_target", &alice).await;
    let app = common::test_app(pool.clone());

    let moved = move_v1(
        &app,
        &token,
        json!({"to_account": first.id.to_string(), "password": PASSWORD}),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "the twin moves first");
    let throttled = move_account(&app, &token, PASSWORD, "second_target").await;
    assert_eq!(
        throttled.json_value(),
        json!({"error": "You can't move your account more than once in 30 days"}),
        "the Pleroma route reads the same cooldown stamp"
    );
    assert_eq!(
        stored(&pool, alice.id).await.moved_to_ap_id.as_deref(),
        Some(first.ap_id.as_str()),
        "the refused attempt kept the original target"
    );
}
