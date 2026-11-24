//! Shared consts and fixture helpers for every submodule of the `statuses`
//! area: the canned error bodies and scopes the route tests assert, the thin
//! wrappers around the HTTP harness, and the model-level fixtures the tests
//! seed through `wired_be::models` directly. Everything here is `pub(super)`,
//! and a submodule imports exactly the members it calls; a helper only one
//! submodule uses lives there instead.

use axum::{Router, http::StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, FollowState},
        hashtag,
        media_attachment::{self, NewMediaAttachment},
        mention,
        status::{self, NewStatus, Status, Visibility},
        user::{self, NewUser, User},
    },
};

use crate::common;

/// The body every miss on these routes carries (decision 8).
pub(super) const NOT_FOUND: &str = "Record not found";
/// The 403 body the scope gate renders for a user token without the scope.
pub(super) const NO_SCOPE: &str = "Insufficient permissions: read:statuses.";
/// The 401 body `MaybeAuthUser` renders for a token that does not resolve.
pub(super) const INVALID_TOKEN: &str = "The access token is invalid";
/// The scope both read routes ask a presented user token for.
pub(super) const SCOPES: &str = "read:statuses";
/// The scope the write routes — `POST /api/v1/statuses` and the delete — ask
/// a user token for.
pub(super) const WRITE: &str = "write:statuses";
/// [`WRITE`] beside `read:statuses`: what a delete test's token carries when
/// the test also reads the status back through the show route.
pub(super) const WRITE_AND_READ: &str = "write:statuses read:statuses";
/// The 403 body the scope gate renders for a user token without [`WRITE`].
pub(super) const NO_WRITE: &str = "Insufficient permissions: write:statuses.";
/// The 401 body `AuthUser` renders when the request carries no token at all.
pub(super) const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body `AuthUser` renders for a token bound to no user.
pub(super) const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The id of a status that is not stored here, for the miss probes.
pub(super) const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

/// `GET /api/v1/statuses/{id}` as `token` (`None`: anonymous). The id stays
/// text so the miss probes can feed any spelling as the path segment.
pub(super) async fn show(app: &Router, token: Option<&str>, id: &str) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("/api/v1/statuses/{id}"));
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `GET /api/v1/statuses?{query}` as `token` (`None`: anonymous). An empty
/// `query` is the route with no parameters at all.
pub(super) async fn batch(app: &Router, token: Option<&str>, query: &str) -> common::TestResponse {
    let path = if query.is_empty() {
        "/api/v1/statuses".to_owned()
    } else {
        format!("/api/v1/statuses?{query}")
    };
    let request = common::TestRequest::get(&path);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `GET …?{key}[]=…` for the batch, one param per id, in the order given.
/// Generic over the id's text form so both `&str` and `String` ids join a
/// query (the tests hold `Uuid`s they stringify inline and generated ids
/// they already own).
pub(super) fn id_query<S: AsRef<str>>(key: &str, ids: &[S]) -> String {
    ids.iter()
        .map(|id| format!("{key}[]={}", id.as_ref()))
        .collect::<Vec<String>>()
        .join("&")
}

/// The ids of `statuses` as the wire spells them, for an expectation.
pub(super) fn ids_of(statuses: &[&Status]) -> Vec<String> {
    statuses
        .iter()
        .map(|status| status.id.to_string())
        .collect()
}

/// A local user named `nickname` plus a bearer token granting `scopes`.
pub(super) async fn user_with_token(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// Inserts `author`'s `direct` status addressed to `mentioned`.
pub(super) async fn direct_to(pool: &PgPool, author: &User, mentioned: &User) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility: Visibility::Direct,
            mentioned_user_ids: vec![mentioned.id],
            ..NewStatus::local(author, "psst")
        },
    )
    .await
    .expect("a direct status with a mention inserts")
}

/// Inserts `reblogger`'s public reblog of `original`.
pub(super) async fn reblog_of(pool: &PgPool, reblogger: &User, original: &Status) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            reblog_of_id: Some(original.id),
            ..NewStatus::local(reblogger, "")
        },
    )
    .await
    .expect("a reblog inserts")
}

/// Records `follower`'s follow of `target` and asserts it landed accepted
/// (both fixtures are open local accounts) — the relation rule 5 grants.
pub(super) async fn followed(pool: &PgPool, follower: &User, target: &User) {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(
        row.state,
        FollowState::Accepted,
        "an open local account accepts on the spot"
    );
}

/// Turns `doomed` into a tombstone, asserting the delete hit the row.
pub(super) async fn deleted(pool: &PgPool, doomed: &Status) {
    let tombstone = status::soft_delete(pool, doomed.id)
        .await
        .expect("the fixture status soft-deletes")
        .expect("the fixture status was live");
    assert!(tombstone.deleted_at.is_some(), "and it is a tombstone now");
}

/// `DELETE /api/v1/statuses/{id}` as `token`. The id stays text so the miss
/// probes can feed any spelling as the path segment.
pub(super) async fn destroy(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::delete(&format!("/api/v1/statuses/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// `POST /api/v1/statuses/{id}/favourite` as `token`, for the test that shows
/// the two writes spend one bucket.
pub(super) async fn favourite(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/statuses/{id}/favourite"))
        .bearer(token)
        .send(app)
        .await
}

/// A local user named `nickname` with a `write:statuses` bearer token (and
/// `read:statuses`, so a test can read the status back through the show
/// route): the caller the delete route answers.
pub(super) async fn writer(pool: &PgPool, nickname: &str) -> (User, String) {
    user_with_token(pool, nickname, WRITE_AND_READ).await
}

/// A local user named `nickname` whose `write:statuses` token belongs to an
/// account `customize` has made staff (Pleroma's `messages_delete` privilege).
pub(super) async fn staff_with_token<F>(
    pool: &PgPool,
    nickname: &str,
    customize: F,
) -> (User, String)
where
    F: FnOnce(NewUser) -> NewUser,
{
    let created = common::create_local_user_with(pool, nickname, customize).await;
    let token = common::create_app_and_token(pool, &created, WRITE_AND_READ).await;
    (created, token)
}

/// The stored row behind `id`, tombstone included: what a delete leaves.
pub(super) async fn stored(pool: &PgPool, id: Uuid) -> Status {
    status::get_by_id_including_deleted(pool, id)
        .await
        .expect("the row reads")
        .expect("the fixture status is stored")
}

/// The live-post count stored for `author`, for the counter assertions.
pub(super) async fn statuses_count(pool: &PgPool, author: &User) -> i32 {
    user::get_by_id(pool, author.id)
        .await
        .expect("the account reads")
        .expect("the fixture account exists")
        .statuses_count
}

/// One media attachment of `author` posted on `status_id`, as its row id: the
/// row a status delete leaves behind.
pub(super) async fn attach(pool: &PgPool, author: &User, status_id: Uuid) -> Uuid {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(author.id),
            status_id: Some(status_id),
            url: format!("{TEST_BASE_URL}/media/fixture.png"),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("a fixture attachment inserts")
    .id
}

/// `GET /api/v1/statuses/{id}/context` as `token` (`None`: anonymous). The id
/// stays text so the miss probes can feed any spelling as the path segment.
pub(super) async fn context(app: &Router, token: Option<&str>, id: &str) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("/api/v1/statuses/{id}/context"));
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// The `id` values of one arm of a context answer, in response order.
pub(super) fn context_ids(body: &Value, arm: &str) -> Vec<String> {
    body[arm]
        .as_array()
        .unwrap_or_else(|| panic!("the `{arm}` arm is an array"))
        .iter()
        .map(|status| status["id"].as_str().expect("a status id").to_owned())
        .collect()
}

/// The route every creation test posts to.
pub(super) const STATUSES: &str = "/api/v1/statuses";
/// 422 body for a post with nothing to say (Pleroma's
/// `validate_character_limit`).
pub(super) const EMPTY_STATUS: &str = "Cannot post an empty status without attachments";
/// 422 body for a `visibility` outside the five names.
pub(super) const BAD_VISIBILITY: &str = "Invalid visibility";
/// 422 body for a `content_type` outside the three advertised formats.
pub(super) const BAD_CONTENT_TYPE: &str = "Invalid content_type";

/// `POST /api/v1/statuses` with the JSON `body` as `token` (`None`: no
/// credential at all).
pub(super) async fn publish(
    app: &Router,
    token: Option<&str>,
    body: Value,
) -> common::TestResponse {
    let request = common::TestRequest::post(STATUSES).json(&body);
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// A creation, asserted 200, answered as JSON.
pub(super) async fn posted(app: &Router, token: &str, body: Value) -> Value {
    let response = publish(app, Some(token), body).await;
    common::assert_answer(&response, StatusCode::OK, "a post that passes");
    response.json_value()
}

/// The id a creation answered with, as the row's own `UUIDv7` text.
pub(super) fn id_of(value: &Value) -> Uuid {
    let text = value["id"].as_str().expect("the entity carries its id");
    Uuid::parse_str(text).expect("the entity id is an id")
}

/// The stored row behind a creation's answer. A refusal never reaches this:
/// no answer, no id, nothing stored.
pub(super) async fn created_row(pool: &PgPool, value: &Value) -> Status {
    status::get_by_id(pool, id_of(value))
        .await
        .unwrap()
        .expect("the created status is stored live")
}

/// How many statuses this database holds: what a refusal never adds to and
/// every post adds exactly one to.
pub(super) async fn stored_count(pool: &PgPool) -> i64 {
    sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM statuses")
        .fetch_one(pool)
        .await
        .expect("the count reads")
        .0
}

/// The account `id` as it stands now, for the counters a post moves.
pub(super) async fn author_now(pool: &PgPool, id: Uuid) -> User {
    user::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the author exists")
}

/// The `status_mentions` links of one status, as the mention model reads
/// them, in position order.
pub(super) async fn mention_links(pool: &PgPool, id: Uuid) -> Vec<Uuid> {
    let links = mention::for_statuses(pool, std::slice::from_ref(&id))
        .await
        .expect("the mention links read");
    links
        .get(&id)
        .map_or_default(|users| users.iter().map(|account| account.id).collect())
}

/// The names `status_hashtags` links to one status, as the hashtag model
/// reads them, in name order.
pub(super) async fn tag_links(pool: &PgPool, id: Uuid) -> Vec<String> {
    let links = hashtag::for_statuses(pool, std::slice::from_ref(&id))
        .await
        .expect("the tag links read");
    links
        .get(&id)
        .map_or_default(|tags| tags.iter().map(|tag| tag.name.clone()).collect())
}

/// A refusal: the same post, asserted 422 with `message`, then dropped.
pub(super) async fn refused(app: &Router, token: &str, body: Value, message: &str) {
    let response = publish(app, Some(token), body).await;
    common::assert_answer(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a refused post",
    );
    common::assert_error(&response, message, "the refusal wording");
}

/// The row-count helpers above stop at `statuses`; the key claims live in
/// their own table, and every keyed test reads this to see one claim per
/// pair, no claim for a preview, and one for a key that moved.
pub(super) async fn key_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM idempotency_keys")
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// `POST /api/v1/statuses` with the JSON `body` as `token`, carrying an
/// `Idempotency-Key` header.
pub(super) async fn publish_keyed(
    app: &Router,
    token: &str,
    key: &str,
    body: Value,
) -> common::TestResponse {
    common::TestRequest::post(STATUSES)
        .json(&body)
        .bearer(token)
        .header("idempotency-key", key)
        .send(app)
        .await
}

/// Inserts `author`'s quote of `original` at `visibility`, straight through
/// the model — the harness's rule for quote fixtures — and returns its row.
pub(super) async fn quote_of(
    pool: &PgPool,
    author: &User,
    original: &Status,
    visibility: Visibility,
) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility,
            quote_of_id: Some(original.id),
            ..NewStatus::local(author, "quoting this")
        },
    )
    .await
    .expect("a quote fixture inserts")
}
