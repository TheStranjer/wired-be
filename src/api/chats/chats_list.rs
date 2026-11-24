//! The chats themselves: opening one, listing them, and showing one.
//!
//! Pleroma's `create`, `index`, `index2` and `show` actions, beside the
//! shared lookups and [`Chat`] renderers the other children of this
//! module build on — `owned_chat` for the `{id}` segment, `chat_entity`
//! and `chat_json` for the entity, `newest_message` for its last
//! message.

use axum::{
    Json,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
};
use serde::Deserialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::account_or_404,
    auth::AuthUser,
    entities::chat::{Chat, ChatMessage},
    error::{AppError, AppResult},
    extract::{ApiParams, flex},
    models::{block, chat, chat::ChatFilter, user::User},
    oauth::scopes::{READ_CHATS, WRITE_CHATS},
    pagination::{MAX_LIMIT, NO_EXTRA_QUERY, PageParams, keyset_link, set_link_header},
    state::AppState,
    util::id::parse_id,
};

use super::{
    CANT_CHAT_WITH_SELF, CHAT_NOT_ALLOWED, LEGACY_MAX_LIMIT, NEWEST_PAGE, RECIPIENT_REJECTS_CHATS,
    messages::{chat_recipient, message_of},
};

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
pub(super) async fn create_chat(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Chat>> {
    auth.require(WRITE_CHATS)?;
    let recipient = account_or_404(&state, &id).await?;
    if let Some(refusal) = chat_refusal(&state, &auth.0, &recipient).await? {
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
/// The block check is [`block::blocked_between`] over `state`'s pool.
/// [`post_message`] asks the same question of an existing chat's pair.
///
/// [`accepts_chat_messages`]: crate::models::user::User::accepts_chat_messages
/// [`post_message`]: super::post_message::post_message
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
pub(super) async fn chat_refusal(
    state: &AppState,
    user: &User,
    recipient: &User,
) -> AppResult<Option<AppError>> {
    if user.id == recipient.id {
        return Ok(Some(AppError::Unprocessable(
            CANT_CHAT_WITH_SELF.to_owned(),
        )));
    }
    if !recipient.accepts_chat_messages.get() {
        return Ok(Some(AppError::Forbidden(
            RECIPIENT_REJECTS_CHATS.to_owned(),
        )));
    }
    if block::blocked_between(&state.db, user.id, recipient.id).await? {
        return Ok(Some(AppError::Forbidden(CHAT_NOT_ALLOWED.to_owned())));
    }
    Ok(None)
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
pub(super) async fn list_chats(
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
pub(super) async fn list_chats_v2(
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
/// activity order under the `Link` header [`keyset_link`] derives from their
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
    // The page's activity order stands, so the cursors name the first and
    // last rows' chat ids.
    let link = keyset_link(
        &state.config.base_url,
        path,
        &NO_EXTRA_QUERY,
        &rows,
        |row| row.id,
    );
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    debug!(
        count = rows.len(),
        user = %user_id,
        "chats listed"
    );
    Ok((headers, Json(chat_entities(state, &rows).await?)))
}

/// The [`Chat`] entities for `rows`, each rendered the way [`chat_json`]
/// renders one. The admin chat listing (WBE-202) builds its pages through
/// this same fan-out.
///
/// # Errors
///
/// As [`chat_entity`]'s.
pub(crate) async fn chat_entities(state: &AppState, rows: &[chat::Chat]) -> AppResult<Vec<Chat>> {
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
pub(super) async fn show_chat(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Chat>> {
    auth.require(READ_CHATS)?;
    let row = owned_chat(&state, auth.0.id, &id).await?;
    chat_json(&state, &row).await
}

/// The `with_muted` and `pinned` query parameters of both chat lists
/// (Pleroma's `index_query`).
#[derive(Debug, Deserialize)]
pub(super) struct ChatListParams {
    /// Whether chats with muted accounts are listed (Pleroma's
    /// `with_muted`); absent means `false`. See [`ChatFilter::with_muted`].
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
pub(super) async fn owned_chat(state: &AppState, user_id: Uuid, id: &str) -> AppResult<chat::Chat> {
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
pub(super) async fn chat_json(state: &AppState, row: &chat::Chat) -> AppResult<Json<Chat>> {
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::api::chats::test_support::{app_state, chatter, quiet, quiet_remote};

    #[tokio::test]
    async fn chatting_with_yourself_is_the_422() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = chatter(&pool, "alice").await;

        let err = chat_refusal(&app_state(&pool), &alice, &alice)
            .await
            .expect("the refusal check runs")
            .expect("the self-chat is refused");

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
            let err = chat_refusal(&app_state(&pool), &alice, &silent)
                .await
                .expect("the refusal check runs")
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
    async fn an_ordinary_pair_passes_and_a_blocked_pair_is_refused_either_way() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = chatter(&pool, "alice").await;
        let bob = chatter(&pool, "bob").await;
        let carol = chatter(&pool, "carol").await;

        assert!(
            chat_refusal(&state, &alice, &bob)
                .await
                .expect("the refusal check runs")
                .is_none(),
            "two chatters may chat"
        );

        let _block = block::block(&pool, &state.config.base_url, &bob, &alice)
            .await
            .expect("bob blocks alice");
        for (user, recipient) in [(&alice, &bob), (&bob, &alice)] {
            let err = chat_refusal(&state, user, recipient)
                .await
                .expect("the refusal check runs")
                .expect("a blocked pair is refused");
            assert!(
                matches!(err, AppError::Forbidden(_)),
                "the block is a 403: {err}"
            );
            assert_eq!(err.to_string(), CHAT_NOT_ALLOWED, "Pleroma's wording");
        }
        assert!(
            chat_refusal(&state, &alice, &carol)
                .await
                .expect("the refusal check runs")
                .is_none(),
            "the block touches only its own pair"
        );
    }

    #[tokio::test]
    async fn the_flag_gate_stands_ahead_of_the_block_gate() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = chatter(&pool, "alice").await;
        let mallory = quiet_remote(&pool, "mallory").await;
        let _block = block::block(&pool, &state.config.base_url, &alice, &mallory)
            .await
            .expect("alice blocks mallory");

        let err = chat_refusal(&state, &alice, &mallory)
            .await
            .expect("the refusal check runs")
            .expect("flag down is a refusal");
        assert_eq!(
            err.to_string(),
            RECIPIENT_REJECTS_CHATS,
            "the silent recipient is refused by name, not as a block"
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
}
