//! The side effects that follow a chat write: who is told, and how.
//!
//! A created or deleted message tells the chat's LOCAL participants over
//! their streaming sockets and a REMOTE counterpart over the wire
//! ([`federation_out`]'s chat arms); a read tells the reader alone. The
//! route handlers end with these hooks.

use tracing::warn;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{chat, chat::ChatMessageReference, user::User},
    services::{federation_out, streaming},
    state::AppState,
};

/// The side effect Pleroma attaches to a created chat message: the telling
/// of the chat's LOCAL sides over their chat sockets — each side's own
/// `pleroma:chat_update` — and of a REMOTE counterpart over the wire:
/// Pleroma's `post_chat_message` runs the message through
/// `Pipeline.common_pipeline`, and the `Create` activity that comes
/// out the other end is what carries the message to the recipient's inbox.
///
/// `message` is one side's reference to the stored message (the sender's on
/// the local route, the local recipient's for a delivered remote message),
/// `sender` the account that wrote it and `recipient` the account it is
/// addressed to, local or remote. Which side `message` belongs to never
/// matters: [`publish_chat_update`] finds each LOCAL participant's own chat
/// row. A local recipient is told by their rows alone — chat messages to
/// local accounts never travel over HTTP — and everything else, including
/// the remote pair's gates, is
/// [`federation_out::after_chat_message_created`]'s.
///
/// [`federation_out::after_chat_message_created`]: crate::services::federation_out::after_chat_message_created
pub(crate) async fn after_chat_message_created(
    state: &AppState,
    message: &ChatMessageReference,
    sender: &User,
    recipient: &User,
) {
    publish_chat_update(state, message, sender, recipient).await;
    federation_out::after_chat_message_created(state, message, sender, recipient).await;
}

/// The [`after_chat_message_created`] counterpart for a deleted chat
/// message: the chat's LOCAL sides hear it on their chat sockets, and
/// Pleroma's `delete_message` ends with an `Undo` of the message's
/// `Create` for federation when the sender withdrew it; this instance
/// federates the withdrawal as a `Delete` of the message's reserved object
/// id (decision 6's tombstone convention, [`crate::federation::note`]'s
/// status `Delete` beside it).
///
/// `user` is the caller who deleted, `other` the chat's counterpart and
/// `message` the caller's reference as it stood before the delete — whose
/// chat id is therefore the deleter's own chat row, which outlives the
/// reference. Only the sender's delete announces anything over HTTP — a
/// participant who merely took the message out of their own list withdraws
/// nobody's copy — and a local counterpart never hears over HTTP; the gates
/// are [`federation_out::after_chat_message_deleted`]'s.
///
/// [`federation_out::after_chat_message_deleted`]: crate::services::federation_out::after_chat_message_deleted
pub(super) async fn after_chat_message_deleted(
    state: &AppState,
    message: &ChatMessageReference,
    user: &User,
    other: &User,
) {
    publish_chat_update(state, message, user, other).await;
    federation_out::after_chat_message_deleted(state, message, user, other).await;
}

/// Tell every LOCAL side of the pair [`after_chat_message_created`] and
/// [`after_chat_message_deleted`] name: one `pleroma:chat_update` per
/// participant, each riding that participant's own chat stream and carrying
/// their OWN chat row id, which is what the socket's render hint resolves
/// through [`chat::get_owned`] (see [`streaming::chat_updated`]).
///
/// `message` is one side's reference to the message and names that side's
/// chat row; the other side's row is found through their own reference to
/// the same message, which [`publish_chat_update`] reaches even after the
/// delete — chat rows outlive references, and a withdrawal leaves both
/// sides' references standing. A REMOTE side is told nothing: their copy of
/// the conversation lives on their own instance, and the wire announcement
/// is the [`federation_out`] arm's work, not this one's.
///
/// Best-effort like every streaming tail: a failed read of a participant's
/// row is a log line and no event for that side, never an error back to the
/// request that already committed its write.
///
/// [`federation_out`]: crate::services::federation_out
/// [`streaming::chat_updated`]: crate::services::streaming::chat_updated
async fn publish_chat_update(
    state: &AppState,
    message: &ChatMessageReference,
    sender: &User,
    recipient: &User,
) {
    for side in [sender, recipient] {
        match local_chat_id(state, side, message).await {
            Ok(Some(chat_id)) => streaming::chat_updated(state, side.id, chat_id),
            Ok(None) => {}
            Err(err) => warn!(
                chat = %message.chat_id,
                message = %message.message_id,
                user = %side.id,
                %err,
                "chat stream event skipped: reading the participant's own chat row failed"
            ),
        }
    }
}

/// `side`'s OWN chat row for the message `message` references, or `None`
/// when `side` holds no chat row here — a remote account, whose side of the
/// conversation lives on their own instance, or a pair whose row was never
/// written for them. `message`'s chat row answers first, since one call
/// site's `message` already belongs to this side; their own reference to
/// the same message answers otherwise, which is how the second side of the
/// pair is found. The question the socket's hint is gated by is the same
/// one [`chat::get_owned`] answers for the reader.
///
/// Any database failure arrives as [`AppError::Database`].
async fn local_chat_id(
    state: &AppState,
    side: &User,
    message: &ChatMessageReference,
) -> AppResult<Option<Uuid>> {
    if !side.local.get() {
        return Ok(None);
    }
    if let Some(chat_row) = chat::get_owned(&state.db, side.id, message.chat_id).await? {
        return Ok(Some(chat_row.id));
    }
    Ok(
        chat::message_reference_for_user(&state.db, side.id, message.message_id)
            .await?
            .map(|reference| reference.chat_id),
    )
}

/// The side effect Pleroma attaches to a successful read: Pleroma's
/// `mark_as_read` and `mark_message_as_read` (2.0-era
/// `chat_controller.ex`) end with
/// `Pleroma.Web.ChatSocket.ChatSocket.broadcast_chat(user, :chat_read, id)`
/// after storing the marker — a push to every chat socket the reading user
/// has connected, so their other clients clear the badge too. This instance
/// serves that transport over the Mastodon hub ([`crate::api::streaming`]),
/// where the push is Pleroma's `pleroma:chat_update` on the reader's own
/// `user:pleroma_chat` stream carrying the reader's own chat row: both read
/// handlers call this hook after the marker is stored (`chat_id` always,
/// and `last_read_id` as the chat-wide marker or the single reference
/// marked, neither of which the rendered chat entity shows). The reader is
/// the only side that read anything, so the reader is the only side told —
/// and the emission is synchronous, like [`streaming::chat_updated`].
///
/// [`streaming::chat_updated`]: crate::services::streaming::chat_updated
pub(super) fn after_chat_read(
    state: &AppState,
    user: &User,
    chat_id: Uuid,
    _last_read_id: Option<Uuid>,
) {
    streaming::chat_updated(state, user.id, chat_id);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::broadcast;

    use super::*;
    use crate::{
        api::chats::test_support::{app_state, chatter, message_pair, remote_chatter},
        streaming::hub::{Audience, RenderSpec, StreamEvent},
    };

    /// The next event the hook under test handed to `rx`, which the test
    /// subscribed to before making the call: the hub delivers every publish
    /// to its local subscribers at once, so one awaited hook call is
    /// already buffered here by the time this reads.
    async fn emitted(rx: &mut broadcast::Receiver<Arc<StreamEvent>>) -> StreamEvent {
        Arc::unwrap_or_clone(rx.recv().await.expect("the hook published an event"))
    }

    /// Asserts `rx` holds no further event: the hook told nobody else.
    fn assert_empty(rx: &mut broadcast::Receiver<Arc<StreamEvent>>) {
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "the hook published an event it owed nobody"
        );
    }

    /// Asserts one published `pleroma:chat_update` names `owner`'s own
    /// stream and their own chat `chat_id` as the render hint.
    fn assert_chat_update(event: &StreamEvent, owner: &User, chat_id: Uuid) {
        assert_eq!(event.event, "pleroma:chat_update", "Pleroma's chat event");
        assert_eq!(
            event.streams,
            vec![format!("user:pleroma_chat:{}", owner.id)],
            "the event rides the owner's own chat stream"
        );
        assert_eq!(
            event.audience,
            Audience::Users(vec![owner.id]),
            "the owner is the whole audience"
        );
        assert_eq!(
            event.render,
            Some(RenderSpec::Chat(chat_id)),
            "the hint carries the owner's own chat row"
        );
        assert!(event.payload.is_none(), "no body: the socket renders it");
        assert!(!event.local_only, "a chat update is not flagged local-only");
    }

    #[tokio::test]
    async fn a_created_local_message_tells_both_local_sides_their_own_chat() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = chatter(&pool, "alice").await;
        let bob = chatter(&pool, "bob").await;
        let (alice_reference, bob_reference) = message_pair(&pool, &alice, &bob).await;
        let bob_chat = bob_reference
            .expect("a local recipient gets a chat row from the write")
            .chat_id;
        let mut rx = state.hub.subscribe();

        after_chat_message_created(&state, &alice_reference, &alice, &bob).await;

        assert_chat_update(&emitted(&mut rx).await, &alice, alice_reference.chat_id);
        assert_chat_update(&emitted(&mut rx).await, &bob, bob_chat);
        assert_ne!(
            alice_reference.chat_id, bob_chat,
            "the two sides carry two different chat rows"
        );
        assert_empty(&mut rx);
    }

    #[tokio::test]
    async fn a_created_message_to_a_remote_counterpart_tells_only_the_local_side() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = chatter(&pool, "alice").await;
        let carol = remote_chatter(&pool, "carol").await;
        let (alice_reference, carol_reference) = message_pair(&pool, &alice, &carol).await;
        assert!(
            carol_reference.is_none(),
            "the model stores no reference for a remote recipient"
        );
        let mut rx = state.hub.subscribe();

        after_chat_message_created(&state, &alice_reference, &alice, &carol).await;

        assert_chat_update(&emitted(&mut rx).await, &alice, alice_reference.chat_id);
        assert_empty(&mut rx);
    }

    #[tokio::test]
    async fn a_delivered_remote_message_tells_only_the_local_recipient_their_own_chat() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = chatter(&pool, "bob").await;
        let carol = remote_chatter(&pool, "carol").await;
        // The delivery shape: the remote account writes, the local one
        // receives, and the hook runs over the RECIPIENT's own reference —
        // src/federation/handlers/chat.rs `deliver`.
        let (_, delivered) = message_pair(&pool, &carol, &bob).await;
        let bob_reference = delivered.expect("the local recipient's reference was written");
        let mut rx = state.hub.subscribe();

        after_chat_message_created(&state, &bob_reference, &carol, &bob).await;

        assert_chat_update(&emitted(&mut rx).await, &bob, bob_reference.chat_id);
        assert_empty(&mut rx);
    }

    #[tokio::test]
    async fn a_deleted_message_tells_both_local_sides_their_own_chat() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = chatter(&pool, "alice").await;
        let bob = chatter(&pool, "bob").await;
        let (alice_reference, bob_reference) = message_pair(&pool, &alice, &bob).await;
        let bob_chat = bob_reference
            .expect("the local side has a reference")
            .chat_id;
        assert!(
            chat::delete_message_for(&pool, alice.id, alice_reference.id)
                .await
                .expect("the delete fixture writes"),
            "alice sent the message, so her delete withdraws it"
        );
        let mut rx = state.hub.subscribe();

        after_chat_message_deleted(&state, &alice_reference, &alice, &bob).await;

        // The deleter's reference is gone but her chat row stands, and the
        // withdrawal leaves bob's reference standing: both sides are told.
        assert_chat_update(&emitted(&mut rx).await, &alice, alice_reference.chat_id);
        assert_chat_update(&emitted(&mut rx).await, &bob, bob_chat);
        assert_empty(&mut rx);
    }

    #[tokio::test]
    async fn a_read_tells_only_the_reader_their_own_chat() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = chatter(&pool, "alice").await;
        let bob = chatter(&pool, "bob").await;
        let chat_row = chat::get_or_create(&pool, &alice, &bob)
            .await
            .expect("the chat fixture creates");
        let mut rx = state.hub.subscribe();

        after_chat_read(&state, &alice, chat_row.id, None);

        assert_chat_update(&emitted(&mut rx).await, &alice, chat_row.id);
        assert_empty(&mut rx);
    }
}
