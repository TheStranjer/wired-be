//! The chat API over HTTP (`/api/v1/pleroma/chats` and its v2 list,
//! WBE-146, with its read-marker and pinning half, WBE-148, and its
//! messages, WBE-147): Pleroma's chat endpoints in full.
//!
//! The reference is `Pleroma.Web.PleromaAPI.ChatController`
//! (`lib/pleroma/web/pleroma_api/controllers/chat_controller.ex`), whose
//! `create`, `index`, `index2`, `show`, `messages`, `post_chat_message`,
//! `delete_message`, `mark_as_read`, `mark_message_as_read`, `pin` and
//! `unpin` actions these routes replicate; the state they change lives in
//! the three chat tables behind [`crate::models::chat`], and the JSON they
//! answer is the [`Chat`] and [`ChatMessage`] entities (decision 3).
//!
//! - `POST /api/v1/pleroma/chats/by-account-id/{id}` — get or create the
//!   caller's chat with that account (Pleroma's `create` +
//!   `Chat.get_or_create`) and answer the [`Chat`] entity with 200. The
//!   account must exist and be active (one 404, [`account_or_404`]), the
//!   pair is refused by `chat_refusal` (self, the recipient's chat flag,
//!   blocks), and the write is idempotent: a second call answers the same
//!   chat, unmoved in its owner's list.
//! - `GET /api/v1/pleroma/chats` — the legacy list (Pleroma's `index`,
//!   whose api spec deprecates it for wanting a page): the caller's chats
//!   ordered by activity, one [`PageParams`] page of at most 30 under the
//!   `with_muted` and `pinned` filters, with the `Link` header keyed on the
//!   chat ids of the returned page.
//! - `GET /api/v2/pleroma/chats` — Pleroma's `index2`: the same query
//!   paginated for real; it orders by the chat's last message time, which
//!   this schema keeps in
//!   [`updated_at`](crate::models::chat::Chat::updated_at) (messages bump
//!   it, opening a chat does not), and its cursors are chat ids.
//! - `GET /api/v1/pleroma/chats/{id}` — Pleroma's `show`: one owned chat as
//!   the [`Chat`] entity; a malformed, unknown or foreign id is one 404.
//! - `GET /api/v1/pleroma/chats/{id}/messages` — Pleroma's `messages`: the
//!   owned chat's [`ChatMessage`] entities, newest first, one [`PageParams`]
//!   page (default 20, capped at [`MAX_LIMIT`]) under a `Link` header keyed
//!   on the reference ids of the page.
//! - `POST /api/v1/pleroma/chats/{id}/messages` — Pleroma's
//!   `post_chat_message`: the body carries `content` (at most
//!   [`max_chat_message_chars`] characters) and/or `media_id` (one of the
//!   poster's own media rows that no status has claimed and whose type is
//!   image, video or audio, attached to the message); neither key present
//!   is the 400 `{"error":"Cannot send an empty chat message"}`, oversize
//!   content the 400 `{"error":"The message is too long"}`, and media the
//!   poster may not attach the 400 `{"error":"Media not found"}`. The write
//!   is [`chat::add_message`] on one transaction, answered with the
//!   sender's [`ChatMessage`] entity. An `Idempotency-Key` header is stored
//!   with the message row (plus the sender: [`chat::sender_reference_for_key`]
//!   is the lookup), so replaying the same key within the hour answers the
//!   message that key created instead of writing a second one — Pleroma
//!   keeps that mapping in a TTL cache in its posting pipeline. Before
//!   anything is written the pair goes through `chat_refusal` again, so a
//!   chat opened before a block, or whose recipient has since stopped
//!   accepting chats, answers the same 403 it would at creation.
//! - `DELETE /api/v1/pleroma/chats/{id}/messages/{message_id}` — Pleroma's
//!   `delete_message`: `message_id` is a reference id, and it resolves only
//!   inside the caller's own chat (foreign, unknown and malformed ids are
//!   one 404). [`chat::delete_message_for`] hides the message from the
//!   caller and, when they wrote it, withdraws it from everyone — the
//!   answer is the deleted message entity as the caller saw it, rendered
//!   before the delete ran.
//! - `POST /api/v1/pleroma/chats/{id}/read` — body `last_read_id` (required):
//!   marks every unread reference of the caller's chat `id` with
//!   `id <= last_read_id` read (Pleroma's `set_all_seen_for_chat`) and answers
//!   the chat with its new `unread` count.
//! - `POST /api/v1/pleroma/chats/{id}/messages/{message_id}/read` — marks one
//!   reference of that chat read and answers the message entity.
//! - `POST /api/v1/pleroma/chats/{id}/pin` and `…/unpin` — set
//!   `chats.pinned` (Pleroma's `Chat.pin`/`Chat.unpin`) and answer the chat.
//!
//! Gates, in Pleroma's order: a user token ([`AuthUser`]) granting
//! `read:chats` on the four reading routes and `write:chats` on the seven
//! writing ones (its `OAuthScopesPlug` pipeline), then the body of the
//! whole-chat read — without `last_read_id` a request is the 400
//! `{"error":"Missing parameter: last_read_id"}` of that controller's
//! fallback clause, checked before anything is looked up; a marker that is
//! present but not an id is 400 `"Invalid parameter: last_read_id"`,
//! Pleroma's own uuid cast having no sane answer here. `{id}` (and `{message_id}`)
//! resolve through [`crate::models::chat::get_owned`], so a malformed,
//! unknown and foreign id are one and the same 404 "Record not found": each
//! participant's chat has its own id and never shares it, and one reference
//! is only markable — or deletable — through the chat that holds it:
//! Pleroma's `mark_message_as_read` and `delete_message` `with` clauses
//! answer the same way.
//!
//! The read handlers finish with `after_chat_read`, which pushes Pleroma's
//! `pleroma:chat_update` over the reader's chat socket stream; the pin
//! routes broadcast nothing there and so call nothing here. The message
//! routes finish with `after_chat_message_created` and
//! `after_chat_message_deleted`: each tells every LOCAL side of the chat
//! over the socket — a `pleroma:chat_update` on that side's own chat stream,
//! each side carrying its own chat row — and announces the change to a
//! REMOTE counterpart over the wire, a message posted to them as a
//! `Create{ChatMessage}` and the sender's delete of it as a `Delete` of that
//! object ([`federation_out`]'s chat arms — Pleroma's `post_chat_message` and
//! `delete_message` pipelines); a local counterpart is told by their rows
//! alone, never over HTTP. The recipient's notification stays with the
//! notification's own producer.
//!
//! [`max_chat_message_chars`]: crate::config::Config::max_chat_message_chars
//! [`federation_out`]: crate::services::federation_out
//! [`Chat`]: crate::entities::chat::Chat
//! [`ChatMessage`]: crate::entities::chat::ChatMessage
//! [`account_or_404`]: crate::api::helpers::account_or_404
//! [`AuthUser`]: crate::auth::AuthUser
//! [`MAX_LIMIT`]: crate::pagination::MAX_LIMIT
//! [`chat::add_message`]: crate::models::chat::add_message
//! [`chat::delete_message_for`]: crate::models::chat::delete_message_for
//! [`chat::sender_reference_for_key`]: crate::models::chat::sender_reference_for_key

use axum::{
    Router,
    http::HeaderName,
    routing::{delete, get, post},
};

use crate::{pagination::PageParams, state::AppState};

use self::{
    chats_list::{create_chat, list_chats, list_chats_v2, show_chat},
    messages::{delete_message, list_messages},
    post_message::post_message,
    read_pin::{mark_chat_read, mark_message_read, pin_chat, unpin_chat},
};

pub(crate) use self::{
    chats_list::{chat_entities, chat_entity},
    events::after_chat_message_created,
    messages::message_of,
};

pub mod chats_list;
pub mod events;
pub mod messages;
mod post_message;
mod read_pin;
#[cfg(test)]
mod test_support;

/// 400 body when the whole-chat read arrives without its marker (Pleroma's
/// `mark_as_read` fallback clause, verbatim).
const MISSING_LAST_READ_ID: &str = "Missing parameter: last_read_id";
/// 400 body when `last_read_id` is present but not an id: Pleroma's
/// `CastAndValidate` plug casts it as `Ecto.UUID` and the comparison below it
/// has no answer for anything else, so this endpoint names the parameter it
/// could not use in the same 400 shape.
const INVALID_LAST_READ_ID: &str = "Invalid parameter: last_read_id";
/// The one-row page that reads a chat's newest live message, what Pleroma's
/// `MessageReference.last_message_for_chat` asks its chat query for.
const NEWEST_PAGE: PageParams = PageParams {
    max_id: None,
    min_id: None,
    since_id: None,
    limit: 1,
    offset: None,
};
/// 422 body when the caller names themselves as the chat recipient
/// (Pleroma's `validate_no_self_chat` changeset error, in the ticket's
/// spelling).
const CANT_CHAT_WITH_SELF: &str = "Can't chat with yourself";
/// 403 body when the recipient's [`accepts_chat_messages`] flag is down
/// (Pleroma lumps this into its `Chat not allowed` refusal too; the ticket
/// gives the flag its own wording).
///
/// [`accepts_chat_messages`]: crate::models::user::User::accepts_chat_messages
const RECIPIENT_REJECTS_CHATS: &str = "Recipient doesn't accept chat messages";
/// 403 body when one side of the pair has blocked the other (Pleroma's
/// `{:error, :chat_not_allowed}` fallback).
const CHAT_NOT_ALLOWED: &str = "Chat not allowed";
/// The `limit` cap of the legacy `GET /api/v1/pleroma/chats` (the ticket's
/// 20-default, 30-maximum page; [`MAX_LIMIT`] caps the v2 list, like
/// Pleroma's own `Pagination` module).
///
/// [`MAX_LIMIT`]: crate::pagination::MAX_LIMIT
const LEGACY_MAX_LIMIT: i64 = 30;
/// 400 body when the caller posted a message with neither `content` nor
/// `media_id` (Pleroma's `validate_chat_content_length` `{:error,
/// :no_content}`, in the ticket's spelling).
const EMPTY_CHAT_MESSAGE: &str = "Cannot send an empty chat message";
/// 400 body when `content` is longer than the
/// [`max_chat_message_chars`](crate::config::Config::max_chat_message_chars)
/// setting (Pleroma's `{:error, :content_too_long}`, ticket wording).
const MESSAGE_TOO_LONG: &str = "The message is too long";
/// 400 body when `media_id` names no media the poster may attach — no such
/// row, not theirs, claimed by a status, or no image/video/audio type
/// (Pleroma's media authorization; the ticket gives them one wording).
const MEDIA_NOT_FOUND: &str = "Media not found";
/// The header [`post_message`] reads its replay key from (Pleroma's
/// `idempotency_key/1`).
const IDEMPOTENCY_KEY_HEADER: HeaderName = HeaderName::from_static("idempotency-key");
/// The `/api/v1/pleroma/chats` router, nested into [`crate::api::v1`].
///
/// Do not call [`Router::fallback`] here (or on anything merged into this
/// tree): the app-wide JSON fallback in [`crate::app`] must answer the
/// unmatched paths.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_chats))
        .route("/by-account-id/{id}", post(create_chat))
        .route("/{id}", get(show_chat))
        .route("/{id}/read", post(mark_chat_read))
        .route("/{id}/messages", get(list_messages).post(post_message))
        .route("/{id}/messages/{message_id}", delete(delete_message))
        .route("/{id}/messages/{message_id}/read", post(mark_message_read))
        .route("/{id}/pin", post(pin_chat))
        .route("/{id}/unpin", post(unpin_chat))
}

/// The `/api/v2/pleroma/chats` router, nested into [`crate::api::v2`].
///
/// The second version is the paged sibling of [`router`]'s legacy list
/// (Pleroma's `index` vs `index2`); the per-chat routes live under
/// `/api/v1` only, which is why this router holds the one route.
pub fn v2_router() -> Router<AppState> {
    Router::new().route("/", get(list_chats_v2))
}
