//! Shared consts, fixtures and helpers for every submodule of the
//! `notifications_v2` area: the listing path and scopes the tests use, the
//! thin wrappers around the HTTP harness, the readers of the listing and
//! the unread count, the marker and seen fixtures, the seeds the grouping,
//! paging and gate tests start from, and the grouped-route callers.
//! Everything here is `pub(super)`, and a submodule imports exactly the
//! members it calls.

use axum::{Router, http::StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    marker,
    notification::Notification,
    status::{self, Visibility},
    user::User,
};

use crate::common;

/// The grouped listing's path, shared by every request and the `Link`
/// header's target.
pub(super) const LIST: &str = "/api/v2/notifications";
/// The scope every grouped read demands.
pub(super) const READ: &str = "read:notifications";
/// The scope the dismissal demands.
pub(super) const WRITE: &str = "write:notifications";
/// The message of the shared `AppError::NotFound` body.
pub(super) const NOT_FOUND: &str = "Record not found";

/// A local user with a `scopes`-granted bearer token: `(user, token)`.
pub(super) async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, scopes).await;
    (user, token)
}

/// A `favourite` notification of `recipient` by `actor` about `status`.
pub(super) async fn favourite(
    pool: &PgPool,
    recipient: &User,
    actor: &User,
    status: &status::Status,
) -> Notification {
    common::create_notification(pool, recipient, actor, "favourite", Some(status)).await
}

/// A favourite whose row the test does not need.
pub(super) async fn favourite_note(
    pool: &PgPool,
    recipient: &User,
    actor: &User,
    status: &status::Status,
) {
    let _row = favourite(pool, recipient, actor, status).await;
}

/// The `GET {LIST}?query` body, asserted to be a 200, parsed.
pub(super) async fn list(app: &Router, token: &str, query: &str) -> Value {
    let response = common::TestRequest::get(&format!("{LIST}{query}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, &format!("the listing `{query}`"));
    response.json_value()
}

/// The groups of a listing body, as values.
pub(super) fn groups(body: &Value) -> Vec<&Value> {
    body["notification_groups"]
        .as_array()
        .expect("notification_groups is a list")
        .iter()
        .collect()
}

/// The ids of a JSON array of strings, as strings.
pub(super) fn id_list(values: &Value) -> Vec<String> {
    values
        .as_array()
        .expect("an id list")
        .iter()
        .map(|value| value.as_str().expect("ids are text").to_owned())
        .collect()
}

/// The `id` of each entity in a JSON array of expanded accounts or
/// statuses, as strings.
pub(super) fn expanded_ids(items: &Value) -> Vec<String> {
    items
        .as_array()
        .expect("an entity list")
        .iter()
        .map(|item| {
            item["id"]
                .as_str()
                .expect("entities carry id text")
                .to_owned()
        })
        .collect()
}

/// The `notification_groups` keys of a body, in answer order.
pub(super) fn group_keys(body: &Value) -> Vec<String> {
    groups(body)
        .into_iter()
        .map(|group| group["group_key"].as_str().expect("a group key").to_owned())
        .collect()
}

/// Percent-encode a group key the way clients put it into a path.
pub(super) fn encoded(key: &str) -> String {
    url::form_urlencoded::byte_serialize(key.as_bytes()).collect()
}

/// The shared seed of the two merged-favourites tests: three favourites of
/// one status (`one`, by bob, carol and dave) and one of another (`two`, by
/// bob).
#[derive(Debug)]
pub(super) struct Merged {
    /// Alice's `read:notifications` token.
    pub(super) token: String,
    /// The thrice-favourited status.
    pub(super) one: status::Status,
    /// The once-favourited status.
    pub(super) two: status::Status,
    /// The actor of the first and last favourites.
    pub(super) bob: User,
    /// The actor of the middle favourite of `one`.
    pub(super) carol: User,
    /// The actor of the newest favourite of `one`.
    pub(super) dave: User,
    /// The oldest notification: bob's first favourite of `one`.
    pub(super) first: Notification,
    /// The newest favourite of `one`: dave's.
    pub(super) third: Notification,
    /// The only favourite of `two`: bob's, and the newest row overall.
    pub(super) alone: Notification,
}

/// Seeds [`Merged`] into `pool`.
pub(super) async fn seed_merged_favourites(pool: &PgPool) -> Merged {
    let (alice, token) = fixture(pool, "alice", READ).await;
    let erin = common::create_local_user(pool, "erin").await;
    let bob = common::create_local_user(pool, "bob").await;
    let carol = common::create_local_user(pool, "carol").await;
    let dave = common::create_local_user(pool, "dave").await;
    let one = common::create_status(pool, &erin, "first post", Visibility::Public).await;
    let two = common::create_status(pool, &erin, "second post", Visibility::Public).await;
    let first = favourite(pool, &alice, &bob, &one).await;
    favourite_note(pool, &alice, &carol, &one).await;
    let third = favourite(pool, &alice, &dave, &one).await;
    let alone = favourite(pool, &alice, &bob, &two).await;
    Merged {
        token,
        one,
        two,
        bob,
        carol,
        dave,
        first,
        third,
        alone,
    }
}

/// `GET {LIST}/unread_count?query` answers its `count`.
pub(super) async fn unread(app: &Router, token: &str, query: &str) -> i64 {
    let response = common::TestRequest::get(&format!("{LIST}/unread_count{query}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the unread count");
    response.json_value()["count"]
        .as_i64()
        .expect("count is a number")
}

/// Stores `alice`'s `notifications` marker at `last_read_id`.
pub(super) async fn set_marker(pool: &PgPool, alice: &User, last_read_id: &str) {
    let _rows: Vec<marker::Marker> =
        marker::upsert_many(pool, alice.id, vec![(marker::NOTIFICATIONS, last_read_id)])
            .await
            .expect("the marker fixture upserts");
}

/// Flags one notification seen, the way a later mark-seen endpoint will.
pub(super) async fn mark_seen(pool: &PgPool, id: Uuid) {
    let _rows = sqlx::query("UPDATE notifications SET seen = true WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("the seen flag lands");
}

/// Seed one `favourite` notification of `alice` and return its group key.
pub(super) async fn seed_one_favourite(pool: &PgPool, alice: &User) -> String {
    let erin = common::create_local_user(pool, "erin").await;
    let bob = common::create_local_user(pool, "bob").await;
    let post = common::create_status(pool, &erin, "gated", Visibility::Public).await;
    favourite_note(pool, alice, &bob, &post).await;
    format!("favourite-{}", post.id)
}

/// The five grouped routes as `(is_post, path)` pairs, the four reads
/// first and the dismissal last.
pub(super) fn grouped_routes(key: &str) -> Vec<(bool, String)> {
    let key = encoded(key);
    vec![
        (false, LIST.to_owned()),
        (false, format!("{LIST}/unread_count")),
        (false, format!("{LIST}/{key}")),
        (false, format!("{LIST}/{key}/accounts")),
        (true, format!("{LIST}/{key}/dismiss")),
    ]
}

/// Send `route` with the bearer `token`; empty `token` sends no credential.
pub(super) async fn call(
    app: &Router,
    route: &(bool, String),
    token: &str,
) -> common::TestResponse {
    let builder = if route.0 {
        common::TestRequest::post(&route.1)
    } else {
        common::TestRequest::get(&route.1)
    };
    if token.is_empty() {
        builder.send(app).await
    } else {
        builder.bearer(token).send(app).await
    }
}

/// Call every grouped route with `token` (empty: unauthenticated).
pub(super) async fn call_every_route(
    app: &Router,
    token: &str,
    key: &str,
) -> Vec<common::TestResponse> {
    let mut responses = Vec::new();
    for route in grouped_routes(key) {
        responses.push(call(app, &route, token).await);
    }
    responses
}

/// Call the four grouped reads (everything but the dismissal) with `token`.
pub(super) async fn call_the_reads(
    app: &Router,
    token: &str,
    key: &str,
) -> Vec<common::TestResponse> {
    let mut responses = Vec::new();
    for route in grouped_routes(key).into_iter().take(4) {
        responses.push(call(app, &route, token).await);
    }
    responses
}
