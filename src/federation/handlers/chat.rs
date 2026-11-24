//! Inbound `Create` and `Delete` of a `ChatMessage`: what a peer messaging a
//! local account — or withdrawing its own message — does to this instance.
//!
//! The reference is Pleroma's `ChatMessageValidator`
//! (`lib/pleroma/web/activity_pub/object_validators/chat_message_validator.ex`)
//! feeding the `ChatMessage` clause of `SideEffects.handle_object_creation`
//! (`lib/pleroma/web/activity_pub/side_effects.ex`) and the
//! `pleroma:chat_mention` arm of `Pleroma.Notification`
//! (`lib/pleroma/notification.ex`): a delivered chat message is checked
//! against its sender and its single recipient, stored in the pair's chats
//! with the recipient's side unread, and told to the recipient. The ticket
//! rules are authoritative where the two differ:
//!
//! * [`create_chat_message`], registered in the dispatcher's
//!   `create_by_object_type` table under `ChatMessage`, reads the embedded
//!   document — `{id, type, attributedTo = actor, to: [one recipient],
//!   content (HTML, sanitized), attachment (optional single
//!   `Document`/`Image`/`Video`/`Audio`), published, emoji?}` — and gates
//!   it: the object's `attributedTo` must name the activity's `actor`
//!   (containment, shared with [`super::create`]), the sender must be an
//!   *active remote* account, the single recipient a *local active* account
//!   that accepts chat messages and has not blocked the sender, and the
//!   document must carry a usable `published` time and either text or
//!   media. The fold then writes one transaction — the remote media row,
//!   [`chat::add_message`] with the object id as the message's `ap_id` (so
//!   a redelivered object dedupes on the unique column, a withdrawn one
//!   never re-imports, and the recipient's reference is the unread one the
//!   model itself writes) — and the recipient's `pleroma:chat_mention`
//!   notification, naming the message and no status, in the same
//!   transaction; then it finishes with the chat API's documented
//!   `after_chat_message_created` hook.
//! * [`delete_chat_message`], the `Delete` counterpart under the same key,
//!   resolves the object id to the stored message, honours only its
//!   sender — anybody else's `Delete` of somebody's chat message is nobody's
//!   business — and withdraws: the row becomes a tombstone and the
//!   notifications about it go with it. Both writes are idempotent, so a
//!   retry between them finishes what it restarts.
//!
//! A delivered `emoji` map is accepted and left unmapped: like the entity's
//! always-empty `emojis` list, chat shortcode mapping is a later ticket's.
//! A `Delete` whose object is a *bare id* (Pleroma's shape) or an embedded
//! `Tombstone` declares no `ChatMessage` type and so routes past this entry
//! to the note handler; that handler looks the id up among the stored chat
//! messages first and hands a hit to `withdraw_chat_message`, so the same
//! sender check and withdrawal apply.
//!
//! The outgoing side — delivering a local user's own chat messages and
//! their sender's `Delete`s — is the chat federation hooks' other half,
//! [`crate::services::federation_out`]'s chat arms; nothing here sends
//! anything anywhere.

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use serde_json::Value;
use sqlx::PgConnection;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::chats::after_chat_message_created,
    error::AppError,
    federation::{
        dispatch::{ActivityView, FederationError, Handlers, Outcome},
        json,
        note_parser::pixel_dimension,
    },
    models::{
        block, chat,
        chat::NewChatMessage,
        media_attachment::{self, NewMediaAttachment},
        notification::{self, NewNotification, TYPE_CHAT_MENTION},
        user::{self, User},
    },
    state::AppState,
    util::sanitize::{sanitize_html, strip_html},
};

/// The object type both of this module's sub-registry entries claim.
const CHAT_MESSAGE_TYPE: &str = "ChatMessage";

/// The attachment kinds a chat message may carry — the ticket's set, which is
/// Pleroma's chat-arm validator list: the note family's `Link` entry is
/// nobody's media.
const ATTACHMENT_TYPES: [&str; 4] = ["Document", "Image", "Video", "Audio"];

/// The `mediaType` Pleroma's attachment validator defaults when a document
/// declares none, and the fallback this fold stores beside it.
const DEFAULT_MEDIA_TYPE: &str = "application/octet-stream";

/// Folds this module's chat routes into the dispatcher's registry: one
/// `create_by_object_type` and one `delete_by_object_type` entry, both keyed
/// `ChatMessage`.
///
/// `crate::federation::handlers::registry` calls it, which is the whole
/// composition of the live registry.
pub fn register(handlers: &mut Handlers) {
    handlers.register_create(CHAT_MESSAGE_TYPE, create_chat_message);
    handlers.register_delete(CHAT_MESSAGE_TYPE, delete_chat_message);
}

/// The `Create` handler of a `ChatMessage`: gate the document and its pair,
/// fold it into the chat, and tell the recipient.
///
/// # Errors
///
/// [`FederationError::Transient`] for a database failure the retry may
/// outlive, [`FederationError::Permanent`] for the model's own refusal of an
/// over-long message; every refusal of shape arrives as [`Outcome::Ignored`].
pub fn create_chat_message(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_create(state, view, signer).await })
}

/// The `Delete` handler of a `ChatMessage`: the sender's own withdrawal,
/// folded into the tombstone; anybody else's is ignored.
///
/// # Errors
///
/// [`FederationError::Transient`] for a database failure; the two writes are
/// idempotent, so the retry completes what it restarts.
pub fn delete_chat_message(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_delete(state, view, signer).await })
}

/// The body of [`create_chat_message`]: the shape and pair gates, the dedupe
/// read, then [`deliver`].
async fn apply_create(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    let Some(object) = view.object.embedded() else {
        debug!(
            object_id = view.object.id().unwrap_or_default(),
            "a chat Create whose object is only a reference; this instance does not fetch it yet"
        );
        return Ok(Outcome::Ignored("object not embedded"));
    };
    let Some(ap_id) = view.object.id() else {
        return Ok(Outcome::Ignored("a chat message whose object names no id"));
    };
    if !super::create::names_actor(object, &view.actor) {
        return Ok(Outcome::Ignored("attributedTo is not the activity actor"));
    }
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "a chat message sent by an account this instance owns",
        ));
    }
    if !signer.is_active.get() {
        return Ok(Outcome::Ignored(
            "a chat message sent by a deactivated account",
        ));
    }
    let stored = chat::message_by_ap_id(&state.db, ap_id)
        .await
        .map_err(|err| {
            FederationError::transient("the chat message fold failed at the dedupe read", &err)
        })?;
    if let Some(message) = stored {
        debug!(message = %message.id, "a Create restates an already delivered chat message");
        return Ok(restatement_of(&message));
    }
    let incoming = match shape_of(object, ap_id, Utc::now()) {
        Ok(incoming) => incoming,
        Err(reason) => {
            debug!(reason, "a delivered chat message is malformed; discarding");
            return Ok(Outcome::Ignored(reason));
        }
    };
    deliver(&state, &incoming, &signer).await
}

/// What a second sight of a stored object answers: the live row's restatement
/// is handled (a second `Create` of a delivered message tells the recipient
/// nothing new), the tombstone's is refused — a withdrawn message is never
/// re-imported, which is why its `ap_id` stays reserved.
const fn restatement_of(message: &chat::ChatMessage) -> Outcome {
    match message.deleted_at {
        Some(_) => Outcome::Ignored("a withdrawn chat message is never re-imported"),
        None => Outcome::Handled,
    }
}

/// The delivered `ChatMessage` in exactly the parts the fold needs.
#[derive(Debug)]
struct IncomingChat<'a> {
    /// The object's `ActivityPub` id — stored as `chat_messages.ap_id`, the
    /// key the delivery dedupes on (decision 6).
    ap_id: &'a str,
    /// The actor id of the single account the object addresses.
    recipient_ap_id: String,
    /// The message text as sanitized HTML.
    content: String,
    /// The same text with its markup stripped — what the model's length
    /// backstop measures.
    raw_text: String,
    /// The object's `published` time, clamped against [`json::CLOCK_SKEW`]; it
    /// becomes the message row's creation time.
    published: DateTime<Utc>,
    /// The document's first attachment, when it carries chat media.
    attachment: Option<ChatAttachment>,
}

/// One delivered chat attachment, in the columns the media row keeps.
#[derive(Debug)]
struct ChatAttachment {
    /// The `http(s)` url of the media.
    url: String,
    /// The declared `mediaType`, or [`DEFAULT_MEDIA_TYPE`].
    content_type: String,
    /// The entry's `name`: the alt text.
    description: Option<String>,
    /// The `blurhash` placeholder string.
    blurhash: Option<String>,
    /// Width in pixels, when the entry states a positive whole one.
    width: Option<i32>,
    /// Height in pixels, likewise.
    height: Option<i32>,
}

/// What the object's `attachment` key held.
enum Attachment {
    /// No attachment at all: the key is absent, or empty, or null —
    /// Pleroma's `fix_attachment` drops those before validation.
    Absent,
    /// The first entry, kept as chat media; every later one is thrown away,
    /// exactly as `fix_attachment` throws them away.
    Media(ChatAttachment),
    /// An entry the fold cannot store: not one of [`ATTACHMENT_TYPES`], or
    /// carrying no `http(s)` url. Pleroma's validator refuses the whole
    /// message for one, so this does too.
    Unusable,
}

/// Reads the embedded document into an [`IncomingChat`], refusing (with the
/// static reason the handler reports) everything Pleroma's validator refuses
/// of this shape: an addressing that is not exactly one recipient, a missing
/// or unparseable `published`, unusable media, neither text nor media to
/// show.
fn shape_of<'a>(
    object: &Value,
    ap_id: &'a str,
    now: DateTime<Utc>,
) -> Result<IncomingChat<'a>, &'static str> {
    let recipient_ap_id = single_recipient(object)
        .ok_or("a chat message that is not addressed to exactly one account")?;
    let published =
        published_time(object, now).ok_or("a chat message without a usable published time")?;
    let markup = object
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let content = sanitize_html(markup);
    let attachment = match attachment_of(object) {
        Attachment::Absent => None,
        Attachment::Media(media) => Some(media),
        Attachment::Unusable => {
            return Err("a chat message whose attachment is not usable media");
        }
    };
    if content.trim().is_empty() && attachment.is_none() {
        return Err("a chat message with neither content nor attachment");
    }
    Ok(IncomingChat {
        ap_id,
        recipient_ap_id,
        content,
        raw_text: strip_html(markup),
        published,
        attachment,
    })
}

/// The object's one recipient: the address of its `to` when exactly one names
/// something (Pleroma's `validate_length(:to, is: 1)`). A public address is
/// just another id at this point — the stored-account read of [`recipient_of`]
/// is what refuses it.
fn single_recipient(object: &Value) -> Option<String> {
    let recipients = object.get("to").map_or_else(Vec::new, json::references);
    match recipients.as_slice() {
        [only] => Some(only.clone()),
        _ => None,
    }
}

/// The object's `published` instant: required (Pleroma's validator names it
/// among the fields), parsed by [`json::clamped_time`], and answered `now`
/// whenever it leads the local clock past [`json::CLOCK_SKEW`] — a remote
/// clock cannot move a message down somebody's chat list.
fn published_time(object: &Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    json::clamped_time(object.get("published"), now)
}

/// The first `attachment` entry of the document, in Pleroma's
/// `fix_attachment` reading: whatever the key holds, only its first element
/// is kept, and a kept entry that is not usable chat media refuses the whole
/// message ([`Attachment::Unusable`]).
fn attachment_of(object: &Value) -> Attachment {
    let Some(value) = object.get("attachment") else {
        return Attachment::Absent;
    };
    match json::as_array(value).first() {
        None | Some(Value::Null) => Attachment::Absent,
        Some(entry) => chat_attachment(entry).map_or(Attachment::Unusable, Attachment::Media),
    }
}

/// One kept entry as the media row's data: of an [`ATTACHMENT_TYPES`] kind
/// (the list that excludes `Link`), carrying an `http(s)` url under `url` or
/// `href`, with the declared `mediaType` — or the validator's default — and
/// the alt text, `blurhash` and dimensions read as the document states them.
fn chat_attachment(entry: &Value) -> Option<ChatAttachment> {
    let kind = entry.get("type").and_then(json::first_type)?;
    if !ATTACHMENT_TYPES.contains(&kind.as_str()) {
        return None;
    }
    let url = json::first_string(entry.get("url")?)
        .or_else(|| json::first_string(entry.get("href")?))
        .filter(|candidate| json::http_host(candidate).is_some())?;
    Some(ChatAttachment {
        url,
        content_type: declared_media_type(entry),
        description: entry.get("name").and_then(Value::as_str).map(str::to_owned),
        blurhash: entry
            .get("blurhash")
            .and_then(Value::as_str)
            .map(str::to_owned),
        width: pixel_dimension(entry.get("width")),
        height: pixel_dimension(entry.get("height")),
    })
}

/// The entry's declared `mediaType` — `mimeType` included, under the
/// validator's own `fix_media_type` — with [`DEFAULT_MEDIA_TYPE`] standing in
/// for anything absent or blank.
fn declared_media_type(entry: &Value) -> String {
    entry
        .get("mediaType")
        .or_else(|| entry.get("mimeType"))
        .and_then(Value::as_str)
        .filter(|declared| !declared.trim().is_empty())
        .unwrap_or(DEFAULT_MEDIA_TYPE)
        .to_owned()
}

/// What the recipient gates decided.
enum Decision {
    /// The account to deliver to, cleared of every gate.
    Deliver(Box<User>),
    /// Why this delivery has nobody to fold for; the reason is static and
    /// rides the dispatcher's debug log.
    Refuse(&'static str),
}

/// The recipient's side of the gates: the object's single address names a
/// stored account, that account is local (whose chat rows this instance
/// keeps), active and accepting chat messages, and it has not blocked the
/// sender — Pleroma's `validate_local_concern`, per the ticket narrowed to
/// the one delivery direction this handler can receive (`User.blocks?` of
/// the recipient against the actor, which [`block::is_blocking`] is).
async fn recipient_of(
    state: &AppState,
    incoming: &IncomingChat<'_>,
    sender: &User,
) -> Result<Decision, FederationError> {
    let Some(recipient) = user::get_by_ap_id(&state.db, &incoming.recipient_ap_id)
        .await
        .map_err(|err| {
            FederationError::transient("the chat message fold failed at the recipient read", &err)
        })?
    else {
        return Ok(Decision::Refuse(
            "a chat message addressed to an account this instance does not store",
        ));
    };
    if let Some(reason) = recipient_refusal(&recipient) {
        return Ok(Decision::Refuse(reason));
    }
    if block::is_blocking(&state.db, recipient.id, sender.id)
        .await
        .map_err(|err| {
            FederationError::transient("the chat message fold failed at the blocks read", &err)
        })?
    {
        return Ok(Decision::Refuse(
            "a chat message from an account its recipient has blocked",
        ));
    }
    Ok(Decision::Deliver(Box::new(recipient)))
}

/// Why a stored account cannot be a chat recipient: a remote account, whose
/// side of a conversation lives on its own instance; a deactivated account;
/// an account with the chat flag down. The order the ticket names them.
const fn recipient_refusal(recipient: &User) -> Option<&'static str> {
    if !recipient.local.get() {
        return Some("a chat message addressed to a remote account");
    }
    if !recipient.is_active.get() {
        return Some("a chat message addressed to a deactivated account");
    }
    if !recipient.accepts_chat_messages.get() {
        return Some("a chat message addressed to an account that does not accept chats");
    }
    None
}

/// The accepted delivery: the write ([`store`]), then the chat API's
/// documented hook over the recipient's own reference — the local side of
/// an inbound pair is the recipient, and the hook federates nothing for a
/// local one (it is the outbound chat ticket's counterpart).
async fn deliver(
    state: &AppState,
    incoming: &IncomingChat<'_>,
    sender: &User,
) -> Result<Outcome, FederationError> {
    let recipient = match recipient_of(state, incoming, sender).await? {
        Decision::Deliver(recipient) => recipient,
        Decision::Refuse(reason) => {
            debug!(
                recipient = %incoming.recipient_ap_id,
                reason, "the recipient refuses the delivered chat message"
            );
            return Ok(Outcome::Ignored(reason));
        }
    };
    let written = store(state, incoming, sender, &recipient).await?;
    let view = written
        .recipient_reference
        .as_ref()
        .unwrap_or(&written.sender_reference);
    after_chat_message_created(state, view, sender, &recipient).await;
    debug!(
        message = %written.message.id,
        recipient = %recipient.id,
        "a delivered chat message became a chat"
    );
    Ok(Outcome::Handled)
}

/// What one folded delivery left behind: the shared message row and the two
/// sides' references.
struct Written {
    /// The stored message.
    message: chat::ChatMessage,
    /// The sender's side's view, which [`chat::add_message`] writes even for
    /// a remote sender.
    sender_reference: chat::ChatMessageReference,
    /// The recipient's side's view, unread; `add_message` answers `None` only
    /// for a remote recipient, and a delivered message's recipient is local.
    recipient_reference: Option<chat::ChatMessageReference>,
}

/// The delivery's write on one transaction: the remote media row when the
/// document carried one, [`chat::add_message`] — both sides' chat rows and
/// references, the recipient's unread — and the recipient's
/// `pleroma:chat_mention` notification, all committing together. A message
/// that stands without its telling (or the reverse) would make the dedupe
/// read of a retry answer the fold complete while half of it is missing.
async fn store(
    state: &AppState,
    incoming: &IncomingChat<'_>,
    sender: &User,
    recipient: &User,
) -> Result<Written, FederationError> {
    let mut tx = state.db.begin().await.map_err(|err| {
        FederationError::transient(
            "the chat message fold failed at the delivery transaction",
            &err,
        )
    })?;
    let attachment_media_id = match &incoming.attachment {
        Some(attachment) => Some(attach_media(&mut tx, attachment).await?),
        None => None,
    };
    let new_message = NewChatMessage {
        content: incoming.content.clone(),
        raw_text: incoming.raw_text.clone(),
        attachment_media_id,
        ap_id: Some(incoming.ap_id.to_owned()),
        idempotency_key: None,
        inserted_at: Some(incoming.published),
    };
    let (message, sender_reference, recipient_reference) = chat::add_message(
        &mut tx,
        &state.config.base_url,
        sender,
        recipient,
        &new_message,
    )
    .await
    .map_err(|err| delivery_refusal(&err))?;
    let _told = notification::create(&mut *tx, &chat_mention(sender, recipient, message.id))
        .await
        .map_err(|err| {
            FederationError::transient(
                "the chat message fold failed at the chat mention notification",
                &err,
            )
        })?;
    tx.commit().await.map_err(|err| {
        FederationError::transient("the chat message fold failed at the delivery commit", &err)
    })?;
    Ok(Written {
        message,
        sender_reference,
        recipient_reference,
    })
}

/// The delivered attachment as a remote `media_attachments` row, on the
/// message's open transaction: no uploader (`user_id` stays `None` — nobody
/// here uploaded it), the document url in both `remote_url` and `url` (where
/// the media proxy mirrors from, and what serves until it has), and the
/// declared type, alt text, `blurhash` and dimensions as stated.
async fn attach_media(
    conn: &mut PgConnection,
    attachment: &ChatAttachment,
) -> Result<Uuid, FederationError> {
    let new = NewMediaAttachment {
        user_id: None,
        status_id: None,
        position: 0,
        remote_url: Some(attachment.url.clone()),
        url: attachment.url.clone(),
        content_type: attachment.content_type.clone(),
        description: attachment.description.clone(),
        blurhash: attachment.blurhash.clone(),
        width: attachment.width,
        height: attachment.height,
        ..NewMediaAttachment::default()
    };
    let row = media_attachment::insert(&mut *conn, &new)
        .await
        .map_err(|err| {
            FederationError::transient(
                "the chat message fold failed at the attachment insert",
                &err,
            )
        })?;
    Ok(row.id)
}

/// The delivery's telling: [`TYPE_CHAT_MENTION`] for the recipient, caused by
/// the sender, naming the message — and no status, because the chat message
/// is the notification's whole subject (the `chat_message_id` column the
/// entity renders its `chat_message` from).
const fn chat_mention<'a>(
    sender: &'a User,
    recipient: &'a User,
    message_id: Uuid,
) -> NewNotification<'a> {
    NewNotification {
        user_id: recipient.id,
        from_account_id: sender.id,
        notification_type: TYPE_CHAT_MENTION,
        status_id: None,
        emoji: None,
        report_id: None,
        chat_message_id: Some(message_id),
    }
}

/// Why a delivery's write failed: the model's length refusal is the
/// document's own fault — no retry changes a too-long message — and
/// everything else is the database being unreachable, which the queue's next
/// attempt may outlive.
fn delivery_refusal(err: &AppError) -> FederationError {
    if let AppError::Unprocessable(why) = err {
        return FederationError::Permanent(why.clone());
    }
    FederationError::transient("the chat message fold failed at the message write", err)
}

/// The body of [`delete_chat_message`]: resolve the object id to the stored
/// message, honour only its sender, and withdraw — the tombstone takes the
/// text out of both sides' listings and the recipient's `pleroma:chat_mention`
/// telling goes with it. The two statements are idempotent, so a retry that
/// lands between them completes the job rather than doubling anything.
async fn apply_delete(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    let Some(ap_id) = view.object.id() else {
        return Ok(Outcome::Ignored("a Delete whose object names no id"));
    };
    let Some(message) = chat::message_by_ap_id(&state.db, ap_id)
        .await
        .map_err(|err| {
            FederationError::transient("the chat message fold failed at the message read", &err)
        })?
    else {
        debug!(
            ap_id,
            "a Delete names no chat message this instance stored; ignoring"
        );
        return Ok(Outcome::Ignored(
            "a chat message this instance never stored",
        ));
    };
    withdraw_chat_message(&state, &message, &signer).await
}

/// The withdrawal of an already resolved chat `message` by `signer`: only its
/// sender is honoured, and then the row becomes a tombstone and the
/// recipient's `pleroma:chat_mention` telling goes with it.
///
/// Shared by the typed `ChatMessage` `Delete` entry and by the note
/// handler's bare-id and `Tombstone` routes, which find a stored message
/// under the object id before they would reserve a status tombstone for it.
///
/// # Errors
///
/// [`FederationError::Transient`] for a database failure; both writes are
/// idempotent, so the retry completes what it restarts.
pub(crate) async fn withdraw_chat_message(
    state: &AppState,
    message: &chat::ChatMessage,
    signer: &User,
) -> Result<Outcome, FederationError> {
    if message.sender_id != signer.id {
        debug!(
            actor = %signer.ap_id,
            "a chat message Delete by an account that is not its sender; discarding"
        );
        return Ok(Outcome::Ignored(
            "a chat message Delete by an account that is not its sender",
        ));
    }
    let withdrawn = chat::withdraw(&state.db, message.id).await.map_err(|err| {
        FederationError::transient(
            "the chat message fold failed at the message withdrawal",
            &err,
        )
    })?;
    let removed = notification::delete_for_chat_message(&state.db, message.id)
        .await
        .map_err(|err| {
            FederationError::transient("the chat message fold failed at the mention cleanup", &err)
        })?;
    debug!(
        message = %message.id,
        withdrawn, removed, "a remote sender withdrew its chat message"
    );
    Ok(Outcome::Handled)
}

#[cfg(test)]
mod tests {
    mod bare_id_delete;

    use serde_json::json;

    use super::*;
    use crate::util::time::parse_time;

    /// The actor every fixture here is delivered by.
    const BOB: &str = "https://remote.example/users/bob";

    /// The document the fixtures address to a local user named `alice`.
    fn chat_object() -> Value {
        json!({
            "id": "https://remote.example/objects/chat-1",
            "type": "ChatMessage",
            "attributedTo": BOB,
            "to": ["https://test.example/users/alice"],
            "content": "<p>hi alice</p>",
            "published": "2024-06-01T10:00:00.000Z",
        })
    }

    /// `chat_object` with `field` replaced by `value`.
    fn with_field(field: &str, value: Value) -> Value {
        let mut object = chat_object();
        let _previous = object
            .as_object_mut()
            .expect("the fixture is an object")
            .insert(field.to_owned(), value);
        object
    }

    /// The refusal a document earns, panicking when it shaped up fine.
    fn refused(object: &Value) -> &'static str {
        match shape_of(object, "https://remote.example/objects/chat-1", Utc::now()) {
            Err(reason) => reason,
            Ok(_) => panic!("the fixture should have been refused"),
        }
    }

    #[test]
    fn register_claims_chat_message_in_both_sub_registries() {
        let mut handlers = Handlers::new();
        register(&mut handlers);
        assert!(
            handlers.handles_create(CHAT_MESSAGE_TYPE),
            "a Create of a ChatMessage folds here, beside the note family"
        );
        assert!(
            handlers.handles_delete(CHAT_MESSAGE_TYPE),
            "and the sender's Delete of one withdraws here"
        );
        assert!(
            !handlers.handles_create("Note"),
            "the note family stays the create module's own keys"
        );
    }

    #[test]
    fn the_shape_read_folds_the_ticket_document() {
        let incoming = shape_of(
            &chat_object(),
            "https://remote.example/objects/chat-1",
            Utc::now(),
        )
        .expect("the fixture is a deliverable chat message");
        assert_eq!(incoming.ap_id, "https://remote.example/objects/chat-1");
        assert_eq!(
            incoming.recipient_ap_id, "https://test.example/users/alice",
            "the one address of the object's to"
        );
        assert_eq!(incoming.content, "<p>hi alice</p>", "the HTML is kept");
        assert_eq!(
            incoming.raw_text, "hi alice\n",
            "the text strips to its body"
        );
        assert!(incoming.attachment.is_none(), "no key, no attachment");
        assert_eq!(
            incoming.published,
            parse_time("2024-06-01T10:00:00.000Z").expect("the fixture time parses"),
            "and the published instant is the creation time"
        );
    }

    #[test]
    fn an_addressing_that_is_not_exactly_one_recipient_is_refused() {
        for value in [
            json!([]),
            json!([
                "https://test.example/users/alice",
                "https://test.example/users/bob"
            ]),
            Value::Null,
        ] {
            assert!(
                refused(&with_field("to", value.clone())).contains("exactly one account"),
                "the {value} addressing is refused"
            );
        }
    }

    #[test]
    fn a_message_without_a_usable_published_time_is_refused() {
        for value in [Value::Null, json!("not a timestamp"), json!(1_717_236_000)] {
            assert!(
                refused(&with_field("published", value.clone())).contains("published time"),
                "the {value} published is refused"
            );
        }
    }

    #[test]
    fn a_future_published_time_lands_as_now() {
        let future = with_field("published", json!("2999-01-01T00:00:00.000Z"));
        let now = Utc::now();
        let incoming = shape_of(&future, "id", now).expect("the clock-skewed message shapes up");
        assert_eq!(
            incoming.published, now,
            "a peer's forward clock is not trusted"
        );
    }

    #[test]
    fn neither_text_nor_media_is_no_message_and_hostile_text_is_sanitized() {
        assert!(
            refused(&with_field("content", Value::Null)).contains("neither content nor attachment"),
            "no content, no attachment, nothing to deliver"
        );
        let mut empty = chat_object();
        let _cleared = empty
            .as_object_mut()
            .expect("the fixture is an object")
            .remove("content");
        let _replaced = empty
            .as_object_mut()
            .expect("the fixture is an object")
            .insert("attachment".to_owned(), json!([]));
        assert!(
            refused(&empty).contains("neither content nor attachment"),
            "an empty attachment array is no attachment either"
        );
        let hostile = with_field("content", json!("<p>hi</p><script>alert(1)</script>"));
        let incoming = shape_of(&hostile, "id", Utc::now()).expect("the markup message shapes up");
        assert!(!incoming.content.contains("script"), "the script is gone");
        assert!(
            !incoming.content.contains("alert"),
            "and its contents with it"
        );
    }

    #[test]
    fn only_the_first_chat_media_entry_survives_and_link_refuses_the_message() {
        let entries = with_field(
            "attachment",
            json!([
                {
                    "type": "Image",
                    "mediaType": "image/png",
                    "url": "https://remote.example/media/first.png",
                    "name": "the first",
                    "width": 640,
                    "height": -2,
                },
                {"type": "Document", "url": "https://remote.example/media/second.pdf"},
            ]),
        );
        let incoming = shape_of(&entries, "id", Utc::now()).expect("the media message shapes up");
        let attachment = incoming.attachment.expect("the first entry is kept");
        assert_eq!(attachment.url, "https://remote.example/media/first.png");
        assert_eq!(attachment.content_type, "image/png", "as declared");
        assert_eq!(
            attachment.description.as_deref(),
            Some("the first"),
            "with its alt text"
        );
        assert_eq!(attachment.width, Some(640), "and its dimensions");
        assert_eq!(attachment.height, None, "a negative one is no dimension");

        let link = with_field(
            "attachment",
            json!({"type": "Link", "url": "https://remote.example/page"}),
        );
        assert!(
            refused(&link).contains("not usable media"),
            "the note family's Link is nobody's chat attachment"
        );
        let urlless = with_field("attachment", json!({"type": "Image"}));
        assert!(
            refused(&urlless).contains("not usable media"),
            "media with no url is refused the same way"
        );
    }

    #[test]
    fn a_null_attachment_is_no_attachment_at_all() {
        let null = with_field("attachment", Value::Null);
        let incoming = shape_of(&null, "id", Utc::now()).expect("the text carries the message");
        assert!(incoming.attachment.is_none(), "a lone null is absent media");
    }

    #[test]
    fn undeclared_media_types_default_to_the_validators_fallback() {
        let entry = with_field(
            "attachment",
            json!({"type": "Document", "url": "https://remote.example/files/doc"}),
        );
        let incoming = shape_of(&entry, "id", Utc::now()).expect("the document shapes up");
        assert_eq!(
            incoming.attachment.expect("kept").content_type,
            DEFAULT_MEDIA_TYPE,
            "no mediaType, no guess: Pleroma's default"
        );
    }

    #[tokio::test]
    async fn the_recipient_gates_name_local_active_chatters() {
        use crate::{
            config::TEST_BASE_URL,
            models::user::{Flag, NewUser, insert, local_ap_id},
        };
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let chatter = insert(
            &pool,
            &NewUser::local("chatter", &local_ap_id(TEST_BASE_URL, "chatter")),
        )
        .await
        .expect("fixture user inserts");
        assert!(
            recipient_refusal(&chatter).is_none(),
            "a fresh local account accepts chats"
        );

        let mirror = insert(
            &pool,
            &NewUser {
                nickname: "bob@remote.example".to_owned(),
                ap_id: BOB.to_owned(),
                ..NewUser::default()
            },
        )
        .await
        .expect("fixture remote inserts");
        assert_eq!(
            recipient_refusal(&mirror),
            Some("a chat message addressed to a remote account"),
            "a remote mirror keeps its chat rows at home"
        );

        let quiet = insert(
            &pool,
            &NewUser {
                accepts_chat_messages: Flag::new(false),
                ..NewUser::local("quiet", &local_ap_id(TEST_BASE_URL, "quiet"))
            },
        )
        .await
        .expect("fixture user inserts");
        assert_eq!(
            recipient_refusal(&quiet),
            Some("a chat message addressed to an account that does not accept chats"),
            "the flag refuses the delivery"
        );

        let retired = insert(
            &pool,
            &NewUser {
                is_active: Flag::new(false),
                ..NewUser::local("retired", &local_ap_id(TEST_BASE_URL, "retired"))
            },
        )
        .await
        .expect("fixture user inserts");
        assert_eq!(
            recipient_refusal(&retired),
            Some("a chat message addressed to a deactivated account"),
            "and so does deactivation"
        );
    }
}
