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
//!   keeps that mapping in a TTL cache in its posting pipeline. Pleroma
//!   runs this action through its `statuses_actions` throttle bucket
//!   (`config/config.exs`: ten thousand requests per fifteen seconds);
//!   this server has no rate limiter yet, and the ticket that adds one
//!   registers this route under that bucket name.
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
//! The read handlers finish with `after_chat_read`, the documented empty
//! hook where Pleroma pushes its `chat_read` socket event; the pin routes
//! broadcast nothing there and so call nothing here. The message routes
//! finish with their own documented empty hooks, `after_chat_message_created`
//! and `after_chat_message_deleted`: Pleroma's chat sockets, recipient
//! notifications and federation hang off the created and deleted message,
//! and the streaming, notification and federation tickets attach them
//! there.
//!
//! [`max_chat_message_chars`]: crate::config::Config::max_chat_message_chars

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, HeaderName},
    routing::{delete, get, post},
};
use serde::Deserialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::account_or_404,
    auth::AuthUser,
    entities::chat::{Chat, ChatMessage},
    error::{AppError, AppResult},
    extract::{ApiBody, ApiParams, flex},
    models::{
        chat,
        chat::{ChatFilter, ChatMessageReference, NewChatMessage},
        media_attachment,
        user::{self, User},
    },
    oauth::scopes::{READ_CHATS, WRITE_CHATS},
    pagination::{MAX_LIMIT, PageParams, link_header, set_link_header},
    state::AppState,
    util::id::parse_id,
};

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
/// The `Link` header's query parameters carry nothing but the cursor: the
/// list's filters are not repeated on either link, as in no other paged
/// listing of this API.
const NO_EXTRA_QUERY: [(String, String); 0] = [];

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

/// `POST /api/v1/pleroma/chats/by-account-id/{id}` — get or create the
/// caller's chat with that account and answer the [`Chat`] entity (200
/// whether the chat was made here or already existed: Pleroma's `create`
/// renders `show.json` either way).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:chats` check; [`AppError::NotFound`] when `id` is malformed,
/// names no account or names a deactivated one ([`account_or_404`]: the
/// ticket's "unknown account 404" and active-account requirement);
/// whatever [`chat_refusal`] decides the pair is owed. Any database failure
/// arrives through `?` as [`AppError::Database`].
async fn create_chat(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Chat>> {
    auth.require(WRITE_CHATS)?;
    let recipient = account_or_404(&state, &id).await?;
    if let Some(refusal) = chat_refusal(&auth.0, &recipient) {
        return Err(refusal);
    }
    let row = chat::get_or_create(&state.db, &auth.0, &recipient).await?;
    debug!(
        chat = %row.id,
        user = %auth.0.id,
        recipient = %recipient.id,
        "chat opened"
    );
    chat_json(&state, &row).await
}

/// The refusal opening `user`'s chat with `recipient` owes the pair, `None`
/// when they may chat: naming yourself is the 422, a recipient whose
/// [`accepts_chat_messages`] flag is down — remote mirror or local account
/// alike — is the 403 that names the flag, and a pair blocked in either
/// direction is Pleroma's `Chat not allowed` 403, in that order.
///
/// [`accepts_chat_messages`]: crate::models::user::User::accepts_chat_messages
fn chat_refusal(user: &User, recipient: &User) -> Option<AppError> {
    if user.id == recipient.id {
        return Some(AppError::Unprocessable(CANT_CHAT_WITH_SELF.to_owned()));
    }
    if !recipient.accepts_chat_messages.get() {
        return Some(AppError::Forbidden(RECIPIENT_REJECTS_CHATS.to_owned()));
    }
    if blocked_between(user, recipient) {
        return Some(AppError::Forbidden(CHAT_NOT_ALLOWED.to_owned()));
    }
    None
}

/// Whether `user` and `recipient` have blocked each other, in either
/// direction — what makes Pleroma's `ChatController.create` answer the 403
/// [`CHAT_NOT_ALLOWED`].
///
/// No pair is blocked yet: the relationship table that records blocks (and
/// mutes, which hide a chat from the list but do not refuse opening it)
/// arrives with the blocks ticket, which wires this hook up the way
/// [`crate::models::follow`]'s like-named hook waits for the same table.
const fn blocked_between(_user: &User, _recipient: &User) -> bool {
    false
}

/// `GET /api/v1/pleroma/chats` — the caller's chats, newest activity first,
/// one page of at most [`LEGACY_MAX_LIMIT`] under the [`ChatListParams`]
/// filters, with the `Link` header keyed on the chat ids of the page.
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`Uri`] the nested router hands the handler: axum strips the prefixes,
/// and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:chats` check; [`AppError::BadRequest`] for a malformed
/// pagination cursor ([`PageParams::from_query`]);
/// [`AppError::Unprocessable`] for a filter that is not a boolean
/// ([`ApiParams`]). Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
///
/// [`Uri`]: axum::http::Uri
async fn list_chats(
    State(state): State<AppState>,
    auth: AuthUser,
    OriginalUri(uri): OriginalUri,
    ApiParams(params): ApiParams<ChatListParams>,
) -> AppResult<(HeaderMap, Json<Vec<Chat>>)> {
    auth.require(READ_CHATS)?;
    let page = PageParams::from_query(uri.query(), LEGACY_MAX_LIMIT)?;
    list_page(&state, auth.0.id, page, params.into_filter(), uri.path()).await
}

/// `GET /api/v2/pleroma/chats` — the paged counterpart of [`list_chats`]
/// (Pleroma's `index2`): the same query, the same filters, [`MAX_LIMIT`] as
/// the page cap instead of the legacy [`LEGACY_MAX_LIMIT`], and the cursors
/// walking chat ids while the page stays ordered by last message time.
///
/// # Errors
///
/// Exactly [`list_chats`]'s.
async fn list_chats_v2(
    State(state): State<AppState>,
    auth: AuthUser,
    OriginalUri(uri): OriginalUri,
    ApiParams(params): ApiParams<ChatListParams>,
) -> AppResult<(HeaderMap, Json<Vec<Chat>>)> {
    auth.require(READ_CHATS)?;
    let page = PageParams::from_query(uri.query(), MAX_LIMIT)?;
    list_page(&state, auth.0.id, page, params.into_filter(), uri.path()).await
}

/// The chats of `user_id` as one listed page: the [`Chat`] entities in
/// activity order under the `Link` header [`page_link`] derives from their
/// ids ([`page_for_user`] does the id keyset and the activity sort; opening
/// a chat never bumps it, a message always does).
///
/// [`page_for_user`]: crate::models::chat::page_for_user
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`];
/// rendering can only 404 through a recipient row that vanished with its
/// chat's counterpart (see [`chat_entity`]).
async fn list_page(
    state: &AppState,
    user_id: Uuid,
    page: PageParams,
    filter: ChatFilter,
    path: &str,
) -> AppResult<(HeaderMap, Json<Vec<Chat>>)> {
    let rows = chat::page_for_user(&state.db, user_id, page, filter).await?;
    let link = page_link(&state.config.base_url, path, &rows);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    debug!(
        count = rows.len(),
        user = %user_id,
        "chats listed"
    );
    Ok((headers, Json(chat_entities(state, &rows).await?)))
}

/// The `Link` header value for a fetched chat page: Pleroma's
/// `add_link_headers` over the first and last row's chat id — the page's
/// activity order stands, so the cursors name those two rows' ids — and
/// `None` for an empty page.
fn page_link(base_url: &str, path: &str, rows: &[chat::Chat]) -> Option<String> {
    let newest = rows.first()?.id.to_string();
    let oldest = rows.last()?.id.to_string();
    link_header(base_url, path, &NO_EXTRA_QUERY, &newest, &oldest)
}

/// The [`Chat`] entities for `rows`, each rendered the way [`chat_json`]
/// renders one.
///
/// # Errors
///
/// As [`chat_entity`]'s.
async fn chat_entities(state: &AppState, rows: &[chat::Chat]) -> AppResult<Vec<Chat>> {
    let mut entities = Vec::with_capacity(rows.len());
    for row in rows {
        entities.push(chat_entity(state, row).await?);
    }
    Ok(entities)
}

/// `GET /api/v1/pleroma/chats/{id}` — one of the caller's chats as the
/// [`Chat`] entity (Pleroma's `show`).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:chats` check; [`AppError::NotFound`] for an id that is
/// malformed, names no chat or names one of another account (see
/// [`owned_chat`]). Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn show_chat(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Chat>> {
    auth.require(READ_CHATS)?;
    let row = owned_chat(&state, auth.0.id, &id).await?;
    chat_json(&state, &row).await
}

/// `GET /api/v1/pleroma/chats/{id}/messages` — Pleroma's `messages`: the
/// caller's chat, one [`PageParams`] page of at most [`MAX_LIMIT`] messages,
/// newest first, with the `Link` header [`message_page_link`] keys on the
/// reference ids of the page.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:chats` check; [`AppError::NotFound`] for a malformed, unknown or
/// foreign `id` (see [`owned_chat`]); [`AppError::BadRequest`] for a malformed
/// pagination cursor ([`PageParams::from_query`]). Any database failure
/// arrives through `?` as [`AppError::Database`], and [`set_link_header`]
/// reports a `Link` value that is not header-safe as [`AppError::Internal`].
async fn list_messages(
    State(state): State<AppState>,
    auth: AuthUser,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
) -> AppResult<(HeaderMap, Json<Vec<ChatMessage>>)> {
    auth.require(READ_CHATS)?;
    let chat_row = owned_chat(&state, auth.0.id, &id).await?;
    let page = PageParams::from_query(uri.query(), MAX_LIMIT)?;
    let references = chat::messages_page(&state.db, chat_row.id, page).await?;
    let link = message_page_link(&state.config.base_url, uri.path(), &references);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    debug!(
        count = references.len(),
        chat = %chat_row.id,
        user = %auth.0.id,
        "chat messages listed"
    );
    let mut entities = Vec::with_capacity(references.len());
    for reference in &references {
        entities.push(message_of(&state, reference).await?);
    }
    Ok((headers, Json(entities)))
}

/// The `Link` header value for a fetched message page: [`page_link`] for
/// references instead of chats — the cursors are the first and last row's
/// reference id, which is what [`chat::messages_page`] keys its pages on.
fn message_page_link(base_url: &str, path: &str, rows: &[ChatMessageReference]) -> Option<String> {
    let newest = rows.first()?.id.to_string();
    let oldest = rows.last()?.id.to_string();
    link_header(base_url, path, &NO_EXTRA_QUERY, &newest, &oldest)
}

/// The body of `POST /api/v1/pleroma/chats/{id}/messages`: Pleroma's
/// `CreateMessageRequest` (its api spec), whose two keys are each optional —
/// the refusal for a body carrying neither is [`PostMessageParams::into_new_message`]'s.
#[derive(Debug, Deserialize)]
struct PostMessageParams {
    /// The message text. At most
    /// [`max_chat_message_chars`](crate::config::Config::max_chat_message_chars)
    /// characters, and the only content a message with no `media_id` may
    /// carry.
    #[serde(default)]
    content: Option<String>,
    /// The media to attach, as id text: the sender's own media row that no
    /// status has claimed and whose type is image, video or audio (see
    /// [`media_attachment::get_chat_attachment`]).
    #[serde(default)]
    media_id: Option<String>,
}

impl PostMessageParams {
    /// The parameters as the model's [`NewChatMessage`], refusing what the
    /// endpoint refuses before anything is looked up further: neither key
    /// present is the 400 [`EMPTY_CHAT_MESSAGE`], `content` over `max_chars`
    /// characters (counted as the model counts them) the 400
    /// [`MESSAGE_TOO_LONG`]. A text becomes [`NewChatMessage::text`]'s
    /// rendered pair of columns; a media-only message keeps both empty, the
    /// way the model stores it. `attachment_media_id` is the resolved media
    /// and `idempotency_key` the request's replay key, hung on the row.
    fn into_new_message(
        self,
        attachment_media_id: Option<Uuid>,
        max_chars: usize,
        idempotency_key: Option<String>,
    ) -> AppResult<NewChatMessage> {
        let Some(text) = self.content else {
            if self.media_id.is_none() {
                return Err(AppError::BadRequest(EMPTY_CHAT_MESSAGE.to_owned()));
            }
            return Ok(NewChatMessage {
                attachment_media_id,
                idempotency_key,
                ..NewChatMessage::default()
            });
        };
        if text.chars().count() > max_chars {
            return Err(AppError::BadRequest(MESSAGE_TOO_LONG.to_owned()));
        }
        Ok(NewChatMessage {
            attachment_media_id,
            idempotency_key,
            ..NewChatMessage::text(&text)
        })
    }
}

/// `POST /api/v1/pleroma/chats/{id}/messages` — Pleroma's
/// `post_chat_message`: append to the caller's chat and answer the
/// [`ChatMessage`] entity as the sender sees it (200, their reference id —
/// Pleroma's `Chat.add_message` renders the sender's `cm_ref` too, and the
/// recipient's unread reference is what the model writes beside it).
///
/// An `Idempotency-Key` header rides with the message and is checked before
/// anything is written: within the hour, the key's owner gets back the
/// message that key created, in whichever chat it was posted, and this call
/// writes nothing and fires no creation hook twice. Two requests racing under
/// one key can both write, as they can in Pleroma's cache — the key keeps a
/// client's own retry from doubling, not the database from permitting it.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:chats` check; [`AppError::NotFound`] for a malformed, unknown
/// or foreign `id`, or a recipient row that vanished with its chat;
/// [`AppError::BadRequest`] with [`MEDIA_NOT_FOUND`] for `media_id` naming no
/// attachable media (malformed id text included), with [`EMPTY_CHAT_MESSAGE`]
/// for a body with neither key, and with [`MESSAGE_TOO_LONG`] for `content`
/// above the [`max_chat_message_chars`] setting. The model answers anything
/// past its own [`MAX_CHAT_MESSAGE_CHARS`] backstop the way it always has.
/// Any database failure arrives through `?` as [`AppError::Database`].
///
/// [`MAX_CHAT_MESSAGE_CHARS`]: crate::models::chat::MAX_CHAT_MESSAGE_CHARS
async fn post_message(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiBody(params): ApiBody<PostMessageParams>,
) -> AppResult<Json<ChatMessage>> {
    auth.require(WRITE_CHATS)?;
    let chat_row = owned_chat(&state, auth.0.id, &id).await?;
    let recipient = chat_recipient(&state, &chat_row).await?;
    let attachment_media_id =
        resolve_chat_media(&state, params.media_id.as_deref(), auth.0.id).await?;
    let idempotency_key = idempotency_key(&headers);
    let new_message = params.into_new_message(
        attachment_media_id,
        state.config.max_chat_message_chars,
        idempotency_key.clone(),
    )?;
    if let Some(key) = idempotency_key.as_deref()
        && let Some(reference) = chat::sender_reference_for_key(&state.db, auth.0.id, key).await?
    {
        debug!(
            chat = %chat_row.id,
            user = %auth.0.id,
            "chat message reposted from its idempotency key"
        );
        return message_json(&state, &reference).await;
    }
    let reference = send_message(&state, &auth.0, &recipient, &new_message).await?;
    debug!(
        chat = %chat_row.id,
        message = %reference.message_id,
        user = %auth.0.id,
        "chat message posted"
    );
    let entity = message_of(&state, &reference).await?;
    after_chat_message_created(&state, &entity, &auth.0, &recipient);
    Ok(Json(entity))
}

/// The `media_id` parameter as a message attachment: absent is no
/// attachment; present must name media the sender may attach, and anything
/// else — unknown id, foreign row, claimed row, non-media type, malformed id
/// text — is the one 400 [`MEDIA_NOT_FOUND`].
///
/// # Errors
///
/// [`AppError::BadRequest`] as described; any database failure arrives
/// through `?` as [`AppError::Database`].
async fn resolve_chat_media(
    state: &AppState,
    text: Option<&str>,
    sender_id: Uuid,
) -> AppResult<Option<Uuid>> {
    let Some(text) = text else {
        return Ok(None);
    };
    let refused = || AppError::BadRequest(MEDIA_NOT_FOUND.to_owned());
    let id = parse_id(text).ok_or_else(refused)?;
    let row = media_attachment::get_chat_attachment(&state.db, id, sender_id)
        .await?
        .ok_or_else(refused)?;
    Ok(Some(row.id))
}

/// The `Idempotency-Key` header, read Pleroma's way (`idempotency_key/1`):
/// exactly one value is the key; an absent header, a repeated one, and bytes
/// no header text can hold are none.
fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    let mut values = headers.get_all(IDEMPOTENCY_KEY_HEADER).iter();
    let key = values.next()?.to_str().ok()?.to_owned();
    values.next().is_none().then_some(key)
}

/// [`chat::add_message`] on a transaction of its own: the message and both
/// sides' chat rows and references commit together. The federation ticket
/// that wraps this write and its delivery in one transaction replaces this
/// helper; until then posting is this one write.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`], and
/// the model's own length backstop arrives as its [`AppError::Unprocessable`].
async fn send_message(
    state: &AppState,
    sender: &User,
    recipient: &User,
    new_message: &NewChatMessage,
) -> AppResult<ChatMessageReference> {
    let mut tx = state.db.begin().await?;
    let (_, sender_reference, _) =
        chat::add_message(&mut tx, sender, recipient, new_message).await?;
    tx.commit().await?;
    Ok(sender_reference)
}

/// `DELETE /api/v1/pleroma/chats/{id}/messages/{message_id}` — Pleroma's
/// `delete_message`: the caller's own reference to the message, and the
/// message itself when the caller wrote it. The answer is the
/// [`ChatMessage`] entity as it was before it went.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:chats` check; [`AppError::NotFound`] for a malformed, unknown
/// or foreign `id` or `message_id` — a reference of another chat does not
/// live in this one, and a second delete finds nothing left to delete, all
/// one 404 (Pleroma's `with` clauses and its `:could_not_delete` fallback).
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn delete_message(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, message_id)): Path<(String, String)>,
) -> AppResult<Json<ChatMessage>> {
    auth.require(WRITE_CHATS)?;
    let chat_row = owned_chat(&state, auth.0.id, &id).await?;
    let Some(reference_id) = parse_id(&message_id) else {
        return Err(AppError::NotFound);
    };
    let Some(reference) = chat::message_reference(&state.db, chat_row.id, reference_id).await?
    else {
        return Err(AppError::NotFound);
    };
    let recipient = chat_recipient(&state, &chat_row).await?;
    // Rendered before the delete, from the reference the caller still has:
    // Pleroma renders the `cm_ref` it read, not the tombstone it leaves.
    let entity = message_of(&state, &reference).await?;
    if !chat::delete_message_for(&state.db, auth.0.id, reference_id).await? {
        return Err(AppError::NotFound);
    }
    debug!(
        chat = %chat_row.id,
        message = %reference_id,
        user = %auth.0.id,
        sender = %reference.sender_id,
        "chat message deleted"
    );
    after_chat_message_deleted(&state, &entity, &auth.0, &recipient);
    Ok(Json(entity))
}

/// The other participant of `row` as a [`User`]: the account a posted
/// message is addressed to and the party a deleted one is broadcast away
/// from. The foreign key makes the row's absence unreachable while the chat
/// stands; it answers the same 404 [`chat_entity`] gives its recipient.
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives through
/// `?` as [`AppError::Database`].
async fn chat_recipient(state: &AppState, row: &chat::Chat) -> AppResult<User> {
    user::get_by_id(&state.db, row.recipient_id)
        .await?
        .ok_or(AppError::NotFound)
}

/// The side effect Pleroma attaches to a created chat message, here
/// documented and empty.
///
/// Pleroma's `post_chat_message` runs the message through
/// `Pipeline.common_pipeline`, and the `Create` activity that comes out the
/// other end is what fans the message into the pair's chat sockets (the
/// `chat_message_created` event), into the recipient's notifications, and
/// out to the recipient's inbox. None of those transports exists here yet —
/// the streaming and federation tickets own them — so this hook takes the
/// created [`ChatMessage`] entity as the recipient's side sees it (the
/// posting endpoint passes the sender's own view; the federation chat
/// handler, [`crate::federation::handlers::chat`], passes the recipient's),
/// the `sender`, and the local-or-remote `recipient` the message is
/// addressed to, and does nothing until those tickets fill in exactly this
/// one function.
pub(crate) const fn after_chat_message_created(
    _state: &AppState,
    _message: &ChatMessage,
    _sender: &User,
    _recipient: &User,
) {
}

/// The side effect Pleroma attaches to a deleted chat message, here
/// documented and empty — the delete counterpart of
/// [`after_chat_message_created`]: Pleroma's `delete_message` ends with the
/// `chat_message_deleted` socket push for the viewer's chat and, when the
/// sender withdrew the message, an `Undo` of its `Create` for federation;
/// `user` is the caller who deleted, `other` the chat's recipient, and
/// `message` the entity as the caller saw it before the delete.
const fn after_chat_message_deleted(
    _state: &AppState,
    _message: &ChatMessage,
    _user: &User,
    _other: &User,
) {
}

/// The `with_muted` and `pinned` query parameters of both chat lists
/// (Pleroma's `index_query`).
#[derive(Debug, Deserialize)]
struct ChatListParams {
    /// Whether chats with muted accounts are listed (Pleroma's
    /// `with_muted`); absent means `false`. The exclusion itself waits for
    /// the mutes table the way [`blocked_between`] waits for the blocks one
    /// — see [`ChatFilter::with_muted`].
    #[serde(default, deserialize_with = "flex::opt_bool")]
    with_muted: Option<bool>,
    /// `Some(true)` lists only pinned chats, `Some(false)` only unpinned
    /// ones, `None` lists both (Pleroma's `restrict_pinned`).
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pinned: Option<bool>,
}

impl ChatListParams {
    /// These parameters as the model's [`ChatFilter`]: the flag defaults to
    /// `false` and the filter stays `None` when its key is absent.
    fn into_filter(self) -> ChatFilter {
        ChatFilter {
            pinned: self.pinned,
            with_muted: self.with_muted.unwrap_or_default(),
        }
    }
}

/// The body of `POST …/chats/{id}/read`: Pleroma's `ChatReadParameter`.
#[derive(Debug, Deserialize)]
struct ReadParams {
    /// The newest reference id the client has seen, as id text; required —
    /// its absence is [`MISSING_LAST_READ_ID`] — and compared against the
    /// reference ids of the caller's chat (decision 4: their text sorts in
    /// creation order, so `<=` is "up to and including").
    #[serde(default)]
    last_read_id: Option<String>,
}

impl ReadParams {
    /// The marker text, refusing the request the way Pleroma's fallback
    /// clause does when the body carries no `last_read_id` at all.
    ///
    /// # Errors
    ///
    /// [`AppError::BadRequest`] with [`MISSING_LAST_READ_ID`].
    fn marker_text(&self) -> AppResult<&str> {
        self.last_read_id
            .as_deref()
            .ok_or_else(|| AppError::BadRequest(MISSING_LAST_READ_ID.to_owned()))
    }
}

/// The marker text as an id: what a whole-chat read compares reference ids
/// against (decision 4).
///
/// # Errors
///
/// [`AppError::BadRequest`] with [`INVALID_LAST_READ_ID`] when the text is
/// not id text.
fn marker_id(text: &str) -> AppResult<Uuid> {
    parse_id(text).ok_or_else(|| AppError::BadRequest(INVALID_LAST_READ_ID.to_owned()))
}

/// `POST /api/v1/pleroma/chats/{id}/read` — mark the caller's chat `id` read
/// up to `last_read_id` and answer the chat with its new `unread` count.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:chats` check; [`AppError::BadRequest`] when the body has no
/// `last_read_id` ([`ReadParams::marker_text`]) or one that is not an id;
/// [`AppError::NotFound`] when `id` is malformed, names no chat or names one
/// of another account. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn mark_chat_read(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<ReadParams>,
) -> AppResult<Json<Chat>> {
    auth.require(WRITE_CHATS)?;
    // The parameter first, the chat second: Pleroma matches the body clause
    // before `Chat.get_by_user_and_id` ever runs.
    let marker_text = params.marker_text()?;
    let chat_row = owned_chat(&state, auth.0.id, &id).await?;
    let last_read_id = marker_id(marker_text)?;
    let marked = chat::mark_read(&state.db, chat_row.id, Some(last_read_id)).await?;
    debug!(
        chat = %chat_row.id,
        user = %auth.0.id,
        marked,
        "chat marked read"
    );
    after_chat_read(&auth.0, chat_row.id, Some(last_read_id));
    chat_json(&state, &chat_row).await
}

/// `POST /api/v1/pleroma/chats/{id}/messages/{message_id}/read` — mark one
/// reference of the caller's chat read and answer it as a message entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:chats` check; [`AppError::NotFound`] when either id is
/// malformed, when `id` names no chat of the caller's, or when `message_id`
/// names no reference *of that chat* (another chat's reference is foreign
/// here, exactly as Pleroma's `with` clause answers it). Any database failure
/// arrives through `?` as [`AppError::Database`].
async fn mark_message_read(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, message_id)): Path<(String, String)>,
) -> AppResult<Json<ChatMessage>> {
    auth.require(WRITE_CHATS)?;
    let chat_row = owned_chat(&state, auth.0.id, &id).await?;
    let Some(reference_id) = parse_id(&message_id) else {
        return Err(AppError::NotFound);
    };
    let Some(reference) = chat::mark_message_read(&state.db, chat_row.id, reference_id).await?
    else {
        return Err(AppError::NotFound);
    };
    debug!(
        chat = %chat_row.id,
        message = %reference_id,
        user = %auth.0.id,
        "chat message marked read"
    );
    after_chat_read(&auth.0, chat_row.id, Some(reference_id));
    message_json(&state, &reference).await
}

/// `POST /api/v1/pleroma/chats/{id}/pin` — pin the caller's chat to the top
/// of their list and answer the chat.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:chats` check; [`AppError::NotFound`] for a malformed, unknown
/// or foreign `id`. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn pin_chat(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Chat>> {
    set_pinned(&state, &auth, &id, true).await
}

/// `POST /api/v1/pleroma/chats/{id}/unpin` — clear the pin [`pin_chat`] set
/// and answer the chat. Errors are its errors.
async fn unpin_chat(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Chat>> {
    set_pinned(&state, &auth, &id, false).await
}

/// The body of `pin` and `unpin`: one flag of the caller's own chat set, then
/// the whole chat rendered (Pleroma's `Chat.pin`/`Chat.unpin` + `show.json`).
///
/// # Errors
///
/// As [`pin_chat`]: the scope check, the 404 for a chat the id does not give
/// the caller, and database failures.
async fn set_pinned(
    state: &AppState,
    auth: &AuthUser,
    id: &str,
    pinned: bool,
) -> AppResult<Json<Chat>> {
    auth.require(WRITE_CHATS)?;
    let Some(chat_id) = parse_id(id) else {
        return Err(AppError::NotFound);
    };
    let Some(chat_row) = chat::set_pinned(&state.db, auth.0.id, chat_id, pinned).await? else {
        return Err(AppError::NotFound);
    };
    debug!(
        chat = %chat_row.id,
        user = %auth.0.id,
        pinned,
        "chat pin flag set"
    );
    chat_json(state, &chat_row).await
}

/// The `{id}` segment of a chat route as the caller's own [`chat::Chat`] row:
/// a malformed id, no such chat and a chat of another account are one 404,
/// because ids are never shared between participants (see the model's
/// [`chat::get_owned`], Pleroma's `Chat.get_by_user_and_id` with the fallback
/// controller behind it).
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives through
/// `?` as [`AppError::Database`].
async fn owned_chat(state: &AppState, user_id: Uuid, id: &str) -> AppResult<chat::Chat> {
    let Some(chat_id) = parse_id(id) else {
        return Err(AppError::NotFound);
    };
    chat::get_owned(&state.db, user_id, chat_id)
        .await?
        .ok_or(AppError::NotFound)
}

/// The `Chat` entity for `row`: its recipient [`User`] and [`chat::Account`]
/// side by side, the unread count as it is now, and the chat's newest live
/// message (see the entity's module docs for what each part replaces in
/// Pleroma's `ChatView.render("show.json")`).
///
/// [`crate::api::streaming`] renders a hinted chat through this same
/// builder, so a socket's `pleroma:chat_update` entity and the endpoint's
/// are built the same way.
///
/// # Errors
///
/// [`AppError::NotFound`] if the recipient row is gone (the foreign key makes
/// that unreachable while the chat exists); database failures arrive through
/// `?` as [`AppError::Database`].
pub(crate) async fn chat_entity(state: &AppState, row: &chat::Chat) -> AppResult<Chat> {
    let recipient = chat_recipient(state, row).await?;
    let unread = chat::unread_count(&state.db, row.id).await?;
    let last_message = newest_message(state, row.id).await?;
    Ok(Chat::from_row(
        row,
        &recipient,
        &state.config,
        unread,
        last_message,
    ))
}

/// The [`Chat`] entity for `row`, answered as JSON.
///
/// # Errors
///
/// As [`chat_entity`]'s.
async fn chat_json(state: &AppState, row: &chat::Chat) -> AppResult<Json<Chat>> {
    Ok(Json(chat_entity(state, row).await?))
}

/// The chat's newest live message as an entity, `None` when it has none.
///
/// # Errors
///
/// As [`chat_json`]'s.
async fn newest_message(state: &AppState, chat_id: Uuid) -> AppResult<Option<ChatMessage>> {
    let references = chat::messages_page(&state.db, chat_id, NEWEST_PAGE).await?;
    let Some(reference) = references.first() else {
        return Ok(None);
    };
    message_of(state, reference).await.map(Some)
}

/// The [`ChatMessage`] entity for `reference`, with its attachment row read
/// when it carries one.
///
/// # Errors
///
/// As [`chat_json`]'s.
async fn message_of(state: &AppState, reference: &ChatMessageReference) -> AppResult<ChatMessage> {
    let attachment = match reference.attachment_media_id {
        Some(media_id) => media_attachment::get_by_id(&state.db, media_id).await?,
        None => None,
    };
    Ok(ChatMessage::from_reference(
        reference,
        attachment.as_ref(),
        &state.config,
    ))
}

/// The [`ChatMessage`] entity for a freshly marked reference.
///
/// # Errors
///
/// As [`chat_json`]'s.
async fn message_json(
    state: &AppState,
    reference: &ChatMessageReference,
) -> AppResult<Json<ChatMessage>> {
    Ok(Json(message_of(state, reference).await?))
}

/// The side effect Pleroma attaches to a successful read, here documented and
/// empty.
///
/// Pleroma's `mark_as_read` and `mark_message_as_read` (2.0-era
/// `chat_controller.ex`) end with
/// `Pleroma.Web.ChatSocket.ChatSocket.broadcast_chat(user, :chat_read, id)`
/// after storing the marker: a `chat_read` event pushed to every chat socket
/// the reading user has connected, so their other clients clear the badge too.
/// This instance serves no chat sockets yet — the streaming ticket owns that
/// transport — so there is nothing to broadcast: both read handlers call this
/// hook after the marker is stored (`chat_id` always, and `last_read_id` as
/// the chat-wide marker or the single reference marked), and wiring the
/// sockets up later means filling in exactly this one function.
const fn after_chat_read(_user: &User, _chat_id: Uuid, _last_read_id: Option<Uuid>) {}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use serde_json::json;

    use super::*;

    /// Parses `value` as the whole-chat read's body.
    fn read_params(value: serde_json::Value) -> ReadParams {
        crate::extract::into_params(value).expect("the fixture is a parameters object")
    }

    #[test]
    fn a_body_without_the_marker_is_refused_with_pleromas_message() {
        for value in [json!({}), json!({"other": "x"})] {
            let err = read_params(value)
                .marker_text()
                .expect_err("no last_read_id, so no marker");
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "the refusal is a 400: {err}"
            );
            assert_eq!(err.to_string(), MISSING_LAST_READ_ID, "its wording");
        }
    }

    #[test]
    fn a_body_with_the_marker_hands_over_its_text() {
        // The text is kept verbatim; turning it into an id is the handler's
        // `parse_id` step, so even nonsense survives this gate — only the
        // *absence* of the key is the 400 Pleroma's fallback clause answers.
        assert_eq!(
            read_params(json!({"last_read_id": "0193"}))
                .marker_text()
                .unwrap(),
            "0193",
            "the value is passed through unparsed"
        );
        assert_eq!(
            read_params(json!({"last_read_id": ""}))
                .marker_text()
                .expect("an empty marker is still a present marker"),
            "",
            "and it is the id parse, not this gate, that refuses it"
        );
    }

    #[test]
    fn only_id_text_passes_the_marker_parse() {
        let good = "01890b9a-2c1e-7a5b-8c9d-1e2f3a4b5c6d";
        assert_eq!(
            marker_id(good).expect("uuid text is a marker").to_string(),
            good,
            "hyphenated id text round-trips"
        );
        for junk in ["not-a-uuid", "", "01890b9a"] {
            let err = marker_id(junk).expect_err("junk is not an id");
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "the refusal is a 400: {err}"
            );
            assert_eq!(err.to_string(), INVALID_LAST_READ_ID, "its wording");
        }
    }

    /// Inserts a local [`User`] named `nickname` that accepts chats.
    async fn chatter(pool: &sqlx::PgPool, nickname: &str) -> User {
        user::insert(
            pool,
            &user::NewUser::local(
                nickname,
                &user::local_ap_id(crate::config::TEST_BASE_URL, nickname),
            ),
        )
        .await
        .expect("fixture user inserts")
    }

    /// Inserts a local [`User`] named `nickname` whose chat flag is down.
    async fn quiet(pool: &sqlx::PgPool, nickname: &str) -> User {
        user::insert(
            pool,
            &user::NewUser {
                accepts_chat_messages: user::Flag::new(false),
                ..user::NewUser::local(
                    nickname,
                    &user::local_ap_id(crate::config::TEST_BASE_URL, nickname),
                )
            },
        )
        .await
        .expect("fixture user inserts")
    }

    /// Inserts a remote mirror named `nickname` whose chat flag is down: the
    /// shape a fetched `Person` that said `acceptsChatMessages: false` has.
    async fn quiet_remote(pool: &sqlx::PgPool, nickname: &str) -> User {
        user::insert(
            pool,
            &user::NewUser {
                nickname: format!("{nickname}@remote.example"),
                ap_id: format!("https://remote.example/users/{nickname}"),
                domain: Some("remote.example".to_owned()),
                accepts_chat_messages: user::Flag::new(false),
                ..user::NewUser::default()
            },
        )
        .await
        .expect("fixture remote user inserts")
    }

    #[tokio::test]
    async fn chatting_with_yourself_is_the_422() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = chatter(&pool, "alice").await;

        let err = chat_refusal(&alice, &alice).expect("the self-chat is refused");

        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "naming yourself is a 422: {err}"
        );
        assert_eq!(
            err.to_string(),
            CANT_CHAT_WITH_SELF,
            "the ticket's spelling"
        );
    }

    #[tokio::test]
    async fn a_recipient_that_does_not_accept_chats_is_named_in_a_403() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = chatter(&pool, "alice").await;

        for silent in [
            quiet(&pool, "carol").await,
            quiet_remote(&pool, "mallory").await,
        ] {
            let err = chat_refusal(&alice, &silent)
                .unwrap_or_else(|| panic!("{} refuses the pair", silent.nickname));
            assert!(
                matches!(err, AppError::Forbidden(_)),
                "the flag is refused with a 403: {err}"
            );
            assert_eq!(
                err.to_string(),
                RECIPIENT_REJECTS_CHATS,
                "its own wording, remote mirror or local account alike"
            );
        }
    }

    #[tokio::test]
    async fn an_ordinary_pair_passes_and_no_pair_is_blocked_yet() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = chatter(&pool, "alice").await;
        let bob = chatter(&pool, "bob").await;
        let mallory = quiet_remote(&pool, "mallory").await;

        assert!(
            chat_refusal(&alice, &bob).is_none(),
            "two chatters may chat"
        );
        assert!(
            !blocked_between(&alice, &bob) && !blocked_between(&bob, &alice),
            "no pair is blocked until the blocks ticket wires the hook up"
        );
        // The flag gate stands ahead of the block hook: a silent recipient
        // is refused by name whatever the block tables will one day say.
        assert!(
            chat_refusal(&alice, &mallory).is_some(),
            "flag down is a refusal"
        );
    }

    /// Parses `value` as the lists' query parameters and filters as they ask.
    fn filter_params(value: serde_json::Value) -> ChatFilter {
        crate::extract::into_params::<ChatListParams>(value)
            .expect("the fixture is a parameters object")
            .into_filter()
    }

    #[test]
    fn absent_filters_give_the_plain_list() {
        assert_eq!(
            filter_params(json!({})),
            ChatFilter::default(),
            "no keys, no restrictions"
        );
        assert_eq!(
            filter_params(json!({"pinned": null, "with_muted": null})),
            ChatFilter::default(),
            "an explicit null is still absent"
        );
    }

    #[test]
    fn the_pinned_filter_keeps_its_three_states() {
        assert_eq!(
            filter_params(json!({"pinned": true})).pinned,
            Some(true),
            "the pinned half"
        );
        assert_eq!(
            filter_params(json!({"pinned": "false"})).pinned,
            Some(false),
            "the unpinned half, in a form client's string spelling"
        );
        assert_eq!(
            filter_params(json!({"with_muted": "1"})).pinned,
            None,
            "and no filter at all when the key is absent"
        );
    }

    #[test]
    fn with_muted_is_read_in_its_client_spellings() {
        assert!(
            filter_params(json!({"with_muted": "true"})).with_muted,
            "`\"true\"` includes the muted"
        );
        assert!(
            !filter_params(json!({"with_muted": false})).with_muted,
            "and a plain false excludes them (once the mutes ticket gives them an exclusion)"
        );
    }

    #[test]
    fn a_filter_that_is_not_a_boolean_is_a_parameter_error() {
        for key in ["with_muted", "pinned"] {
            let err = crate::extract::into_params::<ChatListParams>(json!({ key: "maybe" }))
                .expect_err("`maybe` is not a boolean");
            assert!(
                err.to_string().contains("expected a boolean"),
                "and the flex message says so: {err}"
            );
        }
    }

    #[test]
    fn the_legacy_page_caps_lower_than_the_v2_page() {
        let legacy = PageParams::from_query(Some("limit=35"), LEGACY_MAX_LIMIT)
            .expect("a limit above the cap falls to it");
        assert_eq!(legacy.limit, 30, "the legacy list takes at most 30");
        assert_eq!(
            PageParams::from_query(None, LEGACY_MAX_LIMIT)
                .expect("no query is the default page")
                .limit,
            20,
            "and defaults to 20"
        );
        let v2 = PageParams::from_query(Some("limit=35"), MAX_LIMIT)
            .expect("the v2 page lets 35 through");
        assert_eq!(v2.limit, 35, "only MAX_LIMIT caps it");
    }

    /// Parses `value` as the message post's body.
    fn message_params(value: serde_json::Value) -> PostMessageParams {
        crate::extract::into_params(value).expect("the fixture is a parameters object")
    }

    #[test]
    fn the_message_body_takes_content_and_media_id_as_optional_strings() {
        let id = "01890b9a-2c1e-7a5b-8c9d-1e2f3a4b5c6d";
        let both = message_params(json!({"content": "hi", "media_id": id}));
        assert_eq!(both.content.as_deref(), Some("hi"), "the text");
        assert_eq!(both.media_id.as_deref(), Some(id), "and the media id text");
        let neither = message_params(json!({}));
        assert!(neither.content.is_none(), "content is optional");
        assert!(neither.media_id.is_none(), "and so is media_id");
        let nulled = message_params(json!({"content": null, "media_id": null}));
        assert!(
            nulled.content.is_none() && nulled.media_id.is_none(),
            "an explicit null is still absent"
        );
    }

    #[test]
    fn a_message_body_of_the_wrong_shape_is_a_parameter_error() {
        for value in [json!({"content": 7}), json!({"media_id": ["x"]})] {
            let err = crate::extract::into_params::<PostMessageParams>(value)
                .expect_err("a number is not a string body");
            assert!(
                matches!(err, AppError::Unprocessable(_)),
                "the refusal is a 422: {err}"
            );
        }
    }

    #[test]
    fn neither_content_nor_media_is_the_empty_message_400() {
        let err = message_params(json!({}))
            .into_new_message(None, 5_000, None)
            .expect_err("nothing to send");
        assert!(matches!(err, AppError::BadRequest(_)), "a 400: {err}");
        assert_eq!(err.to_string(), EMPTY_CHAT_MESSAGE, "the ticket's wording");
    }

    #[test]
    fn content_becomes_the_rendered_message_and_media_only_keeps_both_empty() {
        let media = Uuid::nil();
        let text = message_params(json!({"content": "hi *there*"}))
            .into_new_message(Some(media), 5_000, Some("k1".to_owned()))
            .expect("plain text is a message");
        assert_eq!(text.raw_text, "hi *there*", "the source is kept");
        assert_eq!(text.content, "<p>hi *there*</p>", "the HTML is rendered");
        assert_eq!(
            text.attachment_media_id,
            Some(media),
            "the attachment rides"
        );
        assert_eq!(text.idempotency_key.as_deref(), Some("k1"), "and the key");

        let only_media =
            message_params(json!({"media_id": "01890b9a-2c1e-7a5b-8c9d-1e2f3a4b5c6d"}))
                .into_new_message(Some(media), 5_000, None)
                .expect("media alone is a message");
        assert_eq!(only_media.content, "", "content stays empty");
        assert_eq!(only_media.raw_text, "", "both columns");
        assert!(
            only_media.idempotency_key.is_none(),
            "and no key means none"
        );

        // Pleroma's validator refuses nil content, not empty content: a
        // present empty string is a sent message, and rendering it is empty.
        let empty_text = message_params(json!({"content": ""}))
            .into_new_message(None, 5_000, None)
            .expect("an empty string is still a present content");
        assert_eq!(empty_text.content, "", "rendering empty text is empty");
        assert_eq!(empty_text.raw_text, "", "kept verbatim");
    }

    #[test]
    fn content_over_the_setting_is_the_too_long_400() {
        // The setting is in characters, counted the way the model counts
        // them: three euro signs fit a limit of three, a fourth does not.
        let err = message_params(json!({"content": "€€€€"}))
            .into_new_message(None, 3, None)
            .expect_err("4 characters over a limit of 3");
        assert!(matches!(err, AppError::BadRequest(_)), "a 400: {err}");
        assert_eq!(err.to_string(), MESSAGE_TOO_LONG, "the ticket's wording");
        assert!(
            message_params(json!({"content": "€€€"}))
                .into_new_message(None, 3, None)
                .is_ok(),
            "exactly the limit goes through"
        );
        // An attachment does not excuse length here the way Pleroma's
        // `validate_chat_content_length(_, true)` lets it: the model's
        // backstop measures the text whatever rides beside it.
        assert!(
            message_params(json!({"content": "abcd", "media_id": "x"}))
                .into_new_message(None, 3, None)
                .is_err(),
            "a media message's content is bounded too"
        );
    }

    /// A header map carrying `values` under the idempotency header.
    fn keyed(values: &[HeaderValue]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            let _added: bool = headers.append(IDEMPOTENCY_KEY_HEADER, value.clone());
        }
        headers
    }

    #[test]
    fn exactly_one_idempotency_key_header_is_the_key() {
        assert!(idempotency_key(&keyed(&[])).is_none(), "absent is none");
        assert_eq!(
            idempotency_key(&keyed(&[HeaderValue::from_static("k-1")])).as_deref(),
            Some("k-1"),
            "one value is the key"
        );
        assert_eq!(
            idempotency_key(&keyed(&[HeaderValue::from_static("")])).as_deref(),
            Some(""),
            "even the empty string is a key a client sent"
        );
        assert!(
            idempotency_key(&keyed(&[
                HeaderValue::from_static("a"),
                HeaderValue::from_static("b"),
            ]))
            .is_none(),
            "two values are no key (Pleroma's `[key] -> key` clause)"
        );
        let binary = HeaderValue::from_bytes(&[b'k', 0xff]).expect("a header value of raw bytes");
        assert!(
            idempotency_key(&keyed(&[binary])).is_none(),
            "bytes no string can hold are no key"
        );
    }

    /// A reference row whose only interesting part is its id `n`, for the
    /// `Link` test.
    fn link_reference(n: u128) -> ChatMessageReference {
        ChatMessageReference {
            id: Uuid::from_u128(n),
            chat_id: Uuid::nil(),
            message_id: Uuid::nil(),
            unread: false,
            inserted_at: std::time::SystemTime::now().into(),
            sender_id: Uuid::nil(),
            content: String::new(),
            raw_text: String::new(),
            attachment_media_id: None,
            ap_id: None,
            idempotency_key: None,
            message_inserted_at: std::time::SystemTime::now().into(),
        }
    }

    #[test]
    fn the_message_link_keys_on_the_pages_reference_ids() {
        let link = message_page_link(
            "https://h",
            "/p",
            &[link_reference(3), link_reference(2), link_reference(1)],
        )
        .expect("a page links");
        assert!(
            link.contains("max_id=00000000-0000-0000-0000-000000000001"),
            "next walks to the oldest reference id: {link}"
        );
        assert!(
            link.contains("min_id=00000000-0000-0000-0000-000000000003"),
            "prev walks to the newest: {link}"
        );
        assert!(
            message_page_link("https://h", "/p", &[]).is_none(),
            "an empty page links nowhere"
        );
    }
}
