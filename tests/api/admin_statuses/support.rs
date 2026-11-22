//! The fixtures, request helpers and answer readers the admin statuses tests
//! share. The test modules are its children, so they reach every helper
//! through `use super::*`.

mod ledger;
mod pages_and_gates;
mod writes;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    federation::publisher,
    models::{
        status::{self, NewStatus, Status as StatusRow, Visibility},
        user::{Flag, NewUser, User},
    },
};

use crate::common;

/// The admin API prefix the status routes live under.
const ADMIN: &str = "/api/v1/pleroma/admin";
/// The whole-network ledger route, and the prefix of its `{id}` twin.
const LEDGER: &str = "/api/v1/pleroma/admin/statuses";
/// The 401 body when no token is presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body for a valid token whose user is not an admin.
const NOT_ADMIN: &str = "User is not an admin.";
/// The 403 body for an app-only (user-unbound) token.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 body of a token that grants neither `admin:read` nor
/// `admin:read:statuses`.
const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The 403 body of a token that grants neither `admin:write` nor
/// `admin:write:statuses`.
const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";
/// The 404 body of every miss on these routes, per the ticket.
const NOT_FOUND: &str = "Not found";
/// The 422 body of a `visibility` off the five the column holds.
const INVALID_VISIBILITY: &str = "Invalid visibility";
/// Both area scopes, for a caller that reads and writes alike.
const BOTH: &str = "admin:read:statuses admin:write:statuses";
/// The `ActivityStreams` public address a public note carries in `to`.
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

// --- fixtures -------------------------------------------------------------------------

/// A local user named `nickname` whose account is an admin, with a bearer
/// token granting `scopes`.
async fn admin(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` whose account is a moderator, with a
/// bearer token granting `scopes`.
async fn moderator(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` with no staff role, with a bearer token
/// granting `scopes`.
async fn regular(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// `poster`'s status of `visibility`, straight through the model.
async fn post(pool: &PgPool, poster: &User, visibility: Visibility) -> StatusRow {
    let new = NewStatus {
        visibility,
        ..NewStatus::local(poster, &format!("a {} post", visibility.as_str()))
    };
    status::insert(pool, TEST_BASE_URL, &new)
        .await
        .expect("the status fixture inserts")
}

/// `poster`'s five visibilities, oldest seeded first: the whole ladder in
/// one account's hands, so a filter's answer is a slice of it.
async fn ladder(pool: &PgPool, poster: &User) -> Vec<StatusRow> {
    let mut rows = Vec::new();
    for visibility in [
        Visibility::Public,
        Visibility::Unlisted,
        Visibility::Private,
        Visibility::Direct,
        Visibility::Local,
    ] {
        rows.push(post(pool, poster, visibility).await);
    }
    rows
}

/// `reblogger`'s public boost of `original`.
async fn boost(pool: &PgPool, reblogger: &User, original: &StatusRow) -> StatusRow {
    status::create_reblog(pool, TEST_BASE_URL, reblogger, original, Visibility::Public)
        .await
        .expect("the reblog fixture writes")
        .0
}

/// `quoter`'s public quote of `original`.
async fn quote(pool: &PgPool, quoter: &User, original: &StatusRow) -> StatusRow {
    let new = NewStatus {
        quote_of_id: Some(original.id),
        ..NewStatus::local(quoter, "quoting this")
    };
    status::insert(pool, TEST_BASE_URL, &new)
        .await
        .expect("the quote fixture inserts")
}

/// A mirror of a post from `host`, authored by that host's `author`: the
/// shape decision 6 stores for a received activity — `local` clear, the
/// remote's own `uri`, and the author's `domain` what the instance listing
/// keys on.
async fn mirror_of(pool: &PgPool, author: &User, host: &str, slug: &str) -> StatusRow {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            uri: Some(format!("https://{host}/objects/{slug}")),
            user_id: author.id,
            content: format!("a {host} post"),
            raw_text: format!("a {host} post"),
            ..NewStatus::default()
        },
    )
    .await
    .expect("the mirrored fixture inserts")
}

// --- request helpers -------------------------------------------------------------------

/// The `GET` of `path` as `token`, asserted 200 and answered as JSON.
async fn listed(app: &Router, path: &str, token: &str) -> Value {
    let response = get_with(app, path, token).await;
    common::assert_answer(&response, StatusCode::OK, &format!("the read of {path}"));
    response.json_value()
}

/// One `GET` of `path` with `token`, answered whole.
async fn get_with(app: &Router, path: &str, token: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// The `GET` of `path` with no token at all.
async fn anonymous(app: &Router, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).send(app).await
}

/// The `PUT` of `body` on status `id`, asserted 200 and answered as JSON.
async fn rescoped(app: &Router, token: &str, id: Uuid, body: Value) -> Value {
    let response = common::TestRequest::put(&format!("{LEDGER}/{id}"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the staff scope write");
    response.json_value()
}

/// The `PUT` of `body` on status `id`, answered whole for the refusals.
async fn refused_put(app: &Router, token: &str, id: Uuid, body: Value) -> common::TestResponse {
    common::TestRequest::put(&format!("{LEDGER}/{id}"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// The `DELETE` of the admin route for status `id`.
async fn removed(app: &Router, token: &str, id: Uuid) -> common::TestResponse {
    common::TestRequest::delete(&format!("{LEDGER}/{id}"))
        .bearer(token)
        .send(app)
        .await
}

// --- answer readers --------------------------------------------------------------------

/// The ids of an answered array, in the answered order.
fn ids_of(answer: &Value) -> Vec<String> {
    id_list(
        answer
            .as_array()
            .expect("the ledger answers an array of entities"),
    )
}

/// The ids of an envelope's `activities` array, in the answered order.
fn activity_ids(answer: &Value) -> Vec<String> {
    id_list(
        answer["activities"]
            .as_array()
            .expect("the page answers an activities array"),
    )
}

/// The `id` text of every entity of `entries`, in order.
fn id_list(entries: &[Value]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| {
            entry["id"]
                .as_str()
                .expect("entities carry id text")
                .to_owned()
        })
        .collect()
}

/// The ids of `rows` in the order a listing answers them: the fixtures seed
/// oldest first and `UUIDv7` ids ascend with creation (decision 4), so the
/// newest-first page is this slice reversed.
fn newest(rows: &[StatusRow]) -> Vec<String> {
    rows.iter().rev().map(|row| row.id.to_string()).collect()
}

/// The moderation-log rows of `action` — actor and sentence — oldest first.
async fn log_lines(pool: &PgPool, action: &str) -> Vec<(String, String)> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT actor_nickname, message FROM moderation_log WHERE action = $1 ORDER BY id",
    )
    .bind(action)
    .fetch_all(pool)
    .await
    .expect("the log reads")
}

/// The activities the publisher's queue holds, in insertion order.
async fn published(pool: &PgPool) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>(
        "SELECT args->'activity' FROM jobs WHERE kind = $1 ORDER BY inserted_at, id",
    )
    .bind(publisher::KIND_PUBLISH)
    .fetch_all(pool)
    .await
    .expect("the publish jobs read")
    .into_iter()
    .map(|(activity,)| activity)
    .collect()
}

/// The stored row behind `id`, tombstone included.
async fn stored(pool: &PgPool, id: Uuid) -> StatusRow {
    status::get_by_id_including_deleted(pool, id)
        .await
        .expect("the row reads")
        .expect("the fixture row is stored")
}
