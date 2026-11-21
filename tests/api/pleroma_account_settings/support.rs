//! The shared fixtures of the account settings tests: the routes and
//! secrets, the request builders and the stored-state readers.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    federation::publisher,
    models::{
        follow::{self, FollowState},
        user::{self, Flag, User},
    },
};

use crate::common;

/// Password every login-capable fixture shares.
pub(super) const PASSWORD: &str = "correct horse battery staple";
/// The new password the `change_password` fixtures submit (past the
/// default 8-character minimum).
pub(super) const NEW_PASSWORD: &str = "a-brand-new-secret";
/// The aliases route.
pub(super) const ALIASES: &str = "/api/pleroma/aliases";
/// The move endpoint.
pub(super) const MOVE: &str = "/api/pleroma/move_account";
/// The password-change endpoint.
pub(super) const CHANGE_PASSWORD: &str = "/api/pleroma/change_password";
/// The email-change endpoint.
pub(super) const CHANGE_EMAIL: &str = "/api/pleroma/change_email";
/// The account-deletion endpoint.
pub(super) const DELETE_ACCOUNT: &str = "/api/pleroma/delete_account";
/// The account-disable endpoint.
pub(super) const DISABLE_ACCOUNT: &str = "/api/pleroma/disable_account";
/// A handle `WebFinger` refuses before any request (single-label
/// domain), so the unresolved-target tests stay network-free.
pub(super) const UNRESOLVABLE: &str = "ghost@invalid";
/// An actor id no row of the test database mirrors.
pub(super) const UNSEEN_URL: &str = "https://unseen.example/users/ghost";
/// The job kind `delete_account` queues (mirrors
/// [`wired_be::jobs::deletion::KIND_DELETE`], asserted to agree).
pub(super) const USERS_DELETE: &str = "users.delete";

/// Inserts a local user named `nickname` that can log in with
/// [`PASSWORD`] plus a bearer token granting `scopes`.
pub(super) async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_user_with_password(pool, nickname, PASSWORD).await;
    let minted = common::create_app_and_token(pool, &created, scopes).await;
    (created, minted)
}

/// Inserts a local user named `nickname` that lists `origin`'s actor id
/// among its aliases: the move precondition's satisfied target.
pub(super) async fn aliased_target(pool: &PgPool, nickname: &str, origin: &User) -> User {
    let origin_ap_id = origin.ap_id.clone();
    common::create_local_user_with(pool, nickname, |mut new| {
        new.also_known_as = vec![origin_ap_id];
        new
    })
    .await
}

/// `GET /api/pleroma/aliases` with `token`.
pub(super) async fn get_aliases(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get(ALIASES)
        .bearer(token)
        .send(app)
        .await
}

/// `PUT /api/pleroma/aliases` naming `alias` (as JSON).
pub(super) async fn put_alias(app: &Router, token: &str, alias: &str) -> common::TestResponse {
    common::TestRequest::put(ALIASES)
        .bearer(token)
        .json(&json!({"alias": alias}))
        .send(app)
        .await
}

/// `DELETE /api/pleroma/aliases` naming `alias` (as a form body, the
/// other spelling decision 10 accepts).
pub(super) async fn delete_alias(app: &Router, token: &str, alias: &str) -> common::TestResponse {
    common::TestRequest::delete(ALIASES)
        .bearer(token)
        .form(&[("alias", alias)])
        .send(app)
        .await
}

/// `POST /api/pleroma/move_account` (as a form body).
pub(super) async fn move_account(
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
pub(super) async fn stored(pool: &PgPool, id: Uuid) -> User {
    user::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the fixture row exists")
}

/// `follower` follows `who` through the model's normal follow path.
pub(super) async fn follow_user(pool: &PgPool, follower: &User, who: &User) {
    let _granted = follow::follow(pool, TEST_BASE_URL, follower, who)
        .await
        .expect("the follow fixture is recorded");
}

/// The stored relationship of the pair, which the test expects to exist.
pub(super) async fn relation(pool: &PgPool, follower: &User, target: &User) -> follow::Follow {
    follow::get(pool, follower.id, target.id)
        .await
        .unwrap()
        .expect("the relationship exists")
}

/// Alice's four followers: two ordinary local accounts, one that opted out of
/// having its following moved, and one remote mirror.
pub(super) async fn four_followers(pool: &PgPool, alice: &User) -> (User, User, User, User) {
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
pub(super) async fn assert_carried(pool: &PgPool, follower: &User, origin: &User, target: &User) {
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
pub(super) async fn assert_stays(pool: &PgPool, stay: &User, origin: &User, target: &User) {
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

/// The federation half of a completed move: exactly one `federator.publish`
/// job — the bare fixture's origin has no followers collection column, so
/// the profile `Update` arm stays quiet — carrying the `Move` that names
/// the origin as both actor and object and the destination as target.
pub(super) async fn assert_move_queued(pool: &PgPool, origin: &User, destination: &User) {
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

/// The four credential/departure endpoints take a JSON body; this posts it
/// with `token` as the bearer credential.
pub(super) async fn post_json(
    app: &Router,
    path: &str,
    token: &str,
    body: Value,
) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// The `oauth_tokens` rows owned by user `user_id`.
pub(super) async fn token_count(pool: &PgPool, user_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_tokens WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("the tokens count")
}

/// The `(queue, args)` of every live-or-done job row of `kind`.
pub(super) async fn queued_jobs(pool: &PgPool, kind: &str) -> Vec<(String, Value)> {
    sqlx::query_as::<_, (String, Value)>("SELECT queue, args FROM jobs WHERE kind = $1 ORDER BY id")
        .bind(kind)
        .fetch_all(pool)
        .await
        .expect("the jobs read")
}

/// Whether the given request still authenticates as alice's own view of
/// the account (`verify_credentials` is the cheapest `AuthUser` route).
pub(super) async fn signed_in(app: &Router, token: &str) -> StatusCode {
    common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(token)
        .send(app)
        .await
        .status
}

/// `POST /api/v1/accounts/move` with the JSON body of `to_account` and
/// `password`.
pub(super) async fn move_v1(app: &Router, token: &str, body: Value) -> common::TestResponse {
    post_json(app, "/api/v1/accounts/move", token, body).await
}
