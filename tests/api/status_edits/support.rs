//! The fixtures and request helpers the status edit tests share. The test
//! modules are its children, so they reach every helper through
//! `use super::*`.

mod edits;
mod history_source_gates;
mod media;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        media_attachment::{self, NewMediaAttachment},
        status::{self, NewStatus, Status as StatusRow, Visibility},
        status_edit,
        user::User,
    },
    streaming::hub::{Audience, RenderSpec},
    util::time::mastodon_time,
};

use crate::common;

/// The 404 body of the shared status gate, and the 403 body of the edit's
/// and the source's ownership refusals.
const NOT_FOUND: &str = "Record not found";
/// The 401 body `AuthUser` sends when no usable token was presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body for an app-only token on a route that needs a user.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 401 body for a token that does not resolve.
const BAD_TOKEN: &str = "The access token is invalid";
/// The 403 wording of the edit gate: it asks `write:statuses`.
const NO_WRITE: &str = "Insufficient permissions: write:statuses.";
/// The 403 wording of the source and history gates: they ask
/// `read:statuses`.
const NO_READ: &str = "Insufficient permissions: read:statuses.";
/// The 422 body of `PUT` for a reblog row.
const CANNOT_EDIT: &str = "Cannot edit a reblog";
/// The 422 bodies the edit shares with the create.
const EMPTY_STATUS: &str = "Cannot post an empty status without attachments";
const BAD_CONTENT_TYPE: &str = "Invalid content_type";
const OVER_LIMIT: &str = "The status is over the character limit";
const SPOILER_TOO_LONG: &str =
    "Validation failed: Spoiler text is too long (maximum is 200 characters)";
/// An id that names no stored status.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

/// A local user named `nickname` with a `write:statuses read:statuses`
/// bearer token: enough to edit a post and read it, its history and its
/// source back through the API.
async fn editor(pool: &PgPool, nickname: &str) -> (User, String) {
    scoped(pool, nickname, "write:statuses read:statuses").await
}

/// A local user named `nickname` with exactly `scopes`, for the gate tests
/// that must *lack* one of the two.
async fn scoped(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A public post by `author`, through the model.
async fn post(pool: &PgPool, author: &User, text: &str) -> StatusRow {
    common::create_status(pool, author, text, Visibility::Public).await
}

/// A public post by `author` at `visibility`.
async fn post_with(pool: &PgPool, author: &User, text: &str, visibility: Visibility) -> StatusRow {
    common::create_status(pool, author, text, visibility).await
}

/// A public post through `POST /api/v1/statuses`, as the create route wrote
/// it — the fixture for the tests that read a creation-built post back
/// through the edit and the source route.
async fn posted(app: &Router, token: &str, body: &Value) -> Value {
    let response = common::TestRequest::post("/api/v1/statuses")
        .bearer(token)
        .json(body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the create accepts the body");
    response.json_value()
}

/// A mirror of a remote post, by a fresh remote account: the shape
/// federation stores, and the row no edit of ours may touch.
async fn remote_post(pool: &PgPool, nickname: &str, uri: &str) -> StatusRow {
    let author = common::create_remote_user(pool, nickname, "remote.example").await;
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            uri: Some(uri.to_owned()),
            user_id: author.id,
            content: "<p>from elsewhere</p>".to_owned(),
            raw_text: "from elsewhere".to_owned(),
            local: false,
            ..NewStatus::default()
        },
    )
    .await
    .expect("the fixture remote post inserts")
}

/// An unposted upload by `author` — a `media_ids` entry an edit may claim.
async fn upload(pool: &PgPool, author: &User) -> Uuid {
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

/// `attachment` already on `status_row` at `position`.
async fn attach(pool: &PgPool, author: &User, status_row: &StatusRow, position: i32) -> Uuid {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(author.id),
            status_id: Some(status_row.id),
            position,
            url: format!("{TEST_BASE_URL}/media/fixture.png"),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("the fixture attachment inserts")
    .id
}

/// The status's attachment ids as stored, in render order.
async fn stored_media(pool: &PgPool, status_id: Uuid) -> Vec<Uuid> {
    let rows = media_attachment::for_statuses(pool, &[status_id])
        .await
        .expect("the attachment rows read");
    rows.get(&status_id).map_or_else(Vec::new, |found| {
        found.iter().map(|attachment| attachment.id).collect()
    })
}

/// The superseded versions of `status_id`, straight from the table.
async fn revisions(pool: &PgPool, status_id: Uuid) -> Vec<status_edit::StatusEdit> {
    status_edit::list_for_status(pool, status_id)
        .await
        .expect("the revisions read")
}

/// `PUT /api/v1/statuses/{id}` with a JSON body, the id as text so the miss
/// probes can feed any spelling as the path segment.
async fn edit(app: &Router, token: &str, id: &str, body: Value) -> common::TestResponse {
    common::TestRequest::put(&format!("/api/v1/statuses/{id}"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// [`edit`] keyed by the stored row, the usual call.
async fn edit_row(app: &Router, token: &str, row: &StatusRow, body: Value) -> common::TestResponse {
    edit(app, token, &row.id.to_string(), body).await
}

/// `GET /api/v1/statuses/{id}/history` as `token` (`None`: anonymous).
async fn history(app: &Router, token: Option<&str>, id: &str) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("/api/v1/statuses/{id}/history"));
    match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    }
}

/// `GET /api/v1/statuses/{id}/source` as `token`.
async fn source(app: &Router, token: &str, id: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/api/v1/statuses/{id}/source"))
        .bearer(token)
        .send(app)
        .await
}

/// The `content` of every entry of a history array.
fn contents_of(body: &Value) -> Vec<&str> {
    body.as_array()
        .expect("the history is an array")
        .iter()
        .map(|entry| entry["content"].as_str().expect("a content string"))
        .collect()
}
