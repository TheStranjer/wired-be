//! Chat messages, read and removed, and the shared message builders.
//!
//! `GET /api/v1/pleroma/chats/{id}/messages` (Pleroma's `messages`) and
//! `DELETE …/messages/{message_id}` (its `delete_message`), with
//! `keyset_link` for the page's `Link` header, `chat_recipient` for the
//! counterpart, and `message_of` and `message_json` for the entity. The posting route lives in the sibling `post_message` child.

use axum::{
    Json,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
};
use tracing::debug;

use crate::{
    auth::AuthUser,
    entities::chat::ChatMessage,
    error::{AppError, AppResult},
    models::{
        chat,
        chat::ChatMessageReference,
        media_attachment,
        user::{self, User},
    },
    oauth::scopes::{READ_CHATS, WRITE_CHATS},
    pagination::{MAX_LIMIT, NO_EXTRA_QUERY, PageParams, keyset_link, set_link_header},
    state::AppState,
    util::id::parse_id,
};

use super::{chats_list::owned_chat, events::after_chat_message_deleted};

/// `GET /api/v1/pleroma/chats/{id}/messages` — Pleroma's `messages`: the
/// caller's chat, one [`PageParams`] page of at most [`MAX_LIMIT`] messages,
/// newest first, with the `Link` header [`keyset_link`] keys on the
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
pub(super) async fn list_messages(
    State(state): State<AppState>,
    auth: AuthUser,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
) -> AppResult<(HeaderMap, Json<Vec<ChatMessage>>)> {
    auth.require(READ_CHATS)?;
    let chat_row = owned_chat(&state, auth.0.id, &id).await?;
    let page = PageParams::from_query(uri.query(), MAX_LIMIT)?;
    let references = chat::messages_page(&state.db, chat_row.id, page).await?;
    // The cursors are the first and last row's reference id, which is what
    // [`chat::messages_page`] keys its pages on.
    let link = keyset_link(
        &state.config.base_url,
        uri.path(),
        &NO_EXTRA_QUERY,
        &references,
        |reference| reference.id,
    );
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
pub(super) async fn delete_message(
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
    after_chat_message_deleted(&state, &reference, &auth.0, &recipient).await;
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
///
/// [`chat_entity`]: crate::api::chats::chats_list::chat_entity
pub(super) async fn chat_recipient(state: &AppState, row: &chat::Chat) -> AppResult<User> {
    user::get_by_id(&state.db, row.recipient_id)
        .await?
        .ok_or(AppError::NotFound)
}

/// The [`ChatMessage`] entity for `reference`, with its attachment row read
/// when it carries one.
///
/// `crate::api::admin::chats` renders a foreign chat's messages through this
/// same builder, so a moderator sees exactly the entity the owner's client
/// does.
///
/// # Errors
///
/// As [`chat_json`]'s.
pub(crate) async fn message_of(
    state: &AppState,
    reference: &ChatMessageReference,
) -> AppResult<ChatMessage> {
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
///
/// [`chat_json`]: crate::api::chats::chats_list::chat_json
pub(super) async fn message_json(
    state: &AppState,
    reference: &ChatMessageReference,
) -> AppResult<Json<ChatMessage>> {
    Ok(Json(message_of(state, reference).await?))
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

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
        let link = keyset_link(
            "https://h",
            "/p",
            &NO_EXTRA_QUERY,
            &[link_reference(3), link_reference(2), link_reference(1)],
            |reference| reference.id,
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
        let none: [ChatMessageReference; 0] = [];
        assert!(
            keyset_link("https://h", "/p", &NO_EXTRA_QUERY, &none, |reference| {
                reference.id
            })
            .is_none(),
            "an empty page links nowhere"
        );
    }
}
