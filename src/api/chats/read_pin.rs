//! The read markers and the pin flag: Pleroma's `mark_as_read`,
//! `mark_message_as_read`, `pin` and `unpin` — the four write routes that
//! update a chat's unread state or its owner's list order and answer the
//! fresh [`Chat`] or [`ChatMessage`] entity.

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    entities::chat::{Chat, ChatMessage},
    error::{AppError, AppResult},
    extract::ApiBody,
    models::chat,
    oauth::scopes::WRITE_CHATS,
    state::AppState,
    util::id::parse_id,
};

use super::{
    INVALID_LAST_READ_ID, MISSING_LAST_READ_ID,
    chats_list::{chat_json, owned_chat},
    events::after_chat_read,
    messages::message_json,
};

/// The body of `POST …/chats/{id}/read`: Pleroma's `ChatReadParameter`.
#[derive(Debug, Deserialize)]
pub(super) struct ReadParams {
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
pub(super) async fn mark_chat_read(
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
    after_chat_read(&state, &auth.0, chat_row.id, Some(last_read_id));
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
pub(super) async fn mark_message_read(
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
    after_chat_read(&state, &auth.0, chat_row.id, Some(reference_id));
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
pub(super) async fn pin_chat(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Chat>> {
    set_pinned(&state, &auth, &id, true).await
}

/// `POST /api/v1/pleroma/chats/{id}/unpin` — clear the pin [`pin_chat`] set
/// and answer the chat. Errors are its errors.
pub(super) async fn unpin_chat(
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
