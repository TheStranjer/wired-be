//! Fixtures and asserts the three message-route children share: the
//! posters, the chat-owner lookup, the expiry back-date and the answer and
//! refusal asserts over a list of responses.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    chat::{self, ChatFilter},
    user::User,
};

use super::support::{all_page, delete_as, message_delete_path, messages_path};
use crate::common;

/// The status `response` answers, with its body for the message.
pub(super) fn assert_answer(response: &common::TestResponse, status: StatusCode, who: &str) {
    assert_eq!(response.status, status, "{who}: {}", response.text());
}

/// The `error` body `response` carries.
pub(super) fn assert_says(response: &common::TestResponse, message: &str, who: &str) {
    assert_eq!(
        response.error_field().as_deref(),
        Some(message),
        "the refusal wording for {who}"
    );
}

/// The status every response in `refused` answers, for `who`.
pub(super) fn assert_all_answer(refused: &[common::TestResponse], status: StatusCode, who: &str) {
    for response in refused {
        assert_answer(response, status, who);
    }
}

/// The `error` body every response in `refused` carries, for `who`.
pub(super) fn assert_all_say(refused: &[common::TestResponse], message: &str, who: &str) {
    for response in refused {
        assert_says(response, message, who);
    }
}

/// The 404 a delete of `reference` in chat `chat` answers — `who` is what the
/// assertion names, and the wording is the shared "Record not found".
pub(super) async fn assert_delete_404(
    app: &Router,
    token: &str,
    chat: &str,
    reference: &str,
    who: &str,
) {
    let refused = delete_as(app, token, &message_delete_path(chat, reference)).await;
    assert_eq!(
        refused.status,
        StatusCode::NOT_FOUND,
        "{who}: {}",
        refused.text()
    );
    assert_eq!(
        refused.error_field().as_deref(),
        Some("Record not found"),
        "the plain 404 wording for {who}"
    );
}

/// `POST`s a chat message with `content` (and `key`, when the client sent
/// one) as `token`: the endpoint's usual JSON body, spelled as text.
pub(super) async fn post_text(
    app: &Router,
    token: &str,
    chat: &str,
    content: &str,
    key: Option<&str>,
) -> common::TestResponse {
    post_message(app, token, chat, json!({"content": content}), key).await
}

/// `POST` the message body `body` into chat `chat` as `token`, with the
/// `Idempotency-Key` header when `key` names one.
pub(super) async fn post_message(
    app: &Router,
    token: &str,
    chat: &str,
    body: Value,
    key: Option<&str>,
) -> common::TestResponse {
    let mut request = common::TestRequest::post(&messages_path(chat))
        .bearer(token)
        .json(&body);
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    request.send(app).await
}

/// The id of `user_id`'s most recently active chat, read through the
/// model's list: the posting fixtures write one chat, this walks it back
/// out for the other side's token to name.
pub(super) async fn own_chat_id(pool: &PgPool, user_id: Uuid) -> Uuid {
    let rows = chat::page_for_user(pool, user_id, all_page(), ChatFilter::default())
        .await
        .expect("the list reads");
    rows.first().expect("the user has a chat").id
}

/// A local [`User`] with a token granting both chat scopes — the fixture
/// that posts and then lists what it posted.
pub(super) async fn poster(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, "read:chats write:chats").await;
    (created, token)
}

/// Moves the message behind reference `ref_id` outside the idempotency
/// replay window.
pub(super) async fn expire_message(pool: &PgPool, ref_id: Uuid) {
    let _moved: sqlx::postgres::PgQueryResult = sqlx::query(
        "UPDATE chat_messages SET inserted_at = now() - interval '2 hours' \
         WHERE id = (SELECT message_id FROM chat_message_references WHERE id = $1)",
    )
    .bind(ref_id)
    .execute(pool)
    .await
    .expect("the backdate runs");
}
