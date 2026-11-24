//! The admin chat endpoints: the chat ledger staff read and the one
//! withdrawal they can perform, under `/api/v1/pleroma/admin` (WBE-202).
//!
//! The reference is Pleroma's `AdminAPI.ChatController`
//! (`lib/pleroma/web/admin_api/controllers/chat_controller.ex`) beside the
//! `list_user_chats` action of `AdminAPIController`
//! (`lib/pleroma/web/admin_api/controllers/admin_api_controller.ex`), routed
//! in `lib/pleroma/web/router.ex` and gated by `admin:read:chats` on the
//! reads and `admin:write:chats` on the delete — the same gates as the
//! `require` call each handler makes first, on the area `chats`. The role
//! half follows Pleroma's privileges: the three reads sit behind
//! `:require_privileged_role_messages_read`, which only admins hold, so they
//! take the [`AdminUser`] extractor and a moderator is refused; the delete
//! shares the `:require_privileged_role_messages_delete` scope with the
//! status writes, a privilege moderators hold, so it takes the [`StaffUser`]
//! extractor and a moderator can remove abusive messages reported to them.
//!
//! The entities are the ordinary [`Chat`] and [`ChatMessage`] of the client
//! API (decision 3), rendered **without ownership checks**: a chat belongs
//! to the account it is stored under and its id is never shared, so the
//! user-side routes run every read through [`chat::get_owned`]; here the id
//! alone resolves, through [`chat::get_by_id`], and the page and message
//! lists are the very reads the owner's client hits
//! ([`chat::page_for_user`], [`chat::messages_page`]).
//!
//! - `GET /users/{nickname}/chats` (`list_user_chats`) — the account the
//!   segment names, by nickname (a remote `nick@domain` handle included) or
//!   id through [`admin::get_by_nickname_or_id`], its chats newest activity
//!   first, one [`PageParams`] page under the `Link` header the client
//!   chat lists' shared page builder keys on the page's chat ids. An
//!   unknown nickname is the 404 `{"error":"Not found"}` the admin account
//!   reads answer with.
//! - `GET /chats/{id}` (`show`) — the chat whose row carries that id, as
//!   the [`Chat`] entity its owner sees (the `account` is the chat's other
//!   participant); a malformed or unknown id is one 404.
//! - `GET /chats/{id}/messages` (`messages`) — that chat's
//!   [`ChatMessage`] entities, newest first, one [`PageParams`] page under
//!   the `Link` header keyed on the reference ids, exactly as the owner's
//!   own listing carries them; a withdrawn message is already excluded by
//!   [`chat::messages_page`]'s live filter, so staff never see text the
//!   participants can no longer read.
//! - `DELETE /chats/{id}/messages/{message_id}` (`delete_message`) —
//!   Pleroma's admin withdrawal, and the piece that makes it moderation
//!   rather than the owner-side route's self-service delete:
//!   [`chat::withdraw`] tombstones the message for **both** participants
//!   whoever wrote it (the user-side delete only hides the caller's own
//!   copy), [`federation_out::after_chat_message_deleted_by_admin`] announces the
//!   withdrawal to the network when the message was this instance's to
//!   announce, and [`moderation_log::chat_message_delete`] records the
//!   staff action. `message_id` is the reference id inside the named chat,
//!   the same id the client API pages and marks by; a malformed one, an
//!   unknown one and a reference of another chat are one 404, and so is a
//!   second delete of a message the first one already withdrew. The answer
//!   is the [`ChatMessage`] entity as it stood before the delete — what
//!   Pleroma's `render("show.json", cm_ref)` shows from the reference it
//!   read first.
//!
//! [`AdminUser`]: crate::auth::admin::AdminUser
//! [`StaffUser`]: crate::auth::admin::StaffUser

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
    routing::{delete, get},
};
use sqlx::PgPool;
use tracing::debug;

use crate::{
    api::chats::{chat_entities, chat_entity, message_of, message_page_link, page_link},
    auth::admin::{AdminUser, StaffUser},
    entities::chat::{Chat, ChatMessage},
    error::{AppError, AppResult},
    models::{chat, chat::ChatFilter, moderation_log, user::admin},
    pagination::{MAX_LIMIT, PageParams, set_link_header},
    services::federation_out,
    state::AppState,
    util::id::parse_id,
};

/// The read endpoint class of [`AdminUser::require`].
const CLASS_READ: &str = "read";
/// The write endpoint class of [`StaffUser::require`].
const CLASS_WRITE: &str = "write";
/// The admin scope area these endpoints live in (`admin:read:chats` and
/// `admin:write:chats`, Pleroma's `OAuthScopesPlug` grants for the chat
/// controller and `list_user_chats`).
const AREA_CHATS: &str = "chats";

/// The chat routes of the admin API, merged into [`super`].
///
/// Do not call [`Router::fallback`] here (or on anything merged into this
/// tree): the app-wide JSON fallback in [`crate::app`] must answer the
/// unmatched paths.
///
/// [`Router::fallback`]: axum::Router::fallback
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/users/{nickname}/chats", get(list_user_chats))
        .route("/chats/{id}", get(show_chat))
        .route("/chats/{id}/messages", get(list_messages))
        .route("/chats/{id}/messages/{message_id}", delete(delete_message))
}

/// `GET /api/v1/pleroma/admin/users/{nickname}/chats` — Pleroma's
/// `list_user_chats`: that account's chats as the [`Chat`] entities, newest
/// activity first, one page under the `Link` header keyed on chat ids.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::RouteNotFound`] for text that is
/// no stored nickname and no id; [`AppError::BadRequest`] for a malformed
/// pagination cursor ([`PageParams::from_query`]); [`AppError::NotFound`]
/// through [`chat_entity`] for a chat whose recipient row vanished. Any
/// database failure arrives through `?` as [`AppError::Database`], and
/// [`set_link_header`] reports a `Link` value that is not header-safe as
/// [`AppError::Internal`].
async fn list_user_chats(
    State(state): State<AppState>,
    admin: AdminUser,
    OriginalUri(uri): OriginalUri,
    Path(raw): Path<String>,
) -> AppResult<(HeaderMap, Json<Vec<Chat>>)> {
    admin.require(CLASS_READ, AREA_CHATS)?;
    let target = admin::get_by_nickname_or_id(&state.db, &raw)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    let page = PageParams::from_query(uri.query(), MAX_LIMIT)?;
    let rows = chat::page_for_user(&state.db, target.id, page, ChatFilter::default()).await?;
    let link = page_link(&state.config.base_url, uri.path(), &rows);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    debug!(
        user = %target.id,
        listed = rows.len(),
        "admin user chats listed"
    );
    Ok((headers, Json(chat_entities(&state, &rows).await?)))
}

/// `GET /api/v1/pleroma/admin/chats/{id}` — Pleroma's admin `show`: the
/// chat the id names, whoever owns it, as the [`Chat`] entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::NotFound`] for a malformed or
/// unknown id (see [`chat_by_id`]) or a chat whose recipient row vanished;
/// any database failure arrives through `?` as [`AppError::Database`].
async fn show_chat(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<Chat>> {
    admin.require(CLASS_READ, AREA_CHATS)?;
    let row = chat_by_id(&state.db, &raw).await?;
    debug!(chat = %row.id, "admin chat shown");
    chat_entity(&state, &row).await.map(Json)
}

/// `GET /api/v1/pleroma/admin/chats/{id}/messages` — Pleroma's admin
/// `messages`: the chat's [`ChatMessage`] entities, newest first, one page
/// under the `Link` header keyed on reference ids, the same cursors the
/// owner's client pages by. A withdrawn message is not among them: the
/// listing runs the model's live filter, the same one the participants'
/// clients see through.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::NotFound`] for a malformed or
/// unknown `id` (see [`chat_by_id`]); [`AppError::BadRequest`] for a
/// malformed pagination cursor ([`PageParams::from_query`]). Any database
/// failure arrives through `?` as [`AppError::Database`], and
/// [`set_link_header`] reports a `Link` value that is not header-safe as
/// [`AppError::Internal`].
async fn list_messages(
    State(state): State<AppState>,
    admin: AdminUser,
    OriginalUri(uri): OriginalUri,
    Path(raw): Path<String>,
) -> AppResult<(HeaderMap, Json<Vec<ChatMessage>>)> {
    admin.require(CLASS_READ, AREA_CHATS)?;
    let row = chat_by_id(&state.db, &raw).await?;
    let page = PageParams::from_query(uri.query(), MAX_LIMIT)?;
    let references = chat::messages_page(&state.db, row.id, page).await?;
    let link = message_page_link(&state.config.base_url, uri.path(), &references);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    debug!(
        chat = %row.id,
        listed = references.len(),
        "admin chat messages listed"
    );
    let mut entities = Vec::with_capacity(references.len());
    for reference in &references {
        entities.push(message_of(&state, reference).await?);
    }
    Ok((headers, Json(entities)))
}

/// `DELETE /api/v1/pleroma/admin/chats/{id}/messages/{message_id}` — the
/// moderation withdrawal: the message behind the chat's reference becomes a
/// tombstone for both participants, its `Delete` goes out to a remote
/// partner when this instance minted the object, and the action lands in
/// the moderation log. The answer is the entity as the chat's owner saw it
/// before the delete ran — the text is gone the moment this returns.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:write` gate; [`AppError::NotFound`] for a malformed or
/// unknown `id`, or a `message_id` that is malformed, names no reference
/// *of that chat* (another chat's reference is foreign here, exactly as on
/// the client route), or names one whose message this call already
/// withdrew. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn delete_message(
    State(state): State<AppState>,
    staff: StaffUser,
    Path((raw, message_raw)): Path<(String, String)>,
) -> AppResult<Json<ChatMessage>> {
    staff.require(CLASS_WRITE, AREA_CHATS)?;
    let row = chat_by_id(&state.db, &raw).await?;
    let Some(reference_id) = parse_id(&message_raw) else {
        return Err(AppError::NotFound);
    };
    let Some(reference) = chat::message_reference(&state.db, row.id, reference_id).await? else {
        return Err(AppError::NotFound);
    };
    // Rendered before the tombstone, from the reference the chat's owner
    // still has: the entity reports the text the delete is taking away.
    let entity = message_of(&state, &reference).await?;
    if !chat::withdraw(&state.db, reference.message_id).await? {
        return Err(AppError::NotFound);
    }
    federation_out::after_chat_message_deleted_by_admin(&state, &row, &reference).await;
    let logged = moderation_log::chat_message_delete(&state.db, &staff.0, reference.id).await?;
    debug!(
        log = %logged.id,
        chat = %row.id,
        message = %reference.message_id,
        actor = %staff.0.id,
        "staff deleted a chat message"
    );
    Ok(Json(entity))
}

/// The `{id}` segment of an admin chat route as the stored [`chat::Chat`]
/// row: a malformed id and an unknown one are one 404, and — the whole
/// difference from the client route's owned read — no row is foreign,
/// because the admin names any chat by its id alone.
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives
/// through `?` as [`AppError::Database`].
async fn chat_by_id(pool: &PgPool, text: &str) -> AppResult<chat::Chat> {
    let Some(id) = parse_id(text) else {
        return Err(AppError::NotFound);
    };
    chat::get_by_id(pool, id).await?.ok_or(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A malformed path segment is refused as the 404 before any read
    /// would run: this is the one id parse every admin chat route shares.
    /// The unknown-but-well-formed id arm reaches the database, and the
    /// HTTP tests pin that half alongside it.
    #[tokio::test]
    async fn junk_path_segments_are_the_one_404_before_any_read() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no server");
        for raw in ["not-a-uuid", "", "01890b9a"] {
            let err = chat_by_id(&pool, raw)
                .await
                .expect_err("no id text can resolve");
            assert!(
                matches!(err, AppError::NotFound),
                "the malformed-id refusal is a 404: {err}"
            );
        }
    }
}
