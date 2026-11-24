//! The conversation half of a socket's send-time render: the entity JSON a
//! [`RenderSpec::Conversation`] hint answers to — the reader's own
//! participation in the direct-message thread, built by the conversations
//! endpoint's entity builder.
//!
//! [`RenderSpec::Conversation`]: crate::streaming::hub::RenderSpec::Conversation

use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    api::{
        conversations,
        streaming::{connection::Connection, filters::entity_json},
    },
    models::conversation,
};

/// The Conversation entity JSON of the hinted participation `id` (the
/// reader's own row of the direct-message thread, the way the
/// conversations endpoints address a conversation), built by
/// [`crate::api::conversations`]' entity builder. `None` — the drop
/// signal — when the socket has no viewer or no participation of this
/// reader carries that id: [`conversation::get_participation`] answers
/// `None` for a foreign id and for no id at all, so another reader's
/// conversation never renders here.
pub(super) async fn render_hinted_conversation(
    connection: &Connection,
    id: Uuid,
) -> Option<String> {
    let Some(viewer) = connection.viewer() else {
        debug!(%id, "a streaming conversation hint reaches a readerless socket");
        return None;
    };
    let row = match conversation::get_participation(&connection.state.db, viewer.id, id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            debug!(%id, "a streaming conversation hint names no participation of the reader's");
            return None;
        }
        Err(err) => {
            warn!(%err, %id, "a streaming conversation hint could not be read");
            return None;
        }
    };
    let rows = [row];
    // Like the endpoint's single-participation reads, a hinted row goes
    // through the page renderer on its own; that builder answers one
    // entity per row it is handed.
    match conversations::conversation_entities(&connection.state, viewer, &rows).await {
        Ok(entities) => entities
            .into_iter()
            .next()
            .and_then(|entity| entity_json(&entity, id)),
        Err(err) => {
            warn!(%err, %id, "a streaming conversation hint could not be rendered");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use crate::{
        api::streaming::{
            names::DIRECT,
            test_support::{FrameFor as _, account, for_reader, frame_payload, state},
        },
        config::TEST_BASE_URL,
        models::{
            conversation,
            status::{self, NewStatus, Visibility},
            user::User,
        },
        state::AppState,
        streaming::hub::{Audience, RenderSpec, StreamEvent},
    };

    /// A conversation hint renders the reader's own participation through
    /// the conversations endpoint's builder: the thread's other
    /// participant as account, the row's unread flag and the newest status
    /// as this viewer sees it.
    #[tokio::test]
    async fn a_conversation_hint_renders_the_conversation_for_its_participant() {
        let (_db, state) = state().await;
        let reader = account(&state, "conversation-hint-reader").await;
        let author = account(&state, "conversation-hint-author").await;
        let post = dm(&state, &author, &[&reader], "only between us").await;
        let own = participation_id(&state, &reader, &post).await;
        let own_stream = format!("{DIRECT}:{}", reader.id);
        let connection = for_reader(&state, &reader, &[&own_stream]).await;

        let frame = connection
            .frame_for(&conversation_hint(&own_stream, reader.id, own))
            .await
            .expect("the participant's socket renders their conversation");

        let entity = frame_payload(&frame);
        assert_eq!(
            entity["id"],
            own.to_string(),
            "the reader's own participation, never the thread's id"
        );
        assert_eq!(
            entity["accounts"][0]["id"],
            author.id.to_string(),
            "the other participant as the viewer sees them"
        );
        assert_eq!(
            entity["unread"], true,
            "the side that has not answered reads unread"
        );
        assert_eq!(
            entity["last_status"]["content"], "<p>only between us</p>",
            "the thread's newest status rendered into the entity"
        );
    }

    /// A conversation hint answers only for its own participant: another
    /// reader's participation id, and an id nobody holds at all, both drop
    /// the event — the same indistinguishable miss the conversations
    /// endpoints answer with a 404.
    #[tokio::test]
    async fn a_conversation_hint_is_dropped_for_a_stranger_and_a_miss() {
        let (_db, state) = state().await;
        let reader = account(&state, "conversation-drop-reader").await;
        let stranger = account(&state, "conversation-drop-stranger").await;
        let author = account(&state, "conversation-drop-author").await;
        let post = dm(&state, &author, &[&reader], "not for you").await;
        let own = participation_id(&state, &reader, &post).await;

        let theirs = format!("{DIRECT}:{}", stranger.id);
        let stranger_connection = for_reader(&state, &stranger, &[&theirs]).await;
        assert!(
            stranger_connection
                .frame_for(&conversation_hint(&theirs, stranger.id, own))
                .await
                .is_none(),
            "one participant's conversation id means nothing to another"
        );
        assert!(
            stranger_connection
                .frame_for(&conversation_hint(&theirs, stranger.id, Uuid::now_v7()))
                .await
                .is_none(),
            "and so does a hint that names no participation at all"
        );
    }

    /// A `conversation` event that carries only the conversation render
    /// hint, for `reader`'s direct stream — the shape a conversation
    /// producer will publish.
    fn conversation_hint(stream: &str, reader: Uuid, id: Uuid) -> StreamEvent {
        StreamEvent {
            streams: vec![stream.to_owned()],
            event: "conversation".to_owned(),
            payload: None,
            audience: Audience::Users(vec![reader]),
            render: Some(RenderSpec::Conversation(id)),
            local_only: false,
        }
    }

    /// `author`'s `direct` status addressed to everyone in `mentioned`,
    /// with the create hook's participation write run over it: the state a
    /// posted DM leaves behind, and the only way a participation row comes
    /// to exist.
    ///
    /// # Panics
    ///
    /// If either fixture write fails.
    async fn dm(
        state: &AppState,
        author: &User,
        mentioned: &[&User],
        text: &str,
    ) -> status::Status {
        let ids: Vec<Uuid> = mentioned.iter().map(|who| who.id).collect();
        let post = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus {
                visibility: Visibility::Direct,
                mentioned_user_ids: ids,
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("the direct fixture inserts");
        conversation::participate_for_status(&state.db, &post)
            .await
            .expect("the participation hook runs");
        post
    }

    /// `reader`'s own participation id in the thread `post` was stored
    /// into — the id a conversation hint names.
    ///
    /// # Panics
    ///
    /// If either fixture read fails, or the hook left `post` unthreaded or
    /// `reader` out of the thread.
    async fn participation_id(state: &AppState, reader: &User, post: &status::Status) -> Uuid {
        let thread = status::get_by_id(&state.db, post.id)
            .await
            .expect("the fixture post reads")
            .expect("the fixture post is live")
            .direct_conversation_id
            .expect("the participation hook threaded the fixture");
        *conversation::participation_ids(&state.db, reader.id, &[thread])
            .await
            .expect("the participations read")
            .get(&thread)
            .expect("the reader is a participant of the fixture thread")
    }
}
