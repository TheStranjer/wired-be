//! The fixtures, request helpers and constants the admin chat tests share.
//! The test modules are its children, so they reach every helper through
//! `use super::*`.

mod deletes;
mod gates;
mod reads;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    federation::publisher,
    models::{
        chat::{self, ChatMessage, ChatMessageReference, NewChatMessage},
        user::{Flag, NewUser, User},
    },
};

use crate::common;

/// The admin API prefix every admin route here lives under.
const ADMIN: &str = "/api/v1/pleroma/admin";
/// The user-facing message route the two participants use to check what
/// the admin withdrawal hides.
const USER_CHAT_MESSAGES: &str = "/api/v1/pleroma/chats";
/// The 401 body when no token is presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 401 body when a presented token does not resolve.
const INVALID_TOKEN: &str = "The access token is invalid";
/// The 403 body for an app-only (user-unbound) token.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 body for a valid token whose user is not an admin.
const NOT_ADMIN: &str = "User is not an admin.";
/// The 403 body of a token that grants neither `admin:read` nor
/// `admin:read:chats`.
const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The 403 body of a token that grants neither `admin:write` nor
/// `admin:write:chats`.
const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";
/// The 404 body of the unknown nickname, the admin API's router-shaped miss.
const NOT_FOUND: &str = "Not found";
/// The 404 body of the unknown chat and message ids, the record miss.
const RECORD_NOT_FOUND: &str = "Record not found";
/// The admin scope roots every gate-passing token here carries.
const ALL_ADMIN: &str = "admin:read admin:write";
/// Every JSON key a rendered `Chat` entity must carry, sorted.
const CHAT_KEYS: [&str; 6] = [
    "account",
    "id",
    "last_message",
    "pinned",
    "unread",
    "updated_at",
];
/// Every JSON key a rendered `ChatMessage` entity must carry, sorted.
const MESSAGE_KEYS: [&str; 9] = [
    "account_id",
    "attachment",
    "card",
    "chat_id",
    "content",
    "created_at",
    "emojis",
    "id",
    "unread",
];

// --- fixtures -------------------------------------------------------------------------

/// A local admin named `nickname` with a bearer token granting `scopes`.
async fn admin(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// [`chat::add_message`] in a transaction of its own — what an endpoint
/// caller does — answering the message with both sides' references (the
/// third element `None` when `recipient` is remote).
async fn send_as(
    pool: &PgPool,
    sender: &User,
    recipient: &User,
    new_message: &NewChatMessage,
) -> (
    ChatMessage,
    ChatMessageReference,
    Option<ChatMessageReference>,
) {
    let mut tx = pool.begin().await.expect("a transaction begins");
    let sent = chat::add_message(&mut tx, TEST_BASE_URL, sender, recipient, new_message)
        .await
        .expect("the fixture chat message is written");
    tx.commit().await.expect("the transaction commits");
    sent
}

/// A plain-text message from `sender` to LOCAL `recipient`, answered as the
/// message with the SENDER's and the RECIPIENT's own references.
async fn send(
    pool: &PgPool,
    sender: &User,
    recipient: &User,
    text: &str,
) -> (ChatMessage, ChatMessageReference, ChatMessageReference) {
    let (message, sender_reference, recipient_reference) =
        send_as(pool, sender, recipient, &NewChatMessage::text(text)).await;
    (
        message,
        sender_reference,
        recipient_reference.expect("the fixture recipient is local"),
    )
}

/// A chat message from `sender` to `recipient` carrying a local-origin
/// object id — `{TEST_BASE_URL}/objects/{uuid}`, the shape decision 6
/// reserves for this instance's own documents and the only kind the
/// withdrawal hook federates — answered with that id and the sender's
/// reference.
async fn send_local_origin(
    pool: &PgPool,
    sender: &User,
    recipient: &User,
    text: &str,
) -> (String, ChatMessageReference) {
    let ap_id = format!("{TEST_BASE_URL}/objects/{}", Uuid::now_v7());
    let (_, sender_reference, _) = send_as(
        pool,
        sender,
        recipient,
        &NewChatMessage {
            ap_id: Some(ap_id.clone()),
            ..NewChatMessage::text(text)
        },
    )
    .await;
    (ap_id, sender_reference)
}

/// Moves a chat's `updated_at` into the past, so a listing's activity
/// order is settled rather than a race between fixtures (the model's own
/// tests backdate the same way).
async fn backdate_chat(pool: &PgPool, id: Uuid, hours: i64) {
    let _aged = sqlx::query(
        "UPDATE chats SET updated_at = now() - make_interval(hours => $2::int) WHERE id = $1",
    )
    .bind(id)
    .bind(hours)
    .execute(pool)
    .await
    .expect("the stamp is settable");
}

/// The `Link` header value the paged lists answer with for a page whose
/// first and last row carry `newest` and `oldest` as cursors.
fn expected_link(path: &str, newest: &str, oldest: &str) -> String {
    format!(
        "<https://test.example{path}?max_id={oldest}>; rel=\"next\", \
         <https://test.example{path}?min_id={newest}>; rel=\"prev\""
    )
}

/// The `error` string of a JSON error response.
fn error_message(response: &common::TestResponse) -> Option<String> {
    response
        .json_value()
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The rendered object's keys, sorted, for exact key-set assertions.
fn json_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("entities render objects")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// The `id` of every entry of an entity array, in the answered order.
fn entity_ids(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("the lists are arrays")
        .iter()
        .map(|entry| {
            entry["id"]
                .as_str()
                .expect("entities carry id strings")
                .to_owned()
        })
        .collect()
}

/// A GET through the real app.
async fn get(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// A DELETE through the real app.
async fn delete(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::delete(path)
        .bearer(token)
        .send(app)
        .await
}

/// The admin route listing `nickname`'s chats.
fn user_chats_path(nickname: &str) -> String {
    format!("{ADMIN}/users/{nickname}/chats")
}

/// The admin route showing chat `id`.
fn chat_path(id: &str) -> String {
    format!("{ADMIN}/chats/{id}")
}

/// The admin route listing chat `id`'s messages.
fn messages_path(id: &str) -> String {
    format!("{}/messages", chat_path(id))
}

/// The admin route deleting `message_id` out of chat `id`.
fn message_path(id: &str, message_id: &str) -> String {
    format!("{}/{}", messages_path(id), message_id)
}

/// The USER-facing route listing chat owner's `id`'s messages — what the
/// two participants hit to see what the admin withdrawal hid.
fn own_messages_path(id: &Uuid) -> String {
    format!("{USER_CHAT_MESSAGES}/{id}/messages")
}

/// The args of every queued `federator.publish` job, in insertion order.
async fn publish_jobs(pool: &PgPool) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>("SELECT args FROM jobs WHERE kind = $1 ORDER BY inserted_at, id")
        .bind(publisher::KIND_PUBLISH)
        .fetch_all(pool)
        .await
        .expect("the jobs read")
        .into_iter()
        .map(|(args,)| args)
        .collect()
}
