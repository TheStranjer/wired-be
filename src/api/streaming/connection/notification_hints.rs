//! The notification half of a socket's send-time render: the entity JSON a
//! [`RenderSpec::Notification`] hint answers to, built the way the
//! notifications endpoint builds one and for this connection's reader only.
//!
//! [`RenderSpec::Notification`]: crate::streaming::hub::RenderSpec::Notification

use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    api::{
        notifications,
        streaming::{connection::Connection, filters::entity_json},
    },
    models::notification,
};

/// The Notification entity JSON of the hinted notification `id`, built
/// the way [`crate::api::notifications`] renders one and for this
/// socket's reader only: `None` — the drop signal — when the socket
/// has no viewer, the row is not a live notification addressed to
/// this reader (a foreign, unknown or dismissed row, or one whose
/// actor is deactivated, blocked or muted, all of which
/// [`notification::get_visible`] answers `None` for), the actor left
/// no user row, or any read or render fails.
pub(super) async fn render_hinted_notification(
    connection: &Connection,
    id: Uuid,
) -> Option<String> {
    let Some(viewer) = connection.viewer() else {
        debug!(%id, "a streaming notification hint reaches a readerless socket");
        return None;
    };
    let row = match notification::get_visible(&connection.state.db, viewer.id, id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            debug!(%id, "a streaming notification hint is no live notification of the reader's");
            return None;
        }
        Err(err) => {
            warn!(%err, %id, "a streaming notification hint could not be read");
            return None;
        }
    };
    let rows = [row];
    // The render applies the reader's phrase filters in the
    // `notifications` context to the named status, as the endpoint's
    // listing does: a warn match annotates it, an irreversible match
    // takes the notification out of the answer.
    let entities = match notifications::render_notifications(
        &connection.state,
        viewer,
        &rows,
        &connection.filters.words,
    )
    .await
    {
        Ok(entities) => entities,
        Err(err) => {
            warn!(%err, %id, "a streaming notification hint could not be rendered");
            return None;
        }
    };
    let Some(entity) = entities.into_iter().next() else {
        debug!(%id, "a streaming notification hint has no renderable actor or is filtered");
        return None;
    };
    entity_json(&entity, id)
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use crate::{
        api::streaming::{
            names::USER_NOTIFICATION,
            test_support::{account, for_reader, frame_payload, phrase_filter, state},
        },
        config::TEST_BASE_URL,
        models::{
            notification::{self, NewNotification},
            status::{self, NewStatus},
        },
        streaming::hub::{Audience, RenderSpec, StreamEvent},
    };

    /// The word filter judges a rendered notification in the
    /// `notifications` context: the text of its own status falls to an
    /// irreversible filter of that context, a `home` one is not asked, and
    /// a warn filter annotates the embedded status instead.
    #[tokio::test]
    async fn the_notifications_context_filters_judge_the_embedded_status() {
        let (_db, state) = state().await;
        let actor = account(&state, "wordfilter-note-context-actor").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&actor, "a post full of spoilers"),
        )
        .await
        .expect("the status fixture inserts");
        let home_only =
            phrase_filter(&state, "wordfilter-note-home", "spoilers", true, "home").await;
        let warner = phrase_filter(
            &state,
            "wordfilter-note-warner",
            "spoilers",
            false,
            "notifications",
        )
        .await;

        for (reader, annotated) in [(&home_only, false), (&warner, true)] {
            let note = notification::create(
                &state.db,
                &NewNotification {
                    user_id: reader.id,
                    from_account_id: actor.id,
                    notification_type: "favourite",
                    status_id: Some(row.id),
                    emoji: None,
                    report_id: None,
                    chat_message_id: None,
                },
            )
            .await
            .expect("the notification fixture writes");
            let own_stream = format!("{USER_NOTIFICATION}:{}", reader.id);
            let connection = for_reader(&state, reader, &[&own_stream]).await;

            let frame = connection
                .frame_for(&notification_hint(&own_stream, reader.id, note.id))
                .await
                .expect("neither filter drops the notification");
            let entity = frame_payload(&frame);
            let filtered = &entity["status"]["filtered"];
            assert_eq!(
                filtered
                    .as_array()
                    .is_some_and(|results| !results.is_empty()),
                annotated,
                "only the notifications-context warn filter annotates: {filtered}"
            );
        }
    }

    /// The word filter judges every rendered kind the same way: the text
    /// of the notification's own status falls, and its recipient can never
    /// be the socket's anonymous nobody.
    #[tokio::test]
    async fn an_irreversible_word_filter_drops_a_rendered_notification() {
        let (_db, state) = state().await;
        let actor = account(&state, "wordfilter-note-actor").await;
        let recipient = phrase_filter(
            &state,
            "wordfilter-note-recipient",
            "spoilers",
            true,
            "notifications",
        )
        .await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&actor, "a post full of spoilers"),
        )
        .await
        .expect("the status fixture inserts");
        let note = notification::create(
            &state.db,
            &NewNotification {
                user_id: recipient.id,
                from_account_id: actor.id,
                notification_type: "favourite",
                status_id: Some(row.id),
                emoji: None,
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the notification fixture writes");
        let own_stream = format!("{USER_NOTIFICATION}:{}", recipient.id);
        let connection = for_reader(&state, &recipient, &[&own_stream]).await;

        assert!(
            connection
                .frame_for(&notification_hint(&own_stream, recipient.id, note.id))
                .await
                .is_none(),
            "the notification's embedded status text matches the reader's hidden phrase"
        );
    }

    /// A notification hint arrives at its recipient's socket as the full
    /// notification entity — actor expanded, the named status rendered,
    /// the mute flag the endpoint's own relationship read answers — the
    /// same shape `GET /api/v1/notifications` gives.
    #[tokio::test]
    async fn a_notification_hint_renders_the_entity_for_its_recipient() {
        let (_db, state) = state().await;
        let actor = account(&state, "notification-hint-actor").await;
        let recipient = account(&state, "notification-hint-recipient").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&actor, "the liked post"),
        )
        .await
        .expect("the status fixture inserts");
        let note = notification::create(
            &state.db,
            &NewNotification {
                user_id: recipient.id,
                from_account_id: actor.id,
                notification_type: "favourite",
                status_id: Some(row.id),
                emoji: None,
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the notification fixture writes");
        let own_stream = format!("{USER_NOTIFICATION}:{}", recipient.id);
        let connection = for_reader(&state, &recipient, &[&own_stream]).await;

        let frame = connection
            .frame_for(&notification_hint(&own_stream, recipient.id, note.id))
            .await
            .expect("the recipient's socket renders the notification");

        let entity = frame_payload(&frame);
        assert_eq!(entity["id"], note.id.to_string(), "the notification's id");
        assert_eq!(entity["type"], "favourite", "and its stored type");
        assert_eq!(
            entity["account"]["id"],
            actor.id.to_string(),
            "the actor expanded into an account"
        );
        assert_eq!(
            entity["status"]["id"],
            row.id.to_string(),
            "the named status rendered into the entity"
        );
        assert_eq!(
            entity["pleroma"]["is_muted"], false,
            "the recipient does not mute the actor"
        );
    }

    /// A notification hint is the recipient's alone: another reader's
    /// socket and the recipient's own socket after a dismissal both lose
    /// the event at the `get_visible` gate.
    #[tokio::test]
    async fn a_notification_hint_is_dropped_for_everybody_but_its_recipient() {
        let (_db, state) = state().await;
        let actor = account(&state, "notification-drop-actor").await;
        let recipient = account(&state, "notification-drop-recipient").await;
        let stranger = account(&state, "notification-drop-stranger").await;
        let note = notification::create(
            &state.db,
            &NewNotification {
                user_id: recipient.id,
                from_account_id: actor.id,
                notification_type: "follow",
                status_id: None,
                emoji: None,
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the notification fixture writes");

        let theirs = format!("{USER_NOTIFICATION}:{}", stranger.id);
        let stranger_connection = for_reader(&state, &stranger, &[&theirs]).await;
        assert!(
            stranger_connection
                .frame_for(&notification_hint(&theirs, stranger.id, note.id))
                .await
                .is_none(),
            "a notification addressed to somebody else renders on no other socket"
        );

        let own_stream = format!("{USER_NOTIFICATION}:{}", recipient.id);
        let _dismissed = notification::dismiss(&state.db, recipient.id, note.id)
            .await
            .expect("the dismissal fixture runs");
        let recipient_connection = for_reader(&state, &recipient, &[&own_stream]).await;
        assert!(
            recipient_connection
                .frame_for(&notification_hint(&own_stream, recipient.id, note.id))
                .await
                .is_none(),
            "and the recipient loses the row once it is dismissed"
        );
    }

    /// A `notification` event that carries only the notification render
    /// hint, for `reader`'s stream — the shape a notification producer
    /// will publish.
    fn notification_hint(stream: &str, reader: Uuid, id: Uuid) -> StreamEvent {
        StreamEvent {
            streams: vec![stream.to_owned()],
            event: "notification".to_owned(),
            payload: None,
            audience: Audience::Users(vec![reader]),
            render: Some(RenderSpec::Notification(id)),
            local_only: false,
        }
    }
}
