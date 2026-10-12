//! Chat read markers and pinning over HTTP (`/api/v1/pleroma/chats`,
//! WBE-148): the four write-side actions of Pleroma's chat API.
//!
//! The reference is `Pleroma.Web.PleromaAPI.ChatController`
//! (`lib/pleroma/web/pleroma_api/controllers/chat_controller.ex`), whose
//! `mark_as_read`, `mark_message_as_read`, `pin` and `unpin` actions these
//! routes replicate; the state they change lives in the three chat tables
//! behind [`crate::models::chat`], and the JSON they answer is the
//! [`Chat`] and [`ChatMessage`] entities (decision 3).
//!
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
//! `write:chats` (its `OAuthScopesPlug` pipeline for these four actions), then
//! the body of the whole-chat read — without `last_read_id` a request is the
//! 400 `{"error":"Missing parameter: last_read_id"}` of that controller's
//! fallback clause, checked before anything is looked up; a marker that is
//! present but not an id is 400 `"Invalid parameter: last_read_id"`,
//! Pleroma's own uuid cast having no sane answer here. `{id}` (and `{message_id}`)
//! resolve through [`crate::models::chat::get_owned`], so a malformed,
//! unknown and foreign id are one and the same 404 "Record not found": each
//! participant's chat has its own id and never shares it, and one reference
//! is only markable through the chat that holds it — Pleroma's
//! `mark_message_as_read` `with` clause answers the same way.
//!
//! Both read handlers finish with `after_chat_read`, the documented empty
//! hook where Pleroma pushes its `chat_read` socket event; the pin routes
//! broadcast nothing there and so call nothing here.

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};
use serde::Deserialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    entities::chat::{Chat, ChatMessage},
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        chat,
        chat::ChatMessageReference,
        media_attachment,
        user::{self, User},
    },
    oauth::scopes::WRITE_CHATS,
    pagination::PageParams,
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

/// The `/api/v1/pleroma/chats` router, nested into [`crate::api::v1`].
///
/// Do not call [`Router::fallback`] here (or on anything merged into this
/// tree): the app-wide JSON fallback in [`crate::app`] must answer the
/// unmatched paths.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/{id}/read", post(mark_chat_read))
        .route("/{id}/messages/{message_id}/read", post(mark_message_read))
        .route("/{id}/pin", post(pin_chat))
        .route("/{id}/unpin", post(unpin_chat))
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
/// # Errors
///
/// [`AppError::NotFound`] if the recipient row is gone (the foreign key makes
/// that unreachable while the chat exists); database failures arrive through
/// `?` as [`AppError::Database`].
async fn chat_json(state: &AppState, row: &chat::Chat) -> AppResult<Json<Chat>> {
    let recipient = user::get_by_id(&state.db, row.recipient_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let unread = chat::unread_count(&state.db, row.id).await?;
    let last_message = newest_message(state, row.id).await?;
    Ok(Json(Chat::from_row(
        row,
        &recipient,
        &state.config,
        unread,
        last_message,
    )))
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
    Ok(ChatMessage::from_reference(reference, attachment.as_ref()))
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
}
