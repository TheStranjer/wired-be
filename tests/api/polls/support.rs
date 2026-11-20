//! Shared consts and fixture helpers for this area's submodules: the
//! routes and 422 bodies the create-side tests assert, and the thin
//! wrappers around the HTTP harness and the model reads they share.
//! Everything here is `pub(super)`, and a submodule imports exactly the
//! members it calls.

use axum::{Router, http::StatusCode};
use chrono::{TimeDelta, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        media_attachment::{self, NewMediaAttachment},
        poll::{self, Poll},
        status,
        user::User,
    },
};

use crate::common;

/// The route under test.
pub(super) const STATUSES: &str = "/api/v1/statuses";
/// The scope the create endpoint asks a user token for.
pub(super) const WRITE: &str = "write:statuses";
/// The 401 body `AuthUser` renders when the request carries no token at all.
pub(super) const NO_TOKEN: &str = "Invalid credentials";
/// 422 body for a poll with fewer than two options.
pub(super) const TOO_FEW: &str = "Poll must contain at least 2 options";
/// 422 body for a poll past `MAX_POLL_OPTIONS` (the stock 20).
pub(super) const TOO_MANY: &str = "Poll can't contain more than 20 options";
/// 422 body for an empty option or one past `MAX_POLL_OPTION_CHARS`.
pub(super) const BROKEN_OPTION: &str = "Poll options cannot be longer than 200 characters";
/// 422 body for `poll[expires_in]` past `MAX_POLL_EXPIRATION`.
pub(super) const TOO_FAR: &str = "Expiration date is too far in the future";
/// 422 body for `poll[expires_in]` under `MIN_POLL_EXPIRATION`.
pub(super) const TOO_SOON: &str = "Expiration date is too soon";
/// 422 body for a poll sent beside non-empty `media_ids`.
pub(super) const WITH_MEDIA: &str = "Polls cannot have attachments";
/// 422 body for a textless post with no poll (or anything else) to carry it.
pub(super) const EMPTY_STATUS: &str = "Cannot post an empty status without attachments";

/// A local user named `nickname` plus a bearer token granting `scopes`.
pub(super) async fn user_with_token(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// An unposted upload by `author` — a `media_ids` entry a create may claim.
pub(super) async fn upload(pool: &PgPool, author: &User) -> Uuid {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(author.id),
            status_id: None,
            url: format!("{TEST_BASE_URL}/media/fixture.png"),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("the fixture upload inserts")
    .id
}

/// `POST /api/v1/statuses` with the JSON `body` as `token`.
pub(super) async fn posted(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// A creation asserted 200 and answered as JSON.
pub(super) async fn accepted(app: &Router, token: &str, body: Value) -> Value {
    let response = posted(app, token, body).await;
    common::assert_answer(&response, StatusCode::OK, "a body with a valid poll");
    response.json_value()
}

/// A refusal asserted 422 with exactly `message` as its `{"error": …}`.
pub(super) async fn refused(app: &Router, token: &str, body: Value, message: &str, who: &str) {
    let response = posted(app, token, body).await;
    common::assert_answer(&response, StatusCode::UNPROCESSABLE_ENTITY, who);
    common::assert_error(&response, message, who);
}

/// The stored row behind a creation's answer.
pub(super) async fn created_row(pool: &PgPool, value: &Value) -> status::Status {
    let text = value["id"].as_str().expect("the entity carries its id");
    let id = Uuid::parse_str(text).expect("the entity id is an id");
    status::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the created status is stored live")
}

/// How many statuses the database holds: what a refusal never adds to.
pub(super) async fn status_rows(pool: &PgPool) -> i64 {
    sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM statuses")
        .fetch_one(pool)
        .await
        .expect("the status count reads")
        .0
}

/// How many `polls` rows the database holds: one per stored question,
/// none for a plain create, a refused body or a preview.
pub(super) async fn poll_rows(pool: &PgPool) -> i64 {
    sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM polls")
        .fetch_one(pool)
        .await
        .expect("the poll count reads")
        .0
}

/// How many `poll_options` rows the database holds, whatever poll they
/// belong to.
pub(super) async fn option_rows(pool: &PgPool) -> i64 {
    sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM poll_options")
        .fetch_one(pool)
        .await
        .expect("the option count reads")
        .0
}

/// The stored `polls` row behind a creation's answer, required to exist —
/// the row the answer's `poll` entity renders from.
pub(super) async fn stored_poll(pool: &PgPool, value: &Value) -> Poll {
    let row = created_row(pool, value).await;
    poll::get_for_status(pool, row.id)
        .await
        .expect("the poll reads")
        .expect("the create stored the poll beside its status")
}

/// The stored options of `poll_id`, as `(position, title, votes_count)`
/// in the order the model reads them back.
pub(super) async fn stored_options(pool: &PgPool, poll_id: Uuid) -> Vec<(i32, String, i32)> {
    poll::options(pool, poll_id)
        .await
        .expect("the options read")
        .into_iter()
        .map(|option| (option.position, option.title, option.votes_count))
        .collect()
}

/// Assert the poll row's deadline sits within a few seconds of `secs`
/// after `posted_at` — the instant counted at check time, immediately
/// before the row was written.
pub(super) fn assert_deadline(stand: &Poll, posted_at: chrono::DateTime<Utc>, secs: i64) {
    let due = stand
        .expires_at
        .unwrap_or_else(|| panic!("the create stored the {secs}-second deadline"));
    let span = due - posted_at;
    assert!(
        (TimeDelta::seconds(secs) - span).num_seconds().abs() <= 5,
        "the poll closes about {secs} seconds out, not {span}"
    );
}

/// The minimum valid poll's options, as JSON.
pub(super) fn two_options() -> Value {
    json!(["yes", "no"])
}

/// `n` distinct numbered option texts, as JSON.
pub(super) fn numbered_options(n: usize) -> Value {
    json!(
        (0..n)
            .map(|index| format!("option {index}"))
            .collect::<Vec<String>>()
    )
}
