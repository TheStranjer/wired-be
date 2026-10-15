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
//! per-chat operation.
//!
//! Message text is never markdown: [`render_chat_content`] escapes plain text
//! and linkifies URLs with [`text_to_html`], and [`add_message`] refuses
//! anything longer than [`MAX_CHAT_MESSAGE_CHARS`] characters. A message can
//! carry the `Idempotency-Key` its creator sent ([`NewChatMessage`]'s field of
//! that name): [`sender_reference_for_key`] replays a key written within the
//! last hour, which is what makes a retried `POST` answer its first message
//! instead of a duplicate. Withdrawing a message is a soft delete
//! ([`delete_message_for`]) that leaves the row as a tombstone, while a
//! participant who merely wants a message out of *their* list drops only their
//! own reference and leaves the other side untouched.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::user::User,
    pagination::PageParams,
    util::{format::text_to_html, id::new_id},
};

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

/// The joined shape of a reference and its message: the reference's own
/// columns first — its `id` is what a client pages a chat by, and what the
/// entity reports as the message's id — then the message's, with `m.inserted_at`
/// aliased so the message's creation time survives next to the reference's.
///
/// The text stops at `WHERE`, where each caller pushes the literal condition it
/// needs and binds the ids it names: [`messages_page`] (`m.deleted_at IS NULL
/// AND r.chat_id = …`, then the keyset), [`reference_in_chat`]
/// (`r.chat_id = … AND r.message_id = …`), [`message_reference`]
/// (`r.id = … AND r.chat_id = …`) and [`reference_by_id`] (`r.id = …`).
/// Only ids are bound, so nothing supplied by a client ever reaches the text.
const REFERENCE_SELECT_SQL: &str = "SELECT r.id, r.chat_id, r.message_id, r.unread, r.inserted_at,
           m.sender_id, m.content, m.raw_text, m.attachment_media_id, m.ap_id,
           m.idempotency_key, m.inserted_at AS message_inserted_at
      FROM chat_message_references r
      JOIN chat_messages m ON m.id = r.message_id
     WHERE ";

/// [`page_for_user`]: one participant's chats. The text stops where its one
/// placeholder belongs, the way [`REFERENCE_SELECT_SQL`] does; the pinned
/// restriction and the keyset [`PageParams::apply_keyset`] appends follow, the
/// latter running on the chat id `c.id`.
const CHATS_PAGE_SQL: &str = "SELECT c.* FROM chats c WHERE c.user_id = ";

/// [`get_or_create`]: one chat row per direction, written once.
///
/// `DO UPDATE SET recipient_id = EXCLUDED.recipient_id` changes nothing and is
/// there for what Pleroma's `Chat.get_or_create` says of the same trick —
/// "need to set something, otherwise we get nothing back at all": a
/// `DO NOTHING` conflict answers no row. Unlike [`BUMP_CHAT_SQL`] this leaves
/// [`updated_at`](Chat::updated_at) alone, so opening a chat does not move it in
/// its owner's list.
const GET_OR_CREATE_SQL: &str = "INSERT INTO chats (id, user_id, recipient_id)
     VALUES ($1, $2, $3)
ON CONFLICT (user_id, recipient_id) DO UPDATE SET recipient_id = EXCLUDED.recipient_id
 RETURNING *";

/// What [`add_message`] does to each side's chat row: create it when this is the
/// pair's first message, or bump it to the front of its owner's list (Pleroma's
/// `Chat.bump_or_create`).
const BUMP_CHAT_SQL: &str = "INSERT INTO chats (id, user_id, recipient_id)
     VALUES ($1, $2, $3)
ON CONFLICT (user_id, recipient_id) DO UPDATE SET updated_at = now()
 RETURNING *";

/// [`get_owned`]: the chat `id` of user `user_id`. The two participants' chats of
/// one conversation have different ids, so a chat somebody else owns simply is
/// not there.
const OWNED_CHAT_SQL: &str = "SELECT * FROM chats WHERE id = $1 AND user_id = $2";

/// [`set_pinned`]: pinning is the chat owner's own ordering choice, and like
/// every update here it refreshes `updated_at` — Pleroma's `Chat.pin` and
/// `Chat.unpin` go through `Repo.update`, which stamps its `updated_at` too.
const SET_PINNED_SQL: &str = "UPDATE chats
      SET pinned = $3, updated_at = now()
    WHERE id = $1 AND user_id = $2
  RETURNING *";

/// [`unread_count`]: how many messages of one participant's chat they have still
/// to read (Pleroma's `MessageReference.unread_count_for_chat`).
const UNREAD_COUNT_SQL: &str = "SELECT count(*) FROM chat_message_references
     WHERE chat_id = $1 AND unread";

/// The `INSERT` behind [`add_message`]: the id this module mints (decision 4)
/// and what the caller decided, with `inserted_at` left to the column default.
/// `RETURNING *` brings the row back with the timestamps the database stamped.
const INSERT_MESSAGE_SQL: &str = "INSERT INTO chat_messages (
     id, sender_id, content, raw_text, attachment_media_id, ap_id, idempotency_key
) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *";

/// One side's link between a chat and a message. `ON CONFLICT DO NOTHING` keeps
/// a redelivered message from being listed twice in the same chat; the row is
/// read back by the `(chat_id, message_id)` key either way, so a race answers
/// with the winner's reference.
const INSERT_REFERENCE_SQL: &str = "INSERT INTO chat_message_references (
     id, chat_id, message_id, unread
) VALUES ($1, $2, $3, $4)
ON CONFLICT (chat_id, message_id) DO NOTHING";

/// [`delete_message_for`]: who may see a reference and what it points at — the
/// chat's owner, the message's writer and the message's id — which is what
/// decides whether a message is hidden from one list or withdrawn from both.
const REFERENCE_OWNERSHIP_SQL: &str = "SELECT c.user_id, m.sender_id, m.id
      FROM chat_message_references r
      JOIN chats c ON c.id = r.chat_id
      JOIN chat_messages m ON m.id = r.message_id
     WHERE r.id = $1";

/// [`delete_message_for`]: the caller's own reference, and nobody else's.
const DELETE_REFERENCE_SQL: &str = "DELETE FROM chat_message_references WHERE id = $1";

/// [`delete_message_for`] when the caller wrote the message: a tombstone with
/// its text cleared and its `ap_id` reserved, exactly like
/// [`statuses::soft_delete`](crate::models::status::soft_delete). Every side's
/// reference survives and points at nothing live, which is what hides the
/// message from all of them.
const SOFT_DELETE_MESSAGE_SQL: &str = "UPDATE chat_messages
      SET content = '', raw_text = '', deleted_at = now()
    WHERE id = $1 AND deleted_at IS NULL";

/// [`sender_reference_for_key`]: the replay of a key inside its window — the
/// sender's own reference to the newest live message they created under
/// `idempotency_key` within the hour. Pleroma caches that key→id mapping in a
/// TTL cache for `Pipeline.common_pipeline`'s `idempotency_key` option; here
/// the key rides on the message row and the window is its `inserted_at`. The
/// `chats` join is what picks the *sender's* side out of a message's two
/// references, and the tombstone plus time guards leave an expired or
/// withdrawn answer without a row, which reads as "no replay".
const IDEMPOTENT_REPLAY_SQL: &str = "SELECT r.id, r.chat_id, r.message_id, r.unread, r.inserted_at,
           m.sender_id, m.content, m.raw_text, m.attachment_media_id, m.ap_id,
           m.idempotency_key, m.inserted_at AS message_inserted_at
      FROM chat_message_references r
      JOIN chat_messages m ON m.id = r.message_id
      JOIN chats c ON c.id = r.chat_id
     WHERE c.user_id = $1 AND m.sender_id = $1 AND m.idempotency_key = $2
       AND m.deleted_at IS NULL AND m.inserted_at > now() - interval '1 hour'
     ORDER BY m.inserted_at DESC
     LIMIT 1";

/// [`mark_read`]: every unread message of one chat up to a point, Pleroma's
/// `set_all_seen_for_chat`. A `None` `last_read_id` arrives as SQL `NULL` in
/// `$2`, which the first half of the parenthesized condition turns into "no
/// marker, so the whole chat".
const MARK_READ_SQL: &str = "UPDATE chat_message_references
      SET unread = false
    WHERE chat_id = $1 AND unread AND ($2::uuid IS NULL OR id <= $2)";

/// [`mark_message_read`]: one message of one chat. The `chat_id` condition is
/// what keeps a route that names *the caller's* chat from marking a reference
/// that merely shares its id space: one whose chat is another's matches no
/// row here, exactly as Pleroma's `mark_message_as_read` `with` clause fails
/// its `^chat_id <- to_string(cm_ref.chat_id)` match. No `unread` condition,
/// so marking a read reference is the no-op update Pleroma's `mark_as_read`
/// makes of it, and the statement still reports the row it touched.
const MARK_MESSAGE_READ_SQL: &str = "UPDATE chat_message_references
      SET unread = false
    WHERE id = $1 AND chat_id = $2";

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
    /// `ActivityPub` object id: the remote id a mirrored message arrived with.
    /// A local caller leaves `None` until the federation ticket mints
    /// `{BASE_URL}/objects/{uuid}` ids for chat messages.
    pub ap_id: Option<String>,
    /// The `Idempotency-Key` of the request that creates this message, kept
    /// for [`sender_reference_for_key`]'s replay; `None` posts as usual.
    pub idempotency_key: Option<String>,
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
    /// Whether chats with accounts the user has muted are included.
    ///
    /// Nothing is excluded by it yet: mutes — and blocks, which Pleroma leaves
    /// out of the list unconditionally — arrive with their own tables and
    /// tickets, which wire this flag up to them. Until then every chat of the
    /// user is listed, muted accounts included.
    pub with_muted: bool,
}

/// The HTML a chat message stores for the source text `text`.
///
/// Chat content is escaped plain text with linkified URLs and nothing else:
/// [`text_to_html`] and never
/// [`render_markdown`](crate::util::format::render_markdown), matching
/// Pleroma's `CommonAPI.format_chat_content` (`lib/pleroma/web/common_api.ex`),
/// which runs `Formatter.html_escape` and `Formatter.linkify` over the text and
/// no markdown parser.
#[must_use]
pub fn render_chat_content(text: &str) -> String {
    text_to_html(text)
}

/// The chat row of `user` talking to `recipient`, created when the pair has none
/// yet.
///
/// Only that one row is created: the chat row of `recipient` is theirs, written
/// when they open the conversation or receive a message ([`add_message`]). The
/// direction the arguments name is the direction of the row, so
/// `get_or_create(pool, alice, bob)` and `get_or_create(pool, bob, alice)` are
/// two different chats with two different ids.
///
/// Creating is idempotent and never bumps [`updated_at`](Chat::updated_at): a
/// chat that exists stays where its owner's list has it.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); an unknown `user` or
/// `recipient` is the foreign-key violation on `users(id)`.
pub async fn get_or_create(pool: &PgPool, user: &User, recipient: &User) -> AppResult<Chat> {
    let chat = sqlx::query_as::<_, Chat>(GET_OR_CREATE_SQL)
        .bind(new_id())
        .bind(user.id)
        .bind(recipient.id)
        .fetch_one(pool)
        .await?;
    Ok(chat)
}

/// The chat `id`, provided it belongs to `user_id`. `None` when no chat has that
/// id, and `None` just as well when it belongs to somebody else: one
/// participant's chat id means nothing to another.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_owned(pool: &PgPool, user_id: Uuid, id: Uuid) -> AppResult<Option<Chat>> {
    let chat = sqlx::query_as::<_, Chat>(OWNED_CHAT_SQL)
        .bind(id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(chat)
}

/// The chats of `user_id`, newest activity first, `page`'s `limit` at a time,
/// `filter`'s restrictions applied.
///
/// The list order is [`updated_at`](Chat::updated_at), which is what Pleroma's
/// `Chat.for_user_query` orders by, while the cursors are ids, which is what
/// [`PageParams::apply_keyset`] makes of them. Both are honoured: the SQL
/// selects and bounds the page by id, and the fetched rows are then ordered by
/// activity. Since a chat's id never changes while its `updated_at` does, no
/// page can hold a longer activity order than itself, and
/// [`PageParams::finish`]'s `min_id` flip has nothing left to undo here.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn page_for_user(
    pool: &PgPool,
    user_id: Uuid,
    page: PageParams,
    filter: ChatFilter,
) -> AppResult<Vec<Chat>> {
    let mut builder = QueryBuilder::<Postgres>::new(CHATS_PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // the result keeps these statements from being unused (decision 14).
    let _bound: &mut _ = builder.push_bind(user_id);
    if let Some(pinned) = filter.pinned {
        let _pinned: &mut _ = builder.push(" AND c.pinned = ").push_bind(pinned);
    }
    page.apply_keyset(&mut builder, "c.id");
    let mut chats = builder.build_query_as::<Chat>().fetch_all(pool).await?;
    sort_by_activity(&mut chats);
    Ok(chats)
}

/// Pins (`pinned = true`) or unpins `user_id`'s chat `chat_id` and returns the
/// row, `None` when no chat of theirs has that id.
///
/// The flag is per participant: pinning a conversation never touches the other
/// side's row.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_pinned(
    pool: &PgPool,
    user_id: Uuid,
    chat_id: Uuid,
    pinned: bool,
) -> AppResult<Option<Chat>> {
    let chat = sqlx::query_as::<_, Chat>(SET_PINNED_SQL)
        .bind(chat_id)
        .bind(user_id)
        .bind(pinned)
        .fetch_optional(pool)
        .await?;
    Ok(chat)
}

/// How many messages of chat `chat_id` its owner has not read yet, which is what
/// the `Chat` entity renders as `unread` (Pleroma's
/// `MessageReference.unread_count_for_chat`).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unread_count(pool: &PgPool, chat_id: Uuid) -> AppResult<i64> {
    let row = sqlx::query_as::<_, (i64,)>(UNREAD_COUNT_SQL)
        .bind(chat_id)
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

/// Writes one message from `sender` to `recipient` on `conn`, together with
/// both sides' chat rows and references.
///
/// The answer is the message, the sender's reference and — when the recipient
/// is a local account — the recipient's.
///
/// `conn` is the caller's transaction, on purpose: sending a chat message means
/// federating it too, so the ticket that delivers messages wraps this write and
/// its delivery job in one transaction and rolls both back together.
///
/// What it writes, in order:
///
/// - `sender`'s chat of `recipient`: created if the pair has none, bumped to the
///   front of its owner's list;
/// - `recipient`'s chat of `sender`, created and bumped the same way — **unless
///   `recipient` is remote**, whose chat rows live on their own instance; then
///   nothing at all is stored for them and the third half of the answer is
///   `None`;
/// - the [`ChatMessage`] row, from `new_message`;
/// - the sender's reference, already read: you have read what you wrote;
/// - the recipient's reference, unread.
///
/// # Errors
///
/// [`AppError::Unprocessable`] when the message's source text is longer than
/// [`MAX_CHAT_MESSAGE_CHARS`] characters, before anything is written; any
/// database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError) — an unknown sender or
/// recipient is the foreign-key violation on `users(id)`, an attachment that is
/// not a media row the one on `media_attachments(id)`, and a reused `ap_id` the
/// unique violation on `chat_messages.ap_id`.
pub async fn add_message(
    conn: &mut PgConnection,
    sender: &User,
    recipient: &User,
    new_message: &NewChatMessage,
) -> AppResult<(
    ChatMessage,
    ChatMessageReference,
    Option<ChatMessageReference>,
)> {
    check_length(&new_message.raw_text)?;
    let sender_chat = bump_or_create(conn, sender.id, recipient.id).await?;
    let recipient_chat = local_recipient_chat(conn, sender, recipient).await?;
    let message = write_message(conn, sender.id, new_message).await?;
    let sender_reference = reference_in_chat(conn, sender_chat.id, &message, false).await?;
    let recipient_reference = match recipient_chat {
        Some(chat) => Some(reference_in_chat(conn, chat.id, &message, true).await?),
        None => None,
    };
    Ok((message, sender_reference, recipient_reference))
}

/// The messages of chat `chat_id` as its owner sees them, newest first, `page`'s
/// `limit` at a time, a withdrawn message left out: its reference may well still
/// be there, but its tombstone has no text to show.
///
/// Pleroma reads the same list from `MessageReference.for_chat_query`, ordered
/// `desc: :id` on the reference; a message's two references are written by the
/// same [`add_message`] call, so paging a chat by reference id is paging it by
/// message.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn messages_page(
    pool: &PgPool,
    chat_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<ChatMessageReference>> {
    let mut builder = QueryBuilder::<Postgres>::new(REFERENCE_SELECT_SQL);
    let _live: &mut _ = builder.push("m.deleted_at IS NULL AND r.chat_id = ");
    let _bound: &mut _ = builder.push_bind(chat_id);
    page.apply_keyset(&mut builder, "r.id");
    let references = builder
        .build_query_as::<ChatMessageReference>()
        .fetch_all(pool)
        .await?;
    Ok(page.finish(references))
}

/// The one reference `ref_id` of chat `chat_id`, `None` when no reference
/// has that id **in that chat**.
///
/// This is how a per-chat route tells a foreign chat's reference from the
/// caller's own: reference ids are drawn from one space (decision 4), so
/// one of somebody else's chat simply is not here — Pleroma's
/// `delete_message` `with` clause answers the same way.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn message_reference(
    pool: &PgPool,
    chat_id: Uuid,
    ref_id: Uuid,
) -> AppResult<Option<ChatMessageReference>> {
    let mut builder = QueryBuilder::<Postgres>::new(REFERENCE_SELECT_SQL);
    let _pair: &mut _ = builder
        .push("r.id = ")
        .push_bind(ref_id)
        .push(" AND r.chat_id = ")
        .push_bind(chat_id);
    let reference = builder
        .build_query_as::<ChatMessageReference>()
        .fetch_optional(pool)
        .await?;
    Ok(reference)
}

/// The sender's own view of the message their `key` created, `None` when no
/// live message of `sender_id` carries that key within the replay window —
/// the one hour its lookup statement measures.
///
/// The answer is the reference [`add_message`] wrote for the sender — already
/// read, in the chat the message was posted to — so a replayed `POST` renders
/// exactly what the original one answered with. A key that is unknown, older
/// than the hour, or whose message the sender has since withdrawn all answer
/// `None`, and the caller posts as if the key were fresh.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn sender_reference_for_key(
    pool: &PgPool,
    sender_id: Uuid,
    key: &str,
) -> AppResult<Option<ChatMessageReference>> {
    let reference = sqlx::query_as::<_, ChatMessageReference>(IDEMPOTENT_REPLAY_SQL)
        .bind(sender_id)
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(reference)
}

/// Hides one message from `user_id`, the owner of the reference `ref_id`, and
/// reports whether there was such a reference to hide.
///
/// Two different things happen, which is what makes a reference per participant
/// worth having:
///
/// - **taking a message out of one's own list** deletes only that side's
///   reference: the message stays live and the other participant keeps seeing
///   it. There is no way to delete somebody else's copy of a chat message;
/// - **the sender withdrawing their message** soft-deletes it, which hides it
///   from both listings at once, and deletes their own reference with it, since
///   they asked for it out of their list too.
///
/// An unknown `ref_id`, and one belonging to another participant's chat, are
/// both answered `false` and change nothing; so is a second call for a reference
/// the first one already removed.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_message_for(pool: &PgPool, user_id: Uuid, ref_id: Uuid) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    let Some(owner) = reference_owner(&mut tx, ref_id).await? else {
        return Ok(false);
    };
    if owner.for_user != user_id {
        return Ok(false);
    }
    remove_reference(&mut tx, ref_id).await?;
    if owner.sender_id == user_id {
        withdraw_message(&mut tx, owner.message_id).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Marks the messages of chat `chat_id` read, up to and including reference
/// `last_read_id`, and returns how many references went from unread to read.
///
/// `None` marks every unread message of the chat, as Pleroma's
/// `set_all_seen_for_chat` does when the client sends no marker. Already-read
/// references are not touched, so calling it again with the same marker reads
/// nothing new, and a message that arrives after the marker keeps its unread
/// flag.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mark_read(pool: &PgPool, chat_id: Uuid, last_read_id: Option<Uuid>) -> AppResult<u64> {
    let updated = sqlx::query(MARK_READ_SQL)
        .bind(chat_id)
        .bind(last_read_id)
        .execute(pool)
        .await?;
    Ok(updated.rows_affected())
}

/// Marks the message behind `ref_id` in chat `chat_id` read and returns it.
///
/// `None` when no reference has that id **in that chat** — the one statement
/// answering both the unknown id and the id of a reference living in somebody
/// else's chat.
///
/// Marking an already read reference is idempotent: it comes back unchanged, as
/// Pleroma's `MessageReference.mark_as_read` leaves it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mark_message_read(
    pool: &PgPool,
    chat_id: Uuid,
    ref_id: Uuid,
) -> AppResult<Option<ChatMessageReference>> {
    let marked = sqlx::query(MARK_MESSAGE_READ_SQL)
        .bind(ref_id)
        .bind(chat_id)
        .execute(pool)
        .await?;
    if marked.rows_affected() == 0 {
        return Ok(None);
    }
    reference_by_id(pool, ref_id).await
}

/// Orders a page of chats by activity, newest first, the id breaking a tie
/// between two chats bumped in the same transaction.
fn sort_by_activity(chats: &mut [Chat]) {
    chats.sort_unstable_by(|newer, older| {
        older
            .updated_at
            .cmp(&newer.updated_at)
            .then(older.id.cmp(&newer.id))
    });
}

/// Refuses a message whose source text is longer than
/// [`MAX_CHAT_MESSAGE_CHARS`] characters, counted the way a client counts them:
/// Unicode scalar values, not bytes.
fn check_length(raw_text: &str) -> AppResult<()> {
    let length = raw_text.chars().count();
    if length > MAX_CHAT_MESSAGE_CHARS {
        return Err(AppError::Unprocessable(format!(
            "Chat messages are limited to {MAX_CHAT_MESSAGE_CHARS} characters (got {length})"
        )));
    }
    Ok(())
}

/// The chat of local `recipient` talking to `sender`, or `None` when `recipient`
/// is a remote account, whose side of a conversation this instance does not
/// keep.
async fn local_recipient_chat(
    conn: &mut PgConnection,
    sender: &User,
    recipient: &User,
) -> AppResult<Option<Chat>> {
    if !recipient.local.get() {
        return Ok(None);
    }
    let chat = bump_or_create(conn, recipient.id, sender.id).await?;
    Ok(Some(chat))
}

/// The chat of `user_id` talking to `recipient_id`: created if missing, bumped to
/// the front of its owner's list if it was there.
async fn bump_or_create(
    conn: &mut PgConnection,
    user_id: Uuid,
    recipient_id: Uuid,
) -> AppResult<Chat> {
    let chat = sqlx::query_as::<_, Chat>(BUMP_CHAT_SQL)
        .bind(new_id())
        .bind(user_id)
        .bind(recipient_id)
        .fetch_one(conn)
        .await?;
    Ok(chat)
}

/// Writes the message row of [`add_message`] and returns it.
async fn write_message(
    conn: &mut PgConnection,
    sender_id: Uuid,
    new_message: &NewChatMessage,
) -> AppResult<ChatMessage> {
    let message = sqlx::query_as::<_, ChatMessage>(INSERT_MESSAGE_SQL)
        .bind(new_id())
        .bind(sender_id)
        .bind(new_message.content.as_str())
        .bind(new_message.raw_text.as_str())
        .bind(new_message.attachment_media_id)
        .bind(new_message.ap_id.as_deref())
        .bind(new_message.idempotency_key.as_deref())
        .fetch_one(conn)
        .await?;
    Ok(message)
}

/// Links `message` to the chat `chat_id` as unread (`false` for the side that
/// wrote it) and returns that side's view of the message.
///
/// The link is written idempotently and read back through the same join the
/// listing uses, so what [`add_message`] answers is exactly what a later
/// [`messages_page`] of the same chat answers.
async fn reference_in_chat(
    conn: &mut PgConnection,
    chat_id: Uuid,
    message: &ChatMessage,
    unread: bool,
) -> AppResult<ChatMessageReference> {
    sqlx::query(INSERT_REFERENCE_SQL)
        .bind(new_id())
        .bind(chat_id)
        .bind(message.id)
        .bind(unread)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    let mut builder = QueryBuilder::<Postgres>::new(REFERENCE_SELECT_SQL);
    let _pair: &mut _ = builder
        .push("r.chat_id = ")
        .push_bind(chat_id)
        .push(" AND r.message_id = ")
        .push_bind(message.id);
    let reference = builder
        .build_query_as::<ChatMessageReference>()
        .fetch_one(&mut *conn)
        .await?;
    Ok(reference)
}

/// The one reference `ref_id` joined with its message.
async fn reference_by_id(pool: &PgPool, ref_id: Uuid) -> AppResult<Option<ChatMessageReference>> {
    let mut builder = QueryBuilder::<Postgres>::new(REFERENCE_SELECT_SQL);
    let _id: &mut _ = builder.push("r.id = ").push_bind(ref_id);
    let reference = builder
        .build_query_as::<ChatMessageReference>()
        .fetch_optional(pool)
        .await?;
    Ok(reference)
}

/// Whose reference `ref_id` is, who wrote its message, and which message that
/// is; `None` when no reference has that id, or its message is gone.
async fn reference_owner(
    conn: &mut PgConnection,
    ref_id: Uuid,
) -> AppResult<Option<ReferenceOwner>> {
    let row = sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(REFERENCE_OWNERSHIP_SQL)
        .bind(ref_id)
        .fetch_optional(conn)
        .await?;
    Ok(row.map(|(for_user, sender_id, message_id)| ReferenceOwner {
        for_user,
        sender_id,
        message_id,
    }))
}

/// The three ids [`delete_message_for`] decides from, named.
#[derive(Debug)]
struct ReferenceOwner {
    /// The owner of the chat the reference belongs to.
    for_user: Uuid,
    /// Who wrote the message.
    sender_id: Uuid,
    /// What it points at.
    message_id: Uuid,
}

/// Deletes reference `ref_id`, which the caller has established belongs to the
/// user asking.
async fn remove_reference(conn: &mut PgConnection, ref_id: Uuid) -> AppResult<()> {
    sqlx::query(DELETE_REFERENCE_SQL)
        .bind(ref_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

/// Turns message `message_id` into a tombstone, once: a message that is already
/// one matches no row and is left as it is, which is what keeps a second
/// withdrawal from rewriting anything.
async fn withdraw_message(conn: &mut PgConnection, message_id: Uuid) -> AppResult<()> {
    sqlx::query(SOFT_DELETE_MESSAGE_SQL)
        .bind(message_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::{Error as SqlxError, PgPool};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        error::AppError,
        models::user::{NewUser, insert, local_ap_id},
        pagination::MAX_LIMIT,
    };

    /// The whole list in one page.
    const ALL: &str = "limit=40";

    /// Inserts a local user named `nickname`.
    async fn local(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("fixture user inserts")
    }

    /// Inserts a mirror of `nickname` on `remote.example`.
    async fn remote(pool: &PgPool, nickname: &str) -> User {
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
    async fn send(
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
    async fn send_with(
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
        let sent = add_message(&mut conn, sender, recipient, new_message)
            .await
            .expect("the fixture chat message is written");
        conn.commit().await.expect("the transaction commits");
        sent
    }

    /// A [`NewChatMessage`] for `text` carrying `attachment` and `ap_id`.
    fn with_extras(text: &str, attachment: Uuid, ap_id: &str) -> NewChatMessage {
        NewChatMessage {
            attachment_media_id: Some(attachment),
            ap_id: Some(ap_id.to_owned()),
            ..NewChatMessage::text(text)
        }
    }

    /// Page params from a query string, the way the extractor makes them.
    fn page(query: &str) -> PageParams {
        PageParams::from_query(Some(query), MAX_LIMIT).unwrap()
    }

    /// Every chat of `user_id`, newest activity first.
    async fn chats(pool: &PgPool, user_id: Uuid) -> Vec<Chat> {
        page_for_user(pool, user_id, page(ALL), ChatFilter::default())
            .await
            .unwrap()
    }

    /// Every message of chat `chat_id`, newest first.
    async fn messages(pool: &PgPool, chat_id: Uuid) -> Vec<ChatMessageReference> {
        messages_page(pool, chat_id, page(ALL)).await.unwrap()
    }

    /// The ids of `rows`, in the order they came back.
    fn chat_ids(rows: &[Chat]) -> Vec<Uuid> {
        rows.iter().map(|chat| chat.id).collect()
    }

    /// The message ids of `rows`, in the order they came back.
    fn message_ids(rows: &[ChatMessageReference]) -> Vec<Uuid> {
        rows.iter().map(|reference| reference.message_id).collect()
    }

    /// The reference ids of `rows`, in the order they came back.
    fn reference_ids(rows: &[ChatMessageReference]) -> Vec<Uuid> {
        rows.iter().map(|reference| reference.id).collect()
    }

    /// `ids` in descending id order: the order a keyset page fetches in.
    fn newest_by_id(ids: &[Uuid]) -> Vec<Uuid> {
        let mut sorted = ids.to_vec();
        sorted.sort_by(|newer, older| older.cmp(newer));
        sorted
    }

    /// The stored message behind `id`, tombstone included, `None` when its row
    /// is gone.
    async fn stored_message(pool: &PgPool, id: Uuid) -> Option<ChatMessage> {
        sqlx::query_as::<_, ChatMessage>("SELECT * FROM chat_messages WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    /// The stored chat behind `id`, whatever it holds now.
    async fn stored_chat(pool: &PgPool, id: Uuid) -> Chat {
        sqlx::query_as::<_, Chat>("SELECT * FROM chats WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Moves a chat's `updated_at` an hour into the past, so a test can see a
    /// later bump move it forward no matter how fast the machine is.
    async fn backdate_chat(pool: &PgPool, id: Uuid) -> Chat {
        sqlx::query_as::<_, Chat>(
            "UPDATE chats SET updated_at = now() - interval '1 hour' WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// How many rows one counting statement finds.
    async fn count(pool: &PgPool, sql: &'static str) -> i64 {
        let row: (i64,) = sqlx::query_as(sql).fetch_one(pool).await.unwrap();
        row.0
    }

    /// The SQLSTATE a database failure carries, or an empty string when the
    /// failure is not the database reporting a constraint.
    fn sqlstate_of(err: AppError) -> String {
        if let AppError::Database(SqlxError::Database(db_err)) = err {
            return db_err.code().unwrap_or_default().into_owned();
        }
        String::new()
    }

    #[tokio::test]
    async fn get_or_create_is_idempotent_and_per_direction() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;

        let alice_chat = get_or_create(&pool, &alice, &bob).await.unwrap();
        let again = get_or_create(&pool, &alice, &bob).await.unwrap();

        assert!(alice_chat.id.get_version_num() == 7, "the id is a UUIDv7");
        assert_eq!(alice_chat.id, again.id, "the second call writes nothing");
        assert_eq!(alice_chat.user_id, alice.id, "the row belongs to alice");
        assert_eq!(alice_chat.recipient_id, bob.id, "and is about bob");
        assert!(!alice_chat.pinned, "a fresh chat is unpinned");
        assert_eq!(
            alice_chat.inserted_at, alice_chat.updated_at,
            "opening a chat does not bump it"
        );
        assert_eq!(
            get_owned(&pool, alice.id, alice_chat.id).await.unwrap(),
            Some(alice_chat.clone()),
            "get_owned finds a chat for its owner"
        );

        let bob_chat = get_or_create(&pool, &bob, &alice).await.unwrap();
        assert_ne!(
            bob_chat.id, alice_chat.id,
            "the other direction is a second chat row"
        );
        assert_eq!(bob_chat.user_id, bob.id, "belonging to bob");
        assert_eq!(bob_chat.recipient_id, alice.id, "about alice");
        assert_eq!(
            chat_ids(&chats(&pool, alice.id).await),
            vec![alice_chat.id],
            "alice lists one chat"
        );
        assert_eq!(
            chat_ids(&chats(&pool, bob.id).await),
            vec![bob_chat.id],
            "bob lists his own, not alice's"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM chats").await,
            2,
            "two rows for one pair"
        );
    }

    #[tokio::test]
    async fn a_chat_is_only_its_owner_s() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let alice_chat = get_or_create(&pool, &alice, &bob).await.unwrap();

        assert!(
            get_owned(&pool, bob.id, alice_chat.id)
                .await
                .unwrap()
                .is_none(),
            "bob cannot read alice's chat row by id"
        );
        assert!(
            get_owned(&pool, alice.id, new_id())
                .await
                .unwrap()
                .is_none(),
            "an unknown id is no chat"
        );
        assert!(
            set_pinned(&pool, bob.id, alice_chat.id, true)
                .await
                .unwrap()
                .is_none(),
            "and only its owner pins it"
        );
        assert!(
            !stored_chat(&pool, alice_chat.id).await.pinned,
            "so alice's chat stayed unpinned"
        );
        assert!(
            set_pinned(&pool, alice.id, new_id(), true)
                .await
                .unwrap()
                .is_none(),
            "an unknown chat is nothing to pin"
        );
    }

    #[tokio::test]
    async fn pinning_is_per_participant_and_orders_the_list() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let with_bob = get_or_create(&pool, &alice, &bob).await.unwrap();
        let with_carol = get_or_create(&pool, &alice, &carol).await.unwrap();
        let bobs_side = get_or_create(&pool, &bob, &alice).await.unwrap();

        let pinned = set_pinned(&pool, alice.id, with_carol.id, true)
            .await
            .unwrap()
            .expect("alice owns that chat");

        assert!(pinned.pinned, "the flag is set");
        assert!(
            pinned.updated_at > with_carol.updated_at,
            "pinning stamps the row"
        );
        assert_eq!(
            chat_ids(&chats(&pool, alice.id).await),
            vec![pinned.id, with_bob.id],
            "the pinned chat leads the list"
        );
        assert_eq!(
            chat_ids(
                &page_for_user(
                    &pool,
                    alice.id,
                    page(ALL),
                    ChatFilter {
                        pinned: Some(true),
                        with_muted: false,
                    },
                )
                .await
                .unwrap(),
            ),
            vec![pinned.id],
            "the pinned filter names it"
        );
        assert_eq!(
            chat_ids(
                &page_for_user(
                    &pool,
                    alice.id,
                    page(ALL),
                    ChatFilter {
                        pinned: Some(false),
                        with_muted: true,
                    },
                )
                .await
                .unwrap(),
            ),
            vec![with_bob.id],
            "the unpinned filter gets the rest; with_muted excludes nothing yet"
        );
        assert!(
            !stored_chat(&pool, bobs_side.id).await.pinned,
            "bob's own row is untouched by alice's pin"
        );

        let unpinned = set_pinned(&pool, alice.id, pinned.id, false)
            .await
            .unwrap()
            .expect("the chat is still there");
        assert!(!unpinned.pinned, "unpinning clears the flag");
        assert_eq!(
            chat_ids(&chats(&pool, alice.id).await),
            vec![unpinned.id, with_bob.id],
            "the bump it stamps keeps it first for now"
        );
    }

    #[tokio::test]
    async fn add_message_writes_both_sides_with_their_read_flags() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;

        let (message, alice_ref, bob_ref) = send(&pool, &alice, &bob, "hi bob").await;
        let bob_ref = bob_ref.expect("bob is local, so his reference exists");

        assert!(message.id.get_version_num() == 7, "the id is a UUIDv7");
        assert_eq!(message.sender_id, alice.id, "alice wrote it");
        assert_eq!(message.raw_text, "hi bob", "the source text is stored");
        assert_eq!(message.content, "<p>hi bob</p>", "and the rendered HTML");
        assert!(message.attachment_media_id.is_none(), "no attachment");
        assert!(message.ap_id.is_none(), "no ActivityPub id yet");
        assert!(message.deleted_at.is_none(), "the message is live");

        let alice_chat = stored_chat(&pool, alice_ref.chat_id).await;
        let bob_chat = stored_chat(&pool, bob_ref.chat_id).await;
        assert_ne!(alice_chat.id, bob_chat.id, "each side got its own chat row");
        assert_eq!(alice_chat.user_id, alice.id, "one belongs to alice");
        assert_eq!(alice_chat.recipient_id, bob.id, "about bob");
        assert_eq!(bob_chat.user_id, bob.id, "the other to bob");
        assert_eq!(bob_chat.recipient_id, alice.id, "about alice");

        assert_eq!(
            alice_ref.message_id, message.id,
            "alice's reference points at the message"
        );
        assert_eq!(bob_ref.message_id, message.id, "and so does bob's");
        assert_eq!(alice_ref.sender_id, alice.id, "the join carries the writer");
        assert!(!alice_ref.unread, "alice has read what she wrote");
        assert!(bob_ref.unread, "bob has not read it yet");
        assert!(
            message.idempotency_key.is_none(),
            "a plain send carries no idempotency key"
        );
        assert_eq!(alice_ref.content, message.content, "and the text");
        assert_eq!(
            alice_ref.message_inserted_at, message.inserted_at,
            "and the message's creation time"
        );

        assert_eq!(
            unread_count(&pool, alice_ref.chat_id).await.unwrap(),
            0,
            "alice's chat has nothing to read"
        );
        assert_eq!(
            unread_count(&pool, bob_ref.chat_id).await.unwrap(),
            1,
            "bob's has one message"
        );
        assert_eq!(
            message_ids(&messages(&pool, alice_ref.chat_id).await),
            vec![message.id],
            "alice lists the message"
        );
        assert_eq!(
            message_ids(&messages(&pool, bob_ref.chat_id).await),
            vec![message.id],
            "and so does bob"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM chat_message_references").await,
            2,
            "exactly one reference per side"
        );
    }

    #[tokio::test]
    async fn a_message_bumps_both_sides_of_the_conversation() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let (first, alice_ref, bob_ref) = send(&pool, &alice, &bob, "first").await;
        let (_, bobs_other_chat, _) = send(&pool, &bob, &carol, "elsewhere").await;
        let stale_alice_chat = backdate_chat(&pool, alice_ref.chat_id).await;
        let bobs_chat = bob_ref.expect("bob is local");
        let stale_bob_chat = backdate_chat(&pool, bobs_chat.chat_id).await;

        let (second, new_alice_ref, new_bob_ref) = send(&pool, &alice, &bob, "second").await;
        let new_bob_ref = new_bob_ref.expect("bob's side of the new message");

        assert_eq!(
            new_alice_ref.chat_id, stale_alice_chat.id,
            "alice's chat is the same row, not a new one"
        );
        assert_eq!(new_bob_ref.chat_id, stale_bob_chat.id, "and so is bob's");
        assert_eq!(
            new_alice_ref.message_id, second.id,
            "the new message is linked to both sides"
        );
        assert_eq!(new_bob_ref.message_id, second.id, "under one message id");
        assert!(
            stored_chat(&pool, alice_ref.chat_id).await.updated_at > stale_alice_chat.updated_at,
            "alice's chat moved to now"
        );
        assert!(
            stored_chat(&pool, bobs_chat.chat_id).await.updated_at > stale_bob_chat.updated_at,
            "bob's moved too, though he wrote nothing"
        );
        assert_eq!(
            chat_ids(&chats(&pool, alice.id).await),
            vec![alice_ref.chat_id],
            "alice lists one chat"
        );
        assert_eq!(
            chat_ids(&chats(&pool, bob.id).await),
            vec![bobs_chat.chat_id, bobs_other_chat.chat_id],
            "the conversation with news jumps ahead of bob's other chat"
        );
        assert_eq!(
            message_ids(&messages(&pool, alice_ref.chat_id).await),
            vec![second.id, first.id],
            "and the chat lists both messages, newest first"
        );
    }

    #[tokio::test]
    async fn a_remote_recipient_gets_no_chat_and_no_reference() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let mallory = remote(&pool, "mallory").await;

        let (message, alice_ref, mallory_ref) = send(&pool, &alice, &mallory, "hello there").await;

        assert!(
            mallory_ref.is_none(),
            "nothing is stored for a remote account"
        );
        assert_eq!(
            alice_ref.message_id, message.id,
            "alice's reference is hers"
        );
        assert!(!alice_ref.unread, "and she has read what she sent");
        let alice_chat = stored_chat(&pool, alice_ref.chat_id).await;
        assert_eq!(
            alice_chat.recipient_id, mallory.id,
            "in the one chat row this call wrote, about the remote account"
        );
        assert_eq!(
            unread_count(&pool, alice_ref.chat_id).await.unwrap(),
            0,
            "her chat has nothing unread"
        );
        assert_eq!(
            message_ids(&messages(&pool, alice_ref.chat_id).await),
            vec![message.id],
            "and it shows her own message"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM chats").await,
            1,
            "one chat row for the pair, not two"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM chat_message_references").await,
            1,
            "and one reference"
        );
        assert!(
            chats(&pool, mallory.id).await.is_empty(),
            "the remote account lists nothing here"
        );
    }

    #[tokio::test]
    async fn an_overlong_message_is_refused_before_anything_is_written() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let too_long = "x".repeat(MAX_CHAT_MESSAGE_CHARS + 1);
        let mut conn = pool.begin().await.unwrap();

        let err = add_message(&mut conn, &alice, &bob, &NewChatMessage::text(&too_long))
            .await
            .expect_err("one character over the limit is refused");

        assert_eq!(
            err.to_string(),
            format!(
                "Chat messages are limited to {MAX_CHAT_MESSAGE_CHARS} characters (got {})",
                MAX_CHAT_MESSAGE_CHARS + 1
            ),
            "the refusal names the limit and what it saw"
        );
        drop(conn);
        assert!(
            chats(&pool, alice.id).await.is_empty(),
            "the refusal created no chat"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM chat_messages").await,
            0,
            "and no message"
        );

        // The limit itself is allowed, and it counts characters, not bytes:
        // five thousand of the three-byte euro sign fit.
        let exactly_limit = "€".repeat(MAX_CHAT_MESSAGE_CHARS);
        let (message, _, _) = send(&pool, &alice, &bob, &exactly_limit).await;
        assert_eq!(
            message.raw_text.chars().count(),
            MAX_CHAT_MESSAGE_CHARS,
            "so {MAX_CHAT_MESSAGE_CHARS} characters go through"
        );
    }

    #[tokio::test]
    async fn message_pages_walk_by_reference_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let mut sent = Vec::new();
        for index in 0..5 {
            let (message, reference, _) =
                send(&pool, &alice, &bob, &format!("message {index}")).await;
            sent.push((message, reference));
        }
        let chat_id = sent.first().expect("five messages").1.chat_id;
        let ids = newest_by_id(
            &sent
                .iter()
                .map(|(_, reference)| reference.id)
                .collect::<Vec<Uuid>>(),
        );
        let newest = ids.first().expect("five ids").to_string();
        let oldest = ids.last().expect("five ids").to_string();

        let first_page = messages_page(&pool, chat_id, page("limit=2"))
            .await
            .unwrap();
        assert_eq!(first_page.len(), 2, "the limit holds the page down");
        assert_eq!(
            reference_ids(&first_page),
            vec![ids[0], ids[1]],
            "the newest two references, newest first"
        );

        let second_page =
            messages_page(&pool, chat_id, page(&format!("limit=2&max_id={}", ids[1])))
                .await
                .unwrap();
        assert_eq!(
            reference_ids(&second_page),
            vec![ids[2], ids[3]],
            "max_id walks back through the chat"
        );

        let since = messages_page(&pool, chat_id, page(&format!("since_id={oldest}")))
            .await
            .unwrap();
        assert_eq!(
            reference_ids(&since),
            vec![ids[0], ids[1], ids[2], ids[3]],
            "since_id skips the cursor and everything older"
        );

        let previous = messages_page(&pool, chat_id, page(&format!("limit=2&min_id={}", ids[3])))
            .await
            .unwrap();
        assert_eq!(
            reference_ids(&previous),
            vec![ids[1], ids[2]],
            "min_id answers the page just after the cursor, newest first again"
        );

        assert!(
            messages_page(&pool, chat_id, page(&format!("max_id={oldest}")))
                .await
                .unwrap()
                .is_empty(),
            "nothing is older than the oldest reference"
        );
        assert!(
            messages_page(&pool, chat_id, page(&format!("min_id={newest}")))
                .await
                .unwrap()
                .is_empty(),
            "nothing is newer than the newest one"
        );
        assert!(
            messages_page(&pool, new_id(), page(ALL))
                .await
                .unwrap()
                .is_empty(),
            "a chat nobody has written in has no messages"
        );
    }

    #[tokio::test]
    async fn chat_pages_walk_by_activity_and_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let mut opened = Vec::new();
        for nickname in ["bob", "carol", "dave"] {
            let partner = local(&pool, nickname).await;
            let (_, reference, _) = send(&pool, &alice, &partner, "hello").await;
            opened.push(reference.chat_id);
        }

        let all = page_for_user(&pool, alice.id, page(ALL), ChatFilter::default())
            .await
            .unwrap();
        assert_eq!(
            chat_ids(&all),
            vec![opened[2], opened[1], opened[0]],
            "the list is newest activity first"
        );

        let first = page_for_user(&pool, alice.id, page("limit=2"), ChatFilter::default())
            .await
            .unwrap();
        assert_eq!(first.len(), 2, "the limit holds the page down");
        assert_eq!(
            chat_ids(&first),
            vec![opened[2], opened[1]],
            "the two newest ids, ordered by activity"
        );

        let second = page_for_user(
            &pool,
            alice.id,
            page(&format!("limit=2&max_id={}", opened[1])),
            ChatFilter::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            chat_ids(&second),
            vec![opened[0]],
            "max_id walks back to the oldest chat"
        );

        let since = page_for_user(
            &pool,
            alice.id,
            page(&format!("since_id={}", opened[1])),
            ChatFilter::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            chat_ids(&since),
            vec![opened[2]],
            "since_id keeps only the chats with a newer id"
        );

        assert!(
            page_for_user(
                &pool,
                alice.id,
                page(&format!("max_id={}", opened[0])),
                ChatFilter::default(),
            )
            .await
            .unwrap()
            .is_empty(),
            "nothing is older than the oldest chat"
        );
        assert!(
            page_for_user(&pool, new_id(), page(ALL), ChatFilter::default())
                .await
                .unwrap()
                .is_empty(),
            "and a user with no chats gets no rows"
        );
    }

    #[tokio::test]
    async fn removing_a_reference_hides_a_message_from_one_side_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let (message, alice_ref, recipient_ref) = send(&pool, &alice, &bob, "alice says").await;
        let bobs_ref = recipient_ref.expect("bob is local");

        assert!(
            !delete_message_for(&pool, alice.id, bobs_ref.id)
                .await
                .unwrap(),
            "alice has no reference of bob's to delete"
        );
        assert!(
            !delete_message_for(&pool, new_id(), alice_ref.id)
                .await
                .unwrap(),
            "nor has a stranger"
        );
        assert!(
            delete_message_for(&pool, bob.id, bobs_ref.id)
                .await
                .unwrap(),
            "bob's own reference goes"
        );

        assert!(
            messages(&pool, bobs_ref.chat_id).await.is_empty(),
            "bob's list no longer shows it"
        );
        assert_eq!(
            message_ids(&messages(&pool, alice_ref.chat_id).await),
            vec![message.id],
            "alice still sees the message she wrote"
        );
        assert_eq!(
            stored_message(&pool, message.id).await,
            Some(message.clone()),
            "and it was not withdrawn: hiding a copy is not deleting the message"
        );
        assert!(
            get_owned(&pool, bob.id, bobs_ref.chat_id)
                .await
                .unwrap()
                .is_some(),
            "bob keeps his chat row, empty of messages"
        );
        assert_eq!(
            unread_count(&pool, alice_ref.chat_id).await.unwrap(),
            0,
            "alice has nothing unread to begin with"
        );
        assert!(
            !delete_message_for(&pool, bob.id, bobs_ref.id)
                .await
                .unwrap(),
            "deleting the same reference again finds nothing"
        );
    }

    #[tokio::test]
    async fn a_sender_withdrawing_a_message_takes_it_from_everyone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let (message, alice_ref, recipient_ref) = send(&pool, &alice, &bob, "take it back").await;
        let bobs_ref = recipient_ref.expect("bob is local");

        assert!(
            delete_message_for(&pool, alice.id, alice_ref.id)
                .await
                .unwrap(),
            "alice withdraws her message"
        );

        let tombstone = stored_message(&pool, message.id)
            .await
            .expect("the row is still there");
        assert!(tombstone.deleted_at.is_some(), "it is a tombstone now");
        assert_eq!(tombstone.content, "", "its content is cleared");
        assert_eq!(tombstone.raw_text, "", "and its source text");
        assert_eq!(tombstone.sender_id, alice.id, "everything else survives");
        assert!(
            messages(&pool, bobs_ref.chat_id).await.is_empty(),
            "bob no longer sees it: {:?}",
            message_ids(&messages(&pool, bobs_ref.chat_id).await)
        );
        assert!(
            messages(&pool, alice_ref.chat_id).await.is_empty(),
            "nor does alice, whose own reference went with it"
        );
        assert_eq!(
            unread_count(&pool, bobs_ref.chat_id).await.unwrap(),
            1,
            "the dangling reference is counted until bob clears it"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM chat_message_references").await,
            1,
            "and it is the only reference left"
        );
        assert!(
            !delete_message_for(&pool, alice.id, alice_ref.id)
                .await
                .unwrap(),
            "withdrawing twice finds nothing to withdraw"
        );
    }

    #[tokio::test]
    async fn marking_a_chat_read_walks_the_unread_flags() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let mut references = Vec::new();
        for index in 0..3 {
            let (_, _, recipient_ref) = send(&pool, &alice, &bob, &format!("ping {index}")).await;
            references.push(recipient_ref.expect("bob is local"));
        }
        let chat_id = references.first().expect("three messages").chat_id;
        let ids = newest_by_id(
            &references
                .iter()
                .map(|reference| reference.id)
                .collect::<Vec<Uuid>>(),
        );
        let second_oldest = ids[ids.len() - 2];

        assert_eq!(
            unread_count(&pool, chat_id).await.unwrap(),
            3,
            "three unread messages"
        );
        assert_eq!(
            mark_read(&pool, chat_id, Some(second_oldest))
                .await
                .unwrap(),
            2,
            "the two oldest messages are read off"
        );
        assert_eq!(
            unread_count(&pool, chat_id).await.unwrap(),
            1,
            "the newest one is left"
        );
        assert_eq!(
            mark_read(&pool, chat_id, Some(second_oldest))
                .await
                .unwrap(),
            0,
            "marking the same point again reads nothing"
        );
        assert_eq!(
            mark_read(&pool, chat_id, None).await.unwrap(),
            1,
            "with no marker the whole chat is read"
        );
        assert_eq!(
            unread_count(&pool, chat_id).await.unwrap(),
            0,
            "nothing is unread now"
        );

        let (_, _, late_ref) = send(&pool, &alice, &bob, "late").await;
        assert!(
            late_ref.expect("bob is local").unread,
            "a message after the marker is unread"
        );
        assert_eq!(
            mark_read(&pool, chat_id, Some(second_oldest))
                .await
                .unwrap(),
            0,
            "and the old marker does not reach it"
        );
        assert_eq!(
            unread_count(&pool, chat_id).await.unwrap(),
            1,
            "so it stays unread"
        );
        assert_eq!(
            mark_read(&pool, new_id(), None).await.unwrap(),
            0,
            "an unknown chat has nothing to mark"
        );
    }

    #[tokio::test]
    async fn marking_one_message_read_returns_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let (message, alice_ref, recipient_ref) = send(&pool, &alice, &bob, "read me").await;
        let unread = recipient_ref.expect("bob is local");

        assert!(
            mark_message_read(&pool, alice_ref.chat_id, unread.id)
                .await
                .unwrap()
                .is_none(),
            "a reference of bob's chat is nothing for alice's chat to mark"
        );
        assert!(
            reference_still_unread(&pool, unread.id).await,
            "and the refused mark left it unread"
        );

        let read = mark_message_read(&pool, unread.chat_id, unread.id)
            .await
            .unwrap()
            .expect("the reference exists in its own chat");

        assert_eq!(read.id, unread.id, "the same reference");
        assert!(!read.unread, "now read");
        assert_eq!(read.message_id, message.id, "still pointing at the message");
        assert_eq!(read.content, unread.content, "its text is as it was");
        assert_eq!(read.chat_id, unread.chat_id, "and its chat");
        assert_eq!(
            unread_count(&pool, read.chat_id).await.unwrap(),
            0,
            "so the chat counts nothing unread"
        );
        let again = mark_message_read(&pool, read.chat_id, read.id)
            .await
            .unwrap()
            .expect("marking it twice still finds it");
        assert_eq!(again, read, "and changes nothing");
        assert!(
            mark_message_read(&pool, unread.chat_id, new_id())
                .await
                .unwrap()
                .is_none(),
            "an unknown reference is nothing to mark"
        );
        assert!(
            mark_message_read(&pool, new_id(), unread.id)
                .await
                .unwrap()
                .is_none(),
            "and so is a known reference under an unknown chat"
        );
    }

    /// Whether reference `ref_id` still carries its unread flag.
    async fn reference_still_unread(pool: &PgPool, ref_id: Uuid) -> bool {
        let row: (bool,) =
            sqlx::query_as("SELECT unread FROM chat_message_references WHERE id = $1")
                .bind(ref_id)
                .fetch_one(pool)
                .await
                .unwrap();
        row.0
    }

    /// Moves message `id`'s `inserted_at` two hours into the past — outside
    /// the idempotency replay window, whatever the machine's clock speed.
    async fn expire_message(pool: &PgPool, id: Uuid) {
        sqlx::query(
            "UPDATE chat_messages SET inserted_at = now() - interval '2 hours' WHERE id = $1",
        )
        .bind(id)
        .execute(pool)
        .await
        .map(drop)
        .expect("the backdate runs");
    }

    #[tokio::test]
    async fn an_idempotency_key_rides_the_row_and_replays_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;

        let (message, alice_ref, bob_ref) = send_with(
            &pool,
            &alice,
            &bob,
            &NewChatMessage {
                idempotency_key: Some("key-1".to_owned()),
                ..NewChatMessage::text("once")
            },
        )
        .await;

        assert_eq!(
            message.idempotency_key.as_deref(),
            Some("key-1"),
            "the column keeps the key"
        );
        assert_eq!(
            alice_ref.idempotency_key.as_deref(),
            Some("key-1"),
            "the joined view carries it"
        );
        assert_eq!(
            bob_ref.expect("bob is local").idempotency_key.as_deref(),
            Some("key-1"),
            "on both sides"
        );

        let replay = sender_reference_for_key(&pool, alice.id, "key-1")
            .await
            .unwrap()
            .expect("the key replays");
        assert_eq!(replay.id, alice_ref.id, "as the sender's own reference");
        assert_eq!(replay.chat_id, alice_ref.chat_id, "in its own chat");
        assert!(!replay.unread, "which the sender has read");

        assert!(
            sender_reference_for_key(&pool, alice.id, "other-key")
                .await
                .unwrap()
                .is_none(),
            "an unknown key replays nothing"
        );
        assert!(
            sender_reference_for_key(&pool, bob.id, "key-1")
                .await
                .unwrap()
                .is_none(),
            "and never for another user, holder of the same key or not"
        );

        // A keyless message stores no key, and NULL never answers a lookup —
        // not even for the empty key a client might send.
        let (keyless, _, _) = send(&pool, &alice, &bob, "no key").await;
        assert!(keyless.idempotency_key.is_none(), "no key on the row");
        assert!(
            sender_reference_for_key(&pool, alice.id, "")
                .await
                .unwrap()
                .is_none(),
            "and the empty key matches nothing"
        );

        expire_message(&pool, message.id).await;
        assert!(
            sender_reference_for_key(&pool, alice.id, "key-1")
                .await
                .unwrap()
                .is_none(),
            "an hour out of the window replays nothing"
        );
    }

    #[tokio::test]
    async fn a_withdrawn_message_is_not_replayed_and_a_dropped_reference_goes_with_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let (message, alice_ref, _) = send_with(
            &pool,
            &alice,
            &bob,
            &NewChatMessage {
                idempotency_key: Some("gone".to_owned()),
                ..NewChatMessage::text("take it back")
            },
        )
        .await;

        assert!(
            sender_reference_for_key(&pool, alice.id, "gone")
                .await
                .unwrap()
                .is_some(),
            "the key starts out replayable"
        );
        assert!(
            delete_message_for(&pool, alice.id, alice_ref.id)
                .await
                .unwrap(),
            "alice withdraws the message"
        );
        assert!(
            sender_reference_for_key(&pool, alice.id, "gone")
                .await
                .unwrap()
                .is_none(),
            "gone is no longer a replayable key: tombstone and sender's \
             reference both went, so the next same-key post writes afresh"
        );
        assert_eq!(
            message.idempotency_key.as_deref(),
            Some("gone"),
            "the key itself stays on the row — it is the message that is gone"
        );
    }

    #[tokio::test]
    async fn a_reference_is_only_its_own_chats() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let (message, alice_ref, bob_ref) = send(&pool, &alice, &bob, "pair").await;
        let bobs = bob_ref.expect("bob is local");

        let own = message_reference(&pool, alice_ref.chat_id, alice_ref.id)
            .await
            .unwrap()
            .expect("a chat holds its own reference");
        assert_eq!(own, alice_ref, "as the joined view it is");
        assert_eq!(own.message_id, message.id, "pointing at the message");
        assert!(
            message_reference(&pool, bobs.chat_id, alice_ref.id)
                .await
                .unwrap()
                .is_none(),
            "alice's reference is not in bob's chat"
        );
        assert!(
            message_reference(&pool, alice_ref.chat_id, bobs.id)
                .await
                .unwrap()
                .is_none(),
            "nor his in hers"
        );
        assert!(
            message_reference(&pool, new_id(), alice_ref.id)
                .await
                .unwrap()
                .is_none(),
            "and an unknown chat holds nothing"
        );
    }

    #[test]
    fn chat_content_is_escaped_text_with_links() {
        assert_eq!(
            render_chat_content("a <b>tag</b> & an &amp;"),
            "<p>a &lt;b&gt;tag&lt;/b&gt; &amp; an &amp;amp;</p>",
            "markup is escaped, not kept"
        );
        assert_eq!(
            render_chat_content("see https://example.com/a?b=c now"),
            "<p>see <a href=\"https://example.com/a?b=c\" rel=\"ugc nofollow noopener noreferrer\">\
             example.com/a?b=c</a> now</p>",
            "bare urls are linkified, with the rel the sanitizer wants"
        );
        assert_eq!(
            render_chat_content("# heading\n\nsecond"),
            "<p># heading</p><p>second</p>",
            "markdown stays plain text"
        );
        assert_eq!(
            render_chat_content("one\ntwo"),
            "<p>one<br>two</p>",
            "a newline breaks the line"
        );
        assert_eq!(
            NewChatMessage::text("hi *there*").raw_text,
            "hi *there*",
            "the source text is kept verbatim"
        );
        assert_eq!(
            NewChatMessage::text("hi *there*").content,
            "<p>hi *there*</p>",
            "and the emphasis marker is not a tag"
        );
        assert_eq!(
            NewChatMessage::default().content,
            "",
            "a default new message carries nothing"
        );
        assert!(
            NewChatMessage::text("x").idempotency_key.is_none(),
            "and neither text() nor the default sets an idempotency key"
        );
        assert!(
            NewChatMessage::default().idempotency_key.is_none(),
            "on either construction"
        );
    }

    #[tokio::test]
    async fn a_message_can_carry_an_attachment_and_an_ap_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let attachment = insert_attachment(&pool, &alice).await;
        let ap_id = format!("{TEST_BASE_URL}/objects/{}", new_id());

        let (message, alice_ref, bob_ref) = {
            let mut conn = pool.begin().await.unwrap();
            let written = add_message(
                &mut conn,
                &alice,
                &bob,
                &with_extras("look at this", attachment, &ap_id),
            )
            .await
            .expect("a message with an attachment and an id is storable");
            conn.commit().await.unwrap();
            written
        };

        assert_eq!(
            message.attachment_media_id,
            Some(attachment),
            "the attachment is remembered"
        );
        assert_eq!(message.ap_id.as_deref(), Some(ap_id.as_str()), "and the id");
        assert_eq!(message.raw_text, "look at this", "with its text");
        assert_eq!(
            alice_ref.attachment_media_id,
            Some(attachment),
            "the joined view carries the attachment too"
        );
        assert_eq!(
            bob_ref.expect("bob is local").ap_id.as_deref(),
            Some(ap_id.as_str()),
            "and the id, on both sides"
        );
        assert!(
            alice_ref.raw_text.starts_with("look"),
            "the reference row reports the message, not the reference: {}",
            alice_ref.raw_text
        );
    }

    #[tokio::test]
    async fn an_empty_message_is_storable() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let mut conn = pool.begin().await.unwrap();

        let (message, alice_ref, _) = add_message(
            &mut conn,
            &alice,
            &bob,
            &NewChatMessage {
                attachment_media_id: Some(insert_attachment(&pool, &alice).await),
                ..NewChatMessage::default()
            },
        )
        .await
        .expect("the column defaults let a message carry only an attachment");
        conn.commit().await.unwrap();

        assert_eq!(message.content, "", "empty content stays empty");
        assert_eq!(message.raw_text, "", "and so does the source text");
        assert_eq!(alice_ref.content, "", "which the listing shows as it is");
    }

    /// A media row of `uploader`, for the attachment column to point at.
    async fn insert_attachment(pool: &PgPool, uploader: &User) -> Uuid {
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

    #[tokio::test]
    async fn a_reused_message_id_is_refused() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let ap_id = format!("{TEST_BASE_URL}/objects/{}", new_id());
        let attachment = insert_attachment(&pool, &alice).await;

        // A reused message id is how a redelivered remote message would come
        // back twice, and only the unique column can stop it.
        let mut first = pool.begin().await.unwrap();
        let stored = add_message(
            &mut first,
            &alice,
            &bob,
            &with_extras("first use", attachment, &ap_id),
        )
        .await
        .expect("the first use of a message id is stored");
        first.commit().await.unwrap();
        assert_eq!(
            stored.0.ap_id.as_deref(),
            Some(ap_id.as_str()),
            "the fixture message carries the id"
        );

        let mut retry = pool.begin().await.unwrap();
        let duplicate = add_message(
            &mut retry,
            &alice,
            &bob,
            &with_extras("again", attachment, &ap_id),
        )
        .await
        .expect_err("one message id is one message");
        drop(retry);
        assert_eq!(
            sqlstate_of(duplicate),
            "23505",
            "the refusal is the unique chat_messages.ap_id"
        );
    }

    #[tokio::test]
    async fn media_that_is_not_a_row_cannot_be_attached() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;

        let mut conn = pool.begin().await.unwrap();
        let foreign = add_message(
            &mut conn,
            &alice,
            &bob,
            &NewChatMessage {
                attachment_media_id: Some(new_id()),
                ..NewChatMessage::text("attach this")
            },
        )
        .await
        .expect_err("media that does not exist cannot be attached");
        drop(conn);
        assert_eq!(
            sqlstate_of(foreign),
            "23503",
            "the refusal is the foreign key on media_attachments"
        );
    }

    #[tokio::test]
    async fn chats_key_their_owners_and_their_pairs() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;

        let unknown =
            sqlx::query("INSERT INTO chats (id, user_id, recipient_id) VALUES ($1, $2, $3)")
                .bind(new_id())
                .bind(alice.id)
                .bind(new_id())
                .execute(&pool)
                .await
                .expect_err("a chat needs two users this instance knows");
        assert_eq!(
            sqlstate_of(AppError::from(unknown)),
            "23503",
            "the refusal is the foreign key on users"
        );

        let first_chat: Chat = get_or_create(&pool, &alice, &bob).await.unwrap();
        assert_eq!(
            first_chat.recipient_id, bob.id,
            "the model's row is the one the key rejects"
        );
        let second =
            sqlx::query("INSERT INTO chats (id, user_id, recipient_id) VALUES ($1, $2, $3)")
                .bind(new_id())
                .bind(alice.id)
                .bind(bob.id)
                .execute(&pool)
                .await
                .expect_err("one participant has one chat per account");
        assert_eq!(
            sqlstate_of(AppError::from(second)),
            "23505",
            "the refusal is the unique (user_id, recipient_id)"
        );
    }

    #[tokio::test]
    async fn deleting_a_sender_takes_their_chats_and_messages() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let (message, _, bobs_ref) = send(&pool, &alice, &bob, "bye").await;
        let bobs_chat = bobs_ref.expect("bob's side of the conversation");

        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .map(drop)
            .expect("alice's account is deleted");

        assert_eq!(
            count(&pool, "SELECT count(*) FROM chats").await,
            0,
            "both sides' chat rows went with the accounts they name"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM chat_message_references").await,
            0,
            "and the references with them"
        );
        assert!(
            stored_message(&pool, message.id).await.is_none(),
            "alice's message went with its sender"
        );
        assert!(
            get_owned(&pool, bob.id, bobs_chat.chat_id)
                .await
                .unwrap()
                .is_none(),
            "bob keeps no view of the conversation"
        );
    }

    #[tokio::test]
    async fn deleting_an_attachment_leaves_the_message_standing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let attachment = insert_attachment(&pool, &alice).await;
        let (message, alice_ref, _) = {
            let mut conn = pool.begin().await.unwrap();
            let written = add_message(
                &mut conn,
                &alice,
                &bob,
                &with_extras("look at this", attachment, "https://remote.example/cm/1"),
            )
            .await
            .expect("the message stores with its attachment");
            conn.commit().await.unwrap();
            written
        };
        assert_eq!(
            alice_ref.attachment_media_id,
            Some(attachment),
            "the listing starts out with the attachment"
        );

        sqlx::query("DELETE FROM media_attachments WHERE id = $1")
            .bind(attachment)
            .execute(&pool)
            .await
            .map(drop)
            .expect("the attachment is deleted");

        let orphaned = stored_message(&pool, message.id)
            .await
            .expect("the message survives its attachment");
        assert!(
            orphaned.attachment_media_id.is_none(),
            "and is left without it"
        );
        assert_eq!(orphaned.raw_text, "look at this", "its text is intact");
        let listed = messages(&pool, alice_ref.chat_id).await;
        assert_eq!(
            message_ids(&listed),
            vec![message.id],
            "the message is still in the listing"
        );
        assert!(
            listed
                .first()
                .expect("one message is listed")
                .attachment_media_id
                .is_none(),
            "only its attachment is gone"
        );
    }
}
