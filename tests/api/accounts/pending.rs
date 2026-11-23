//! Accounts awaiting staff approval or email confirmation are invisible to
//! strangers on every account route (Pleroma's `User.visible_for/2`): the
//! account itself and staff still see them, an active account is unchanged.

use axum::{Router, http::StatusCode};
use sqlx::PgPool;
use wired_be::{
    api::helpers::visible_to,
    models::user::{Flag, User},
};

use super::support::{NOT_FOUND, show};
use crate::common;

/// The scopes every viewer token here carries: reads and the follow write.
const SCOPES: &str = "read write follow";

/// An account whose registration waits on a moderator.
async fn approval_pending(pool: &PgPool) -> User {
    common::create_local_user_with(pool, "spam", |mut new| {
        new.approval_pending = Flag::new(true);
        new
    })
    .await
}

/// An account whose email address is not confirmed yet.
async fn unconfirmed(pool: &PgPool) -> User {
    common::create_local_user_with(pool, "spam", |mut new| {
        new.email_confirmed = Flag::new(false);
        new
    })
    .await
}

/// `GET` `path` as `token` (anonymous when `None`).
async fn get_as(app: &Router, path: &str, token: Option<&str>) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(token) => request.bearer(token).send(app).await,
        None => request.send(app).await,
    }
}

/// `POST` `path` as `token`.
async fn post_as(app: &Router, path: &str, token: &str) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .send(app)
        .await
}

/// Every read route answers `expected` for `target` to `token`.
async fn assert_reads(app: &Router, target: &User, token: Option<&str>, expected: StatusCode) {
    let id = target.id;
    let nick = &target.nickname;
    for path in [
        format!("/api/v1/accounts/{id}"),
        format!("/api/v1/accounts/lookup?acct={nick}"),
        format!("/api/v1/accounts/{id}/statuses"),
        format!("/api/v1/accounts/{id}/followers"),
        format!("/api/v1/accounts/{id}/following"),
    ] {
        let response = get_as(app, &path, token).await;
        assert_eq!(response.status, expected, "GET {path} as {token:?}");
        if expected == StatusCode::NOT_FOUND {
            assert_eq!(response.text(), NOT_FOUND, "GET {path} is the shared miss");
        }
    }
}

/// Every write route aimed at `target` is the shared 404 for `token`.
async fn assert_writes_hidden(app: &Router, target: &User, token: &str) {
    let id = target.id;
    for action in ["follow", "unfollow", "remove_from_followers"] {
        let path = format!("/api/v1/accounts/{id}/{action}");
        let response = post_as(app, &path, token).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "POST {path}");
    }
    let note = common::TestRequest::post(&format!("/api/v1/accounts/{id}/note"))
        .bearer(token)
        .json(&serde_json::json!({"comment": "hi"}))
        .send(app)
        .await;
    assert_eq!(note.status, StatusCode::NOT_FOUND, "POST note");
}

/// Strangers (anonymous and other users) get 404 on a pending account;
/// an admin and a moderator get 200.
async fn assert_hidden_from_strangers_not_staff(pool: PgPool, target: User) {
    let other = common::create_local_user(&pool, "other").await;
    let other_token = common::create_app_and_token(&pool, &other, SCOPES).await;
    let admin = common::create_local_user_with(&pool, "boss", |mut new| {
        new.is_admin = Flag::new(true);
        new
    })
    .await;
    let admin_token = common::create_app_and_token(&pool, &admin, SCOPES).await;
    let moderator = common::create_local_user_with(&pool, "mod", |mut new| {
        new.is_moderator = Flag::new(true);
        new
    })
    .await;
    let moderator_token = common::create_app_and_token(&pool, &moderator, SCOPES).await;
    let app = common::test_app(pool);

    assert_reads(&app, &target, None, StatusCode::NOT_FOUND).await;
    assert_reads(&app, &target, Some(&other_token), StatusCode::NOT_FOUND).await;
    assert_writes_hidden(&app, &target, &other_token).await;
    assert_reads(&app, &target, Some(&admin_token), StatusCode::OK).await;
    assert_reads(&app, &target, Some(&moderator_token), StatusCode::OK).await;
}

/// An account awaiting staff approval is a 404 to strangers, 200 to staff.
#[tokio::test]
async fn an_approval_pending_account_is_hidden_from_strangers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let target = approval_pending(&pool).await;
    assert_hidden_from_strangers_not_staff(pool, target).await;
}

/// An account awaiting email confirmation is a 404 to strangers, 200 to staff.
#[tokio::test]
async fn an_unconfirmed_account_is_hidden_from_strangers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let target = unconfirmed(&pool).await;
    assert_hidden_from_strangers_not_staff(pool, target).await;
}

/// An unconfirmed account's own token is valid, so it sees itself.
#[tokio::test]
async fn an_unconfirmed_account_sees_itself() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let target = unconfirmed(&pool).await;
    let token = common::create_app_and_token(&pool, &target, SCOPES).await;
    let app = common::test_app(pool);
    assert_reads(&app, &target, Some(&token), StatusCode::OK).await;
}

/// An approval-pending account's own bearer is refused before any route
/// runs (see `auth`), so its self-visibility is the predicate's.
#[tokio::test]
async fn visible_to_lets_a_pending_account_see_itself() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let target = approval_pending(&pool).await;
    let other = common::create_local_user(&pool, "other").await;
    assert!(visible_to(&target, Some(&target)), "the account itself");
    assert!(!visible_to(&target, Some(&other)), "another user");
    assert!(!visible_to(&target, None), "an anonymous viewer");
}

/// A deactivated account stays hidden even from staff.
#[tokio::test]
async fn a_deactivated_account_stays_hidden_from_staff() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let ghost = common::create_local_user_with(&pool, "ghost", |mut new| {
        new.is_active = Flag::new(false);
        new
    })
    .await;
    let admin = common::create_local_user_with(&pool, "boss", |mut new| {
        new.is_admin = Flag::new(true);
        new
    })
    .await;
    let token = common::create_app_and_token(&pool, &admin, SCOPES).await;
    let app = common::test_app(pool);
    let response = get_as(
        &app,
        &format!("/api/v1/accounts/{}", ghost.id),
        Some(&token),
    )
    .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND, "deactivated");
}

/// An active account is unchanged: visible to everyone and followable.
#[tokio::test]
async fn an_active_account_is_unchanged() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let target = common::create_local_user(&pool, "alice").await;
    let other = common::create_local_user(&pool, "other").await;
    let token = common::create_app_and_token(&pool, &other, SCOPES).await;
    let app = common::test_app(pool);
    assert_reads(&app, &target, None, StatusCode::OK).await;
    assert_reads(&app, &target, Some(&token), StatusCode::OK).await;
    let follow = post_as(
        &app,
        &format!("/api/v1/accounts/{}/follow", target.id),
        &token,
    )
    .await;
    assert_eq!(follow.status, StatusCode::OK, "following an active account");
    let anonymous = show(&app, &target.id.to_string()).await;
    assert_eq!(anonymous.status, StatusCode::OK, "show stays public");
}

/// `lookup?resolve=true` does not leak a pending local account either.
#[tokio::test]
async fn resolving_lookup_does_not_reveal_a_pending_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let target = approval_pending(&pool).await;
    let other = common::create_local_user(&pool, "other").await;
    let token = common::create_app_and_token(&pool, &other, SCOPES).await;
    let app = common::test_app(pool);
    let response = get_as(
        &app,
        &format!(
            "/api/v1/accounts/lookup?acct={}&resolve=true",
            target.nickname
        ),
        Some(&token),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "resolve is no bypass"
    );
}
