//! `POST /api/v1/pleroma/chats/{id}/messages` — Pleroma's
//! `post_chat_message`: the [`PostMessageParams`] body and its gates, the
//! [`media_attachment`] lookup, the `Idempotency-Key` replay check and
//! the transactional write ([`send_message`]) behind the route. Reading
//! and deleting messages lives in [`super::messages`].

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use serde::Deserialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    entities::chat::ChatMessage,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        chat,
        chat::{ChatMessageReference, NewChatMessage},
        media_attachment,
        user::User,
    },
    oauth::scopes::WRITE_CHATS,
    state::AppState,
    util::id::parse_id,
};

use super::{
    EMPTY_CHAT_MESSAGE, IDEMPOTENCY_KEY_HEADER, MEDIA_NOT_FOUND, MESSAGE_TOO_LONG,
    chats_list::owned_chat,
    events::after_chat_message_created,
    messages::{chat_recipient, message_json, message_of},
};

/// The body of `POST /api/v1/pleroma/chats/{id}/messages`: Pleroma's
/// `CreateMessageRequest` (its api spec), whose two keys are each optional —
/// the refusal for a body carrying neither is [`PostMessageParams::into_new_message`]'s.
#[derive(Debug, Deserialize)]
pub(super) struct PostMessageParams {
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
pub(super) async fn post_message(
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
    after_chat_message_created(&state, &reference, &auth.0, &recipient).await;
    let entity = message_of(&state, &reference).await?;
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
/// sides' chat rows and references commit together. The delivery of a
/// message to a remote counterpart is the hook the caller fires after this
/// returns: a queued `federator.publish` job, not a second write, so the
/// posting stands on its own while the wire stays the worker's work.
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
    let (_, sender_reference, _) = chat::add_message(
        &mut tx,
        &state.config.base_url,
        sender,
        recipient,
        new_message,
    )
    .await?;
    tx.commit().await?;
    Ok(sender_reference)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use serde_json::json;

    use super::*;

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
}
