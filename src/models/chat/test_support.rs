//! The fixtures the `chat` module's unit tests share: local and remote
//! accounts, a sender of messages, page parameters, readers of the stored
//! rows and the id and error helpers the assertions use. Test-only.

use sqlx::{Error as SqlxError, PgPool};
use uuid::Uuid;

use crate::{
    config::TEST_BASE_URL,
    error::AppError,
    models::user::{NewUser, User, insert},
    pagination::{MAX_LIMIT, PageParams},
    util::id::new_id,
};

pub(super) use crate::test_fixtures::local_user as local;

use super::{
    Chat, ChatFilter, ChatMessage, ChatMessageReference, NewChatMessage, add_message,
    messages_page, page_for_user,
};

/// The whole list in one page.
pub(super) const ALL: &str = "limit=40";

/// Inserts a mirror of `nickname` on `remote.example`.
pub(super) async fn remote(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@remote.example"),
            ap_id: format!("https://remote.example/users/{nickname}"),
            domain: Some("remote.example".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("fixture remote user inserts")
}

/// Sends `text` from `sender` to `recipient` in a transaction of its own —
/// what a caller of [`add_message`] does — and returns what it answered.
pub(super) async fn send(
    pool: &PgPool,
    sender: &User,
    recipient: &User,
    text: &str,
) -> (
    ChatMessage,
    ChatMessageReference,
    Option<ChatMessageReference>,
) {
    send_with(pool, sender, recipient, &NewChatMessage::text(text)).await
}

/// Like [`send`], but with the caller's own [`NewChatMessage`] instead of
/// a plain text one.
pub(super) async fn send_with(
    pool: &PgPool,
    sender: &User,
    recipient: &User,
    new_message: &NewChatMessage,
) -> (
    ChatMessage,
    ChatMessageReference,
    Option<ChatMessageReference>,
) {
    let mut conn = pool.begin().await.expect("a transaction begins");
    let sent = add_message(&mut conn, TEST_BASE_URL, sender, recipient, new_message)
        .await
        .expect("the fixture chat message is written");
    conn.commit().await.expect("the transaction commits");
    sent
}

/// A [`NewChatMessage`] for `text` carrying `attachment` and `ap_id`.
pub(super) fn with_extras(text: &str, attachment: Uuid, ap_id: &str) -> NewChatMessage {
    NewChatMessage {
        attachment_media_id: Some(attachment),
        ap_id: Some(ap_id.to_owned()),
        ..NewChatMessage::text(text)
    }
}

/// Page params from a query string, the way the extractor makes them.
pub(super) fn page(query: &str) -> PageParams {
    PageParams::from_query(Some(query), MAX_LIMIT).unwrap()
}

/// Every chat of `user_id`, newest activity first.
pub(super) async fn chats(pool: &PgPool, user_id: Uuid) -> Vec<Chat> {
    page_for_user(pool, user_id, page(ALL), ChatFilter::default())
        .await
        .unwrap()
}

/// Every message of chat `chat_id`, newest first.
pub(super) async fn messages(pool: &PgPool, chat_id: Uuid) -> Vec<ChatMessageReference> {
    messages_page(pool, chat_id, page(ALL)).await.unwrap()
}

/// The ids of `rows`, in the order they came back.
pub(super) fn chat_ids(rows: &[Chat]) -> Vec<Uuid> {
    rows.iter().map(|chat| chat.id).collect()
}

/// The message ids of `rows`, in the order they came back.
pub(super) fn message_ids(rows: &[ChatMessageReference]) -> Vec<Uuid> {
    rows.iter().map(|reference| reference.message_id).collect()
}

/// The reference ids of `rows`, in the order they came back.
pub(super) fn reference_ids(rows: &[ChatMessageReference]) -> Vec<Uuid> {
    rows.iter().map(|reference| reference.id).collect()
}

/// `ids` in descending id order: the order a keyset page fetches in.
pub(super) fn newest_by_id(ids: &[Uuid]) -> Vec<Uuid> {
    let mut sorted = ids.to_vec();
    sorted.sort_by(|newer, older| older.cmp(newer));
    sorted
}

/// The stored message behind `id`, tombstone included, `None` when its row
/// is gone.
pub(super) async fn stored_message(pool: &PgPool, id: Uuid) -> Option<ChatMessage> {
    sqlx::query_as::<_, ChatMessage>("SELECT * FROM chat_messages WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
}

/// The stored chat behind `id`, whatever it holds now.
pub(super) async fn stored_chat(pool: &PgPool, id: Uuid) -> Chat {
    sqlx::query_as::<_, Chat>("SELECT * FROM chats WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Moves a chat's `updated_at` an hour into the past, so a test can see a
/// later bump move it forward no matter how fast the machine is.
pub(super) async fn backdate_chat(pool: &PgPool, id: Uuid) -> Chat {
    sqlx::query_as::<_, Chat>(
        "UPDATE chats SET updated_at = now() - interval '1 hour' WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// How many rows one counting statement finds.
pub(super) async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    let row: (i64,) = sqlx::query_as(sql).fetch_one(pool).await.unwrap();
    row.0
}

/// The SQLSTATE a database failure carries, or an empty string when the
/// failure is not the database reporting a constraint.
pub(super) fn sqlstate_of(err: AppError) -> String {
    if let AppError::Database(SqlxError::Database(db_err)) = err {
        return db_err.code().unwrap_or_default().into_owned();
    }
    String::new()
}

/// A media row of `uploader`, for the attachment column to point at.
pub(super) async fn insert_attachment(pool: &PgPool, uploader: &User) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO media_attachments (id, user_id, url, content_type)
             VALUES ($1, $2, 'https://test.example/media/x.png', 'image/png')
             RETURNING id",
    )
    .bind(new_id())
    .bind(uploader.id)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}
