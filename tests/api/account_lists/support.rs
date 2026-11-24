//! Fixtures and request helpers the account listing tests share.

use axum::{Router, http::StatusCode};
use chrono::{NaiveDate, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow,
        user::{Flag, User},
    },
};

use crate::common;

/// The instance origin the fixtures live on (`Config::for_tests`).
pub(super) const BASE: &str = TEST_BASE_URL;
/// The 401 body `AuthUser` renders when no token was presented at all.
pub(super) const NO_TOKEN: &str = "Invalid credentials";
/// The 401 body for a token that does not resolve.
pub(super) const INVALID_TOKEN: &str = "The access token is invalid";
/// The 403 body an app-only token gets from the user-requiring extractor.
pub(super) const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 body of a listing whose token lacks the `read` root scope.
pub(super) const NO_READ: &str = "Insufficient permissions: read.";
/// The 403 body of a read whose token lacks `read:accounts`.
pub(super) const NO_READ_ACCOUNTS: &str = "Insufficient permissions: read:accounts.";
/// The 403 body of a read whose token lacks `read:follows`.
pub(super) const NO_READ_FOLLOWS: &str = "Insufficient permissions: read:follows.";
/// The 403 body of the dismissal whose token lacks `write:accounts`.
pub(super) const NO_WRITE_ACCOUNTS: &str = "Insufficient permissions: write:accounts.";
/// The 404 body the dismissal route answers for an account it cannot find.
pub(super) const NOT_FOUND: &str = "Record not found";
/// The 404 body of a path no area claims.
pub(super) const NOT_IMPLEMENTED: &str = "Not implemented";
/// The 400 body of an unreadable month.
pub(super) const BAD_MONTH: &str = "Invalid parameter: month";
/// The 400 body of an unreadable day.
pub(super) const BAD_DAY: &str = "Invalid parameter: day";
/// The familiar-followers batch path, without its parameters.
pub(super) const FAMILIAR_PATH: &str = "/api/v1/accounts/familiar_followers";
/// Pleroma's birthday list path.
pub(super) const BIRTHDAYS_PATH: &str = "/api/v1/pleroma/birthdays";

/// A local user named `nickname` plus a bearer token granting `scopes`.
pub(super) async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` with `birthday` set to `year`/`month`/`day`
/// and shown to strangers — the account the birthday list is built from.
pub(super) async fn born(pool: &PgPool, nickname: &str, year: i32, month: u32, day: u32) -> User {
    common::create_local_user_with(pool, nickname, move |mut new| {
        new.birthday = NaiveDate::from_ymd_opt(year, month, day);
        new.show_birthday = Flag::new(true);
        new
    })
    .await
}

/// A discoverable local user named `nickname` whose newest status was posted
/// `days_ago` days before now (`None`: never posted).
pub(super) async fn discoverable(pool: &PgPool, nickname: &str, days_ago: Option<i64>) -> User {
    common::create_local_user_with(pool, nickname, move |mut new| {
        new.discoverable = Flag::new(true);
        new.last_status_at = days_ago.map(|days| Utc::now() - chrono::Duration::days(days));
        new
    })
    .await
}

/// A local user named `nickname` an admin put in the suggestion pool.
pub(super) async fn suggested(pool: &PgPool, nickname: &str) -> User {
    common::create_local_user_with(pool, nickname, |mut new| {
        new.is_suggested = Flag::new(true);
        new
    })
    .await
}

/// A `GET` of `path` with `token` (`None` anonymous).
pub(super) async fn get(app: &Router, token: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// A `DELETE` of `path` with `token`.
pub(super) async fn delete(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::delete(path)
        .bearer(token)
        .send(app)
        .await
}

/// The `account.id` of every v2 suggestion entry, in order.
pub(super) fn wrapped_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the v2 listing is an array")
        .iter()
        .map(|entry| {
            entry["account"]["id"]
                .as_str()
                .expect("a wrapped account id")
                .to_owned()
        })
        .collect()
}

/// The ids of `users`, newest first: the order `ORDER BY id DESC` answers,
/// which for `UUIDv7`s minted inside one millisecond is their byte order and
/// not their insert order (decision 4).
pub(super) fn newest_first(ids: impl IntoIterator<Item = Uuid>) -> Vec<String> {
    let mut names: Vec<String> = ids.into_iter().map(|id| id.to_string()).collect();
    names.sort_by(|left, right| right.cmp(left));
    names
}

/// Records `follower`'s accepted follow of the open local account `target`.
pub(super) async fn seed_follow(pool: &PgPool, follower: &User, target: &User) {
    let row = follow::follow(pool, BASE, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(
        row.state,
        follow::FollowState::Accepted,
        "an open local account answers on the spot"
    );
}

/// Deactivates `target`, the state a closed account is in.
pub(super) async fn deactivate(pool: &PgPool, target: &User) {
    let gone = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(target.id)
        .execute(pool)
        .await
        .expect("the fixture deactivates");
    assert_eq!(gone.rows_affected(), 1, "the row was there to close");
}

/// Opt `target` into discovery, the profile setting the directory reads and
/// no fixture endpoint in this suite writes.
pub(super) async fn make_discoverable(pool: &PgPool, target: &User) {
    let changed = sqlx::query("UPDATE users SET discoverable = true WHERE id = $1")
        .bind(target.id)
        .execute(pool)
        .await
        .expect("the fixture opts the account into discovery");
    assert_eq!(changed.rows_affected(), 1, "the row was there to flag");
}

/// Moves `target`'s `inserted_at` back `days` days, so an `order=new`
/// expectation does not depend on how fast the fixture loop ran.
pub(super) async fn backdate(pool: &PgPool, target: &User, days: i64) {
    let moved = sqlx::query(
        "UPDATE users SET inserted_at = now() - $1::bigint * interval '1 day' WHERE id = $2",
    )
    .bind(days)
    .bind(target.id)
    .execute(pool)
    .await
    .expect("the fixture backdates the account");
    assert_eq!(moved.rows_affected(), 1, "the row was there to move");
}

/// The directory's answer to `query`, asserted to be a plain 200.
pub(super) async fn directory(app: &Router, query: &str) -> Value {
    let answer = get(app, None, &format!("/api/v1/directory{query}")).await;
    common::assert_answer(&answer, StatusCode::OK, "the anonymous directory");
    answer.json_value()
}

/// The batch path asking about every id in `ids`, spelled the way Mastodon
/// clients spell `id[]`.
pub(super) fn familiar_path(ids: &[String]) -> String {
    let joined: Vec<String> = ids.iter().map(|id| format!("id%5B%5D={id}")).collect();
    format!("{FAMILIAR_PATH}?{}", joined.join("&"))
}
