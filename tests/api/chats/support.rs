//! Shared consts and fixtures the chats submodules draw on: the routes and
//! scopes, the entity key sets, the user and thread fixtures, the request
//! verbs and the readers more than one of them needs.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        chat::{self, ChatMessageReference, NewChatMessage},
        status::Visibility,
        user::User,
    },
    pagination::{MAX_LIMIT, PageParams},
    util::id::new_id,
};

use crate::common;

/// The chats root all v1 routes live under.
pub(super) const ROOT: &str = "/api/v1/pleroma/chats";
/// The v2 chats root, the paged sibling of [`ROOT`].
pub(super) const V2_ROOT: &str = "/api/v2/pleroma/chats";
/// The scope Pleroma's `OAuthScopesPlug` pipeline demands for the writing actions.
pub(super) const SCOPE: &str = "write:chats";
/// The scope the same pipeline demands for the reading actions.
pub(super) const READ_SCOPE: &str = "read:chats";
/// The whole list or page in one fetch.
pub(super) const ALL: &str = "limit=40";
/// Every JSON key a rendered `Chat` entity must carry, sorted.
pub(super) const CHAT_KEYS: [&str; 6] = [
    "account",
    "id",
    "last_message",
    "pinned",
    "unread",
    "updated_at",
];
/// Every JSON key a rendered `ChatMessage` entity must carry, sorted.
pub(super) const MESSAGE_KEYS: [&str; 9] = [
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

/// Id text back to an id, panicking only on fixture bugs.
pub(super) fn parse(text: &str) -> Uuid {
    text.parse().expect("the fixture ids are id text")
}

/// The whole list in one page, the way the model's tests spell it.
pub(super) fn all_page() -> PageParams {
    PageParams::from_query(Some(ALL), MAX_LIMIT).expect("the page parses")
}

/// A local [`User`] named `nickname` plus a bearer token granting `scopes`.
pub(super) async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// Sends `text` from `sender` to `recipient` through the model (what the
/// chat-posting ticket's endpoint will do) and returns the message with both
/// sides' references.
pub(super) async fn send(
    pool: &PgPool,
    sender: &User,
    recipient: &User,
    text: &str,
) -> (
    chat::ChatMessage,
    ChatMessageReference,
    ChatMessageReference,
) {
    let mut conn = pool.begin().await.expect("a transaction begins");
    let sent = chat::add_message(
        &mut conn,
        TEST_BASE_URL,
        sender,
        recipient,
        &NewChatMessage::text(text),
    )
    .await
    .expect("a plain chat message is written");
    conn.commit().await.expect("the transaction commits");
    (sent.0, sent.1, sent.2.expect("the recipient is local"))
}

/// The `alice → bob` fixture the read tests start from: alice has sent bob
/// three messages, so bob's chat of alice holds three unread references.
pub(super) struct Thread {
    /// The writer, with a token granting `read`.
    pub(super) alice: User,
    /// The reader and chat owner.
    pub(super) bob: User,
    /// The token bob's routes are called with.
    pub(super) token: String,
    /// bob's own chat row of alice.
    pub(super) chat: chat::Chat,
    /// The ids of bob's three references, oldest first.
    pub(super) refs: Vec<Uuid>,
}

impl Thread {
    /// bob's chat id as the hyphenated text routes speak it.
    pub(super) fn chat_id(&self) -> String {
        self.chat.id.to_string()
    }
}

pub(super) async fn thread(pool: &PgPool) -> Thread {
    let (alice, _) = fixture(pool, "alice", "read").await;
    let (bob, token) = fixture(pool, "bob", SCOPE).await;
    let mut references = Vec::new();
    let mut chat_id = None;
    for text in ["one", "two", "three"] {
        let (_, _, bob_ref) = send(pool, &alice, &bob, text).await;
        references.push(bob_ref.id);
        chat_id = Some(bob_ref.chat_id);
    }
    references.sort_unstable();
    let own = chat::get_owned(pool, bob.id, chat_id.expect("three messages wrote a chat"))
        .await
        .expect("the chat read works")
        .expect("bob owns the chat his references sit in");
    Thread {
        alice,
        bob,
        token,
        chat: own,
        refs: references,
    }
}

/// The stored unread count of chat `chat_id`, through the model.
pub(super) async fn stored_unread(pool: &PgPool, chat_id: Uuid) -> i64 {
    chat::unread_count(pool, chat_id)
        .await
        .expect("the count reads")
}

/// `POST` `path` as `token` with the JSON `body`.
pub(super) async fn post_json(
    app: &Router,
    token: &str,
    path: &str,
    body: Value,
) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// `POST` `path` as `token` with no body at all.
pub(super) async fn post_empty(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .send(app)
        .await
}

/// The `POST …/chats/{id}/read` body Pleroma demands.
pub(super) fn marker_body(last_read_id: Uuid) -> Value {
    json!({"last_read_id": last_read_id.to_string()})
}

/// The read path of chat `chat`.
pub(super) fn read_path(chat: &str) -> String {
    format!("{ROOT}/{chat}/read")
}

/// The single-message read path inside chat `chat`.
pub(super) fn message_read_path(chat: &str, message: &str) -> String {
    format!("{ROOT}/{chat}/messages/{message}/read")
}

/// All four routes demand a user token granting `write:chats`: no token is
/// `POST`s `path` with `body` (when it has one) as `token`, or with no
/// credential at all when `token` is `None`.
pub(super) async fn post_as(
    app: &Router,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> common::TestResponse {
    let mut request = common::TestRequest::post(path);
    if let Some(secret) = token {
        request = request.bearer(secret);
    }
    if let Some(value) = body {
        request = request.json(value);
    }
    request.send(app).await
}

/// The refusal a route answers for `who`: `status`, and `message` as the
/// body's `error`.
pub(super) fn assert_refused(
    response: &common::TestResponse,
    who: &str,
    path: &str,
    status: StatusCode,
    message: &str,
) {
    assert_eq!(
        response.status,
        status,
        "{who} is refused on {path}: {}",
        response.text()
    );
    assert_eq!(
        response.error_field().as_deref(),
        Some(message),
        "the refusal wording for {who} on {path}"
    );
}

/// A media row of `uploader`, for the attachment column to point at.
pub(super) async fn insert_attachment(pool: &PgPool, uploader: &User) -> Uuid {
    insert_media(pool, uploader, "image/png").await
}

/// A media row of `uploader` with `content_type`, for the posting route's
/// media gate.
pub(super) async fn insert_media(pool: &PgPool, uploader: &User, content_type: &str) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO media_attachments (id, user_id, url, content_type)
         VALUES ($1, $2, 'https://test.example/media/x.png', $3)
         RETURNING id",
    )
    .bind(new_id())
    .bind(uploader.id)
    .bind(content_type)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

/// Claims media row `media` for a fresh status of `owner` — the already
/// posted media the chat gate must not hand out.
pub(super) async fn claim_media(pool: &PgPool, media: Uuid, owner: &User) {
    let posted = common::create_status(pool, owner, "a posted photo", Visibility::Public).await;
    let _claimed: sqlx::postgres::PgQueryResult =
        sqlx::query("UPDATE media_attachments SET status_id = $2 WHERE id = $1")
            .bind(media)
            .bind(posted.id)
            .execute(pool)
            .await
            .expect("the claim runs");
}

/// How many rows the `chat_messages` table holds.
pub(super) async fn count_messages(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM chat_messages")
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// The `GET`/`POST` messages path of chat `chat`.
pub(super) fn messages_path(chat: &str) -> String {
    format!("{ROOT}/{chat}/messages")
}

/// The `DELETE` path of one message of chat `chat`.
pub(super) fn message_delete_path(chat: &str, message: &str) -> String {
    format!("{ROOT}/{chat}/messages/{message}")
}

/// `GET`s `path` with no credential at all.
pub(super) async fn get_anonymous(app: &Router, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).send(app).await
}

/// The `POST …/by-account-id/{id}` path for the account `account`.
pub(super) fn open_path(account: &str) -> String {
    format!("{ROOT}/by-account-id/{account}")
}

/// `GET` `path` as `token`.
pub(super) async fn get_as(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// The chat of `owner` talking to `partner`, through the model — the row the
/// create endpoint writes.
pub(super) async fn open_chat(pool: &PgPool, owner: &User, partner: &User) -> chat::Chat {
    chat::get_or_create(pool, owner, partner)
        .await
        .expect("the fixture chat opens")
}

/// `DELETE` `path` as `token`.
pub(super) async fn delete_as(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::delete(path)
        .bearer(token)
        .send(app)
        .await
}
