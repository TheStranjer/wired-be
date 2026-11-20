//! The chat half of a socket's send-time render: the entity JSON a
//! [`RenderSpec::Chat`] hint answers to — the reader's own row of the chat,
//! built by the chats endpoint's entity builder.
//!
//! [`RenderSpec::Chat`]: crate::streaming::hub::RenderSpec::Chat

use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    api::{
        chats,
        streaming::{connection::Connection, filters::entity_json},
    },
    models::{chat, user},
};

/// The Chat entity JSON of the hinted chat `id` (the reader's own row
/// of it, the way the chats endpoints address a chat), built by
/// [`crate::api::chats`]' entity builder. `None` — the drop signal —
/// when the socket has no viewer, no chat of this reader's carries
/// that id, or the other participant is gone or deactivated.
pub(super) async fn render_hinted_chat(connection: &Connection, id: Uuid) -> Option<String> {
    let Some(reader) = connection.reader_id() else {
        debug!(%id, "a streaming chat hint reaches a readerless socket");
        return None;
    };
    let row = match chat::get_owned(&connection.state.db, reader, id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            debug!(%id, "a streaming chat hint names no chat of the reader's");
            return None;
        }
        Err(err) => {
            warn!(%err, %id, "a streaming chat hint could not be read");
            return None;
        }
    };
    match user::get_by_id(&connection.state.db, row.recipient_id).await {
        Ok(Some(recipient)) if recipient.is_active.get() => {}
        Ok(Some(_)) => {
            debug!(%id, "the other participant of a streaming chat hint is deactivated");
            return None;
        }
        Ok(None) => {
            debug!(%id, "a streaming chat hint names a chat with no recipient");
            return None;
        }
        Err(err) => {
            warn!(%err, %id, "the other participant of a streaming chat hint could not be read");
            return None;
        }
    }
    match chats::chat_entity(&connection.state, &row).await {
        Ok(entity) => entity_json(&entity, id),
        Err(err) => {
            warn!(%err, %id, "a streaming chat hint could not be rendered");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use crate::{
        api::streaming::{
            names::USER_PLEROMA_CHAT,
            test_support::{account, for_reader, frame_payload, state},
        },
        models::chat,
        streaming::hub::{Audience, RenderSpec, StreamEvent},
    };

    /// A chat hint renders the reader's own chat through the chats
    /// endpoint's builder: the peer as account, the live unread count and
    /// the newest message.
    #[tokio::test]
    async fn a_chat_hint_renders_the_chat_for_its_participant() {
        let (_db, state) = state().await;
        let reader = account(&state, "chat-hint-reader").await;
        let sender = account(&state, "chat-hint-sender").await;
        let chat_row = chat::get_or_create(&state.db, &reader, &sender)
            .await
            .expect("the chat fixture opens");
        let mut tx = state
            .db
            .begin()
            .await
            .expect("the message fixture takes a transaction");
        let _sent = chat::add_message(
            &mut tx,
            &state.config.base_url,
            &sender,
            &reader,
            &chat::NewChatMessage::text("hello in here"),
        )
        .await
        .expect("the message fixture sends");
        tx.commit().await.expect("the message fixture commits");
        let own_stream = format!("{USER_PLEROMA_CHAT}:{}", reader.id);
        let connection = for_reader(&state, &reader, &[&own_stream]).await;

        let frame = connection
            .frame_for(&chat_hint(&own_stream, reader.id, chat_row.id))
            .await
            .expect("the participant's socket renders their chat");

        let entity = frame_payload(&frame);
        assert_eq!(
            entity["id"],
            chat_row.id.to_string(),
            "the reader's own chat row"
        );
        assert_eq!(
            entity["account"]["id"],
            sender.id.to_string(),
            "the peer as the chat's account"
        );
        assert_eq!(entity["unread"], 1, "the unread count read at send time");
        assert_eq!(
            entity["last_message"]["content"], "<p>hello in here</p>",
            "the newest message rendered into the entity"
        );
    }

    /// A chat hint answers only for its participant, and only while the
    /// peer is a live account: the other side's chat id, no id at all and
    /// a deactivated peer all drop the event.
    #[tokio::test]
    async fn a_chat_hint_is_dropped_for_strangers_and_a_deactivated_peer() {
        let (_db, state) = state().await;
        let reader = account(&state, "chat-drop-reader").await;
        let stranger = account(&state, "chat-drop-stranger").await;
        let peer = account(&state, "chat-drop-peer").await;
        let chat_row = chat::get_or_create(&state.db, &reader, &peer)
            .await
            .expect("the chat fixture opens");

        let theirs = format!("{USER_PLEROMA_CHAT}:{}", stranger.id);
        let stranger_connection = for_reader(&state, &stranger, &[&theirs]).await;
        assert!(
            stranger_connection
                .frame_for(&chat_hint(&theirs, stranger.id, chat_row.id))
                .await
                .is_none(),
            "one participant's chat id means nothing to another"
        );
        assert!(
            stranger_connection
                .frame_for(&chat_hint(&theirs, stranger.id, Uuid::now_v7()))
                .await
                .is_none(),
            "and so does a hint that names no chat at all"
        );

        let _deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(peer.id)
            .execute(&state.db)
            .await
            .expect("the deactivation fixture runs");
        let own_stream = format!("{USER_PLEROMA_CHAT}:{}", reader.id);
        let reader_connection = for_reader(&state, &reader, &[&own_stream]).await;
        assert!(
            reader_connection
                .frame_for(&chat_hint(&own_stream, reader.id, chat_row.id))
                .await
                .is_none(),
            "a deactivated peer answers nothing to the participant's socket"
        );
    }

    /// A `pleroma:chat_update` event that carries only the chat render
    /// hint, for `reader`'s chat stream.
    fn chat_hint(stream: &str, reader: Uuid, id: Uuid) -> StreamEvent {
        StreamEvent {
            streams: vec![stream.to_owned()],
            event: "pleroma:chat_update".to_owned(),
            payload: None,
            audience: Audience::Users(vec![reader]),
            render: Some(RenderSpec::Chat(id)),
            local_only: false,
        }
    }
}
