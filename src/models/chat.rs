//! The `chats`, `chat_messages` and `chat_message_references` tables:
//! one-to-one conversations, their messages, and what each participant has
//! seen of them.
//!
//! Reference: Pleroma's `Pleroma.Chat` (`lib/pleroma/chat.ex`) and
//! `Pleroma.Chat.MessageReference` (`lib/pleroma/chat/message_reference.ex`).
//! Pleroma names a chat's other side by the `ActivityPub` id string in
//! `chats.recipient` and keeps the message itself as a JSONB `ChatMessage`
//! object the reference points at; per `docs/ARCHITECTURE.md` decision 6 both
//! facts are relational here — `recipient_id` is a foreign key to [`User`] and a
//! message is a [`ChatMessage`] row — while everything a client sees stays the
//! same.
//!
//! The shape that matters for reading this module is that **a chat is one
//! participant's view of a conversation**, not the pair's. Each side owns its
//! own row, with its own [`pinned`](Chat::pinned) flag and its own
//! [`updated_at`](Chat::updated_at) bump, and every message is linked to the
//! conversation once per side through `chat_message_references`, which
//! [`add_message`] writes read for the sender and unread for the recipient. A
//! chat belongs to the account it is stored under, so ids are never shared
//! between participants and [`get_owned`] is the gate in front of every
//! per-chat operation — except the admin reads, which name a chat by its id
//! alone through [`get_by_id`] and check nothing.
//!
//! Message text is never markdown: [`render_chat_content`] escapes plain text
//! and linkifies URLs with [`text_to_html`], and [`add_message`] refuses
//! anything longer than [`MAX_CHAT_MESSAGE_CHARS`] characters. A message can
//! carry the `Idempotency-Key` its creator sent ([`NewChatMessage`]'s field of
//! that name): [`sender_reference_for_key`] replays a key written within the
//! last hour, which is what makes a retried `POST` answer its first message
//! instead of a duplicate. A message written without an
//! [`ap_id`](ChatMessage::ap_id) gets this instance's own
//! `{base_url}/objects/{uuid}` id from [`add_message`] — the address its
//! federated `Create` and `Delete` name it by (decision 6). Withdrawing a
//! message is a soft delete
//! ([`delete_message_for`]) that leaves the row as a tombstone, while a
//! participant who merely wants a message out of *their* list drops only their
//! own reference and leaves the other side untouched.
//!
//! The code is split by the table or question it answers: `chats` holds
//! the chat rows, `chat_page` the list of them, `messages` the message
//! rows and their reads, `send` and `write` the write of a message,
//! `references` the per-side links, `idempotency` the replay of a key and
//! `read_state` the unread flags. Every public item is re-exported here.
//!
//! [`User`]: crate::models::user::User
//! [`text_to_html`]: crate::util::format::text_to_html

use chrono::{DateTime, Utc};
use uuid::Uuid;

mod chat_page;
mod chats;
mod idempotency;
mod messages;
mod read_state;
mod references;
mod send;
mod write;

#[cfg(test)]
mod test_support;

pub use chat_page::page_for_user;
pub use chats::{get_by_id, get_or_create, get_owned, set_pinned};
pub use idempotency::sender_reference_for_key;
pub use messages::{
    message_by_ap_id, messages_page, render_chat_content, withdraw, withdraw_all_from_sender,
};
pub use read_state::{mark_message_read, mark_read, unread_count};
pub use references::{delete_message_for, message_reference, message_reference_for_user};
pub use send::add_message;

/// Longest source text a chat message may carry, in characters.
///
/// It is the chat counterpart of the `MAX_STATUS_CHARS` limit on a status:
/// Pleroma keeps its number in `[:instance, :chat_limit]` and refuses longer
/// content in `CommonAPI.post_chat_message` (`lib/pleroma/web/common_api.ex`).
/// The HTTP endpoint answers with the operator's
/// [`max_chat_message_chars`](crate::config::Config::max_chat_message_chars)
/// setting; this constant is the storage backstop no caller may raise, and
/// the config's default is the same number (pinned by a test in
/// [`crate::config`]).
pub const MAX_CHAT_MESSAGE_CHARS: usize = 5_000;

/// One row of `chats`: one participant's view of a conversation with one other
/// account (see the module docs for why a pair owns two rows).
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Chat {
    /// `UUIDv7` primary key, minted by [`get_or_create`] (decision 4).
    pub id: Uuid,
    /// Whose chat this row is.
    pub user_id: Uuid,
    /// The other participant.
    pub recipient_id: Uuid,
    /// Whether [`user_id`](Self::user_id) pinned this chat to the top of their
    /// list.
    pub pinned: bool,
    /// When the chat row was created.
    pub inserted_at: DateTime<Utc>,
    /// When it last saw activity: [`add_message`] bumps it on both sides, and
    /// [`page_for_user`] reads the list newest activity first.
    pub updated_at: DateTime<Utc>,
}

/// One row of `chat_messages`: a message both participants see.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct ChatMessage {
    /// `UUIDv7` primary key, minted by [`add_message`] (decision 4).
    pub id: Uuid,
    /// Who wrote it.
    pub sender_id: Uuid,
    /// The message as HTML from [`render_chat_content`]; emptied when the sender
    /// withdraws it.
    pub content: String,
    /// The message as its source text; emptied when the sender withdraws it.
    pub raw_text: String,
    /// The attachment the message carries, if any: a row of
    /// [`media_attachments`](crate::models::media_attachment).
    pub attachment_media_id: Option<Uuid>,
    /// `ActivityPub` object id (decision 6): `{BASE_URL}/objects/{uuid}` for a
    /// message this instance owns, the remote's own id for a mirrored one.
    /// Unique, and it survives the tombstone, so a withdrawn remote message is
    /// never re-imported.
    pub ap_id: Option<String>,
    /// The `Idempotency-Key` the creating request sent, kept so
    /// [`sender_reference_for_key`] can replay it within its window; `None`
    /// for a message posted without a key (and for a mirrored remote one,
    /// whose key lives on the sender's instance).
    pub idempotency_key: Option<String>,
    /// When the sender withdrew this message; `None` while it is live.
    pub deleted_at: Option<DateTime<Utc>>,
    /// When it was sent; for a mirrored remote message the `published` timestamp
    /// it arrived with. The `ChatMessage` entity renders it as `created_at`.
    pub inserted_at: DateTime<Utc>,
}

/// A message as one side of a chat sees it: that side's
/// `chat_message_references` row with the shared [`ChatMessage`]'s columns
/// joined onto it.
///
/// [`id`](Self::id) is the reference's id — what clients page a chat by, and
/// what the entity reports as the message's id — and the columns are read as
/// `REFERENCE_SELECT_SQL` describes them.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct ChatMessageReference {
    /// `UUIDv7` primary key of the reference row, minted by [`add_message`]
    /// (decision 4): the id of this message *in this chat*.
    pub id: Uuid,
    /// The chat this reference belongs to — one participant's side of the
    /// conversation.
    pub chat_id: Uuid,
    /// The message referenced, shared with the other side.
    pub message_id: Uuid,
    /// Whether the owner of [`chat_id`](Self::chat_id) still has to read this
    /// message; [`add_message`] writes the sender's own reference `false`.
    pub unread: bool,
    /// When the reference was created, which is when the message arrived in this
    /// chat.
    pub inserted_at: DateTime<Utc>,
    /// Who wrote the message: the entity's `account_id`.
    pub sender_id: Uuid,
    /// The message as HTML.
    pub content: String,
    /// The message as source text.
    pub raw_text: String,
    /// The attachment the message carries, if any.
    pub attachment_media_id: Option<Uuid>,
    /// The message's `ActivityPub` object id.
    pub ap_id: Option<String>,
    /// The `Idempotency-Key` the message was created with, `None` when it
    /// was created without one; the entity echoes it as `idempotency_key`.
    pub idempotency_key: Option<String>,
    /// When the message was sent: the entity's `created_at`.
    pub message_inserted_at: DateTime<Utc>,
}

/// What to insert: the columns of `chat_messages` a caller decides up front.
///
/// What is left out is what the model owns — the `id` [`add_message`] mints,
/// the `sender_id` it takes from its `sender`, and the `deleted_at` only a
/// withdrawal sets.
///
/// [`NewChatMessage::text`] is the usual starting point for a message somebody
/// typed; [`Default`] gives the empty column values, so a caller sets only what
/// it knows.
#[derive(Clone, Debug, Default)]
pub struct NewChatMessage {
    /// The message as HTML; [`NewChatMessage::text`] renders it with
    /// [`render_chat_content`].
    pub content: String,
    /// The message as source text; what [`MAX_CHAT_MESSAGE_CHARS`] measures.
    pub raw_text: String,
    /// The attachment to carry, if any.
    pub attachment_media_id: Option<Uuid>,
    /// `ActivityPub` object id: the remote id a mirrored message arrived
    /// with. A local caller leaves `None`, and [`add_message`] mints
    /// `{base_url}/objects/{uuid}` for the row it writes.
    pub ap_id: Option<String>,
    /// The `Idempotency-Key` of the request that creates this message, kept
    /// for [`sender_reference_for_key`]'s replay; `None` posts as usual.
    pub idempotency_key: Option<String>,
    /// The message's creation time: the `published` timestamp a mirrored
    /// remote message arrived with. `None` lets the database stamp `now()`,
    /// which is what every locally posted message gets.
    pub inserted_at: Option<DateTime<Utc>>,
}

impl NewChatMessage {
    /// A message whose source text is `text`, formatted the only way chat
    /// messages are formatted: [`render_chat_content`] in
    /// [`content`](Self::content), `text` kept verbatim in
    /// [`raw_text`](Self::raw_text), no attachment, no `ActivityPub` id, no
    /// idempotency key.
    #[must_use]
    pub fn text(text: &str) -> Self {
        Self {
            content: render_chat_content(text),
            raw_text: text.to_owned(),
            attachment_media_id: None,
            ap_id: None,
            idempotency_key: None,
            inserted_at: None,
        }
    }
}

/// What [`page_for_user`] leaves in the list, Pleroma's
/// `ChatController.index_query`
/// (`lib/pleroma/web/pleroma_api/controllers/chat_controller.ex`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChatFilter {
    /// `Some(true)` for pinned chats only, `Some(false)` for the unpinned rest,
    /// `None` for both.
    pub pinned: Option<bool>,
    /// Whether chats with accounts the user has muted (live mutes only: an
    /// expired one hides nothing) are included.
    ///
    /// Chats with accounts the user has blocked are left out whatever this says,
    /// as in Pleroma; an account that blocked the user does not hide its chat.
    pub with_muted: bool,
}
