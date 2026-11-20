//! The status half of a socket's send-time render: the entity JSON a
//! [`RenderSpec::Status`] hint answers to for this connection's own viewer,
//! with the two reads that decide whether the hinted row may be shown at all.
//!
//! [`RenderSpec::Status`]: crate::streaming::hub::RenderSpec::Status

use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    api::streaming::{connection::Connection, filters::entity_json},
    models::{status, status_visibility, user},
    services::render_status::{ViewerMode, render_status},
};

/// The Status entity JSON of the hinted status `id`, rendered for this
/// socket's viewer. `None` — the drop signal — when the row is gone or
/// unreadable (a status deleted before this socket caught up reads as
/// gone: [`status::get_by_id`] hides tombstones), when
/// [`status_visibility::can_view`] says no for this viewer or the
/// author's account is deactivated, or when the renderer has no view
/// of it at all.
///
/// [`status::get_by_id`]: crate::models::status::get_by_id
pub(super) async fn render_hinted_status(connection: &Connection, id: Uuid) -> Option<String> {
    let row = hinted_status_row(connection, id).await?;
    if !status_admits_reader(connection, &row, id).await {
        return None;
    }
    let viewer = connection.viewer();
    match render_status(&connection.state, ViewerMode::User(viewer), &row).await {
        Ok(Some(entity)) => entity_json(&entity, id),
        Ok(None) => {
            debug!(%id, "a streaming render hint has no renderable view");
            None
        }
        Err(err) => {
            warn!(%err, %id, "a streaming render hint could not be rendered");
            None
        }
    }
}

/// The live row the status hint names, `None` when it is gone (the
/// delete beat this socket to it) or unreadable (a log line).
async fn hinted_status_row(connection: &Connection, id: Uuid) -> Option<status::Status> {
    match status::get_by_id(&connection.state.db, id).await {
        Ok(Some(row)) => Some(row),
        Ok(None) => {
            debug!(%id, "a streaming render hint names no live status");
            None
        }
        Err(err) => {
            warn!(%err, %id, "a streaming render hint could not be read");
            None
        }
    }
}

/// Whether this socket's viewer may see the hinted status row at all:
/// the shared visibility rules (the anonymous socket gets the
/// anonymous view, a private post reaches its author and the followers
/// `can_view` admits — and nobody else), and an author whose account
/// is deactivated answers nothing to anybody.
async fn status_admits_reader(connection: &Connection, row: &status::Status, id: Uuid) -> bool {
    let viewer = connection.viewer();
    match status_visibility::can_view(&connection.state.db, row, viewer).await {
        Ok(true) => {}
        Ok(false) => {
            debug!(%id, "a streaming render hint is not visible to its reader");
            return false;
        }
        Err(err) => {
            warn!(%err, %id, "a streaming render hint could not be judged");
            return false;
        }
    }
    match user::get_by_id(&connection.state.db, row.user_id).await {
        Ok(Some(author)) => {
            let active = author.is_active.get();
            if !active {
                debug!(%id, "the author of a streaming render hint is deactivated");
            }
            active
        }
        Ok(None) => {
            debug!(%id, "a streaming render hint names a status with no author");
            false
        }
        Err(err) => {
            warn!(%err, %id, "the author of a streaming render hint could not be read");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::Value;

    use crate::{
        api::streaming::{
            names::{PUBLIC, USER},
            test_support::{
                account, for_reader, frame_payload, hinted_update, inactive_account, phrase_filter,
                state, watching,
            },
        },
        config::TEST_BASE_URL,
        models::{
            follow,
            status::{self, NewStatus, Visibility},
        },
        streaming::hub::{Audience, StreamEvent},
    };

    use super::*;

    /// The render hint of a live status arrives at the socket as an
    /// `update` carrying the rendered entity: the hint is the hub's
    /// envelope, the entity the socket's own view of it.
    #[tokio::test]
    async fn a_status_render_hint_arrives_as_the_rendered_entity() {
        let (_db, state) = state().await;
        let alice = account(&state, "hint-render-alice").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&alice, "rendered at the socket"),
        )
        .await
        .expect("the status fixture inserts");
        let connection = watching(&state, &[PUBLIC]).await;

        let frame = connection
            .frame_for(&hinted_update(row.id))
            .await
            .expect("a hinted update is rendered for the socket");

        let envelope: Value = serde_json::from_str(&frame).expect("the frame is JSON");
        assert_eq!(
            envelope["event"], "update",
            "the event name rides the frame as published: {frame}"
        );
        let payload: Value = serde_json::from_str(
            envelope["payload"]
                .as_str()
                .expect("the rendered update carries a payload string"),
        )
        .expect("the payload is the entity JSON");
        assert_eq!(
            payload["id"],
            Value::from(row.id.to_string()),
            "the payload is the hinted status's entity"
        );
        assert_eq!(
            payload["content"],
            Value::from("<p>rendered at the socket</p>"),
            "rendered from the live row"
        );
    }

    /// A render hint that no longer names a renderable status — the row
    /// tombstone between the publish and this socket's read — drops the
    /// frame: Mastodon's `update` is its entity, and an entity-less one is
    /// noise a client cannot use.
    #[tokio::test]
    async fn a_render_hint_whose_status_is_gone_drops_the_frame() {
        let (_db, state) = state().await;
        let alice = account(&state, "hint-gone-alice").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&alice, "doomed before delivery"),
        )
        .await
        .expect("the status fixture inserts");
        let _tombstone = status::soft_delete(&state.db, row.id)
            .await
            .expect("the delete fixture runs");
        let connection = watching(&state, &[PUBLIC]).await;

        assert!(
            connection.frame_for(&hinted_update(row.id)).await.is_none(),
            "the tombstoned hint delivers nothing"
        );
        assert!(
            connection
                .frame_for(&hinted_update(Uuid::now_v7()))
                .await
                .is_none(),
            "and so does a hint that never named a row"
        );
    }

    /// The ticket's core promise, per viewer: a hinted PRIVATE status
    /// renders for a follower of the author, and the same hint dropped on
    /// a stranger's socket — stream name and audience matched for them,
    /// but the send-time [`status_visibility::can_view`] re-judgment says
    /// no. The pre-serialized path could never tell these viewers apart.
    #[tokio::test]
    async fn a_private_status_hint_renders_for_a_follower_and_not_for_a_stranger() {
        let (_db, state) = state().await;
        let author = account(&state, "private-hint-author").await;
        let follower = account(&state, "private-hint-follower").await;
        let stranger = account(&state, "private-hint-stranger").await;
        let _relation = follow::follow(&state.db, TEST_BASE_URL, &follower, &author)
            .await
            .expect("the follow fixture writes");
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus {
                visibility: Visibility::Private,
                ..NewStatus::local(&author, "followers only")
            },
        )
        .await
        .expect("the status fixture inserts");

        let own_stream = format!("{USER}:{}", follower.id);
        let follower_connection = for_reader(&state, &follower, &[&own_stream]).await;
        let frame = follower_connection
            .frame_for(&StreamEvent {
                streams: vec![own_stream.clone()],
                audience: Audience::Users(vec![follower.id]),
                ..hinted_update(row.id)
            })
            .await
            .expect("the hinted private status renders for a follower");
        let entity = frame_payload(&frame);
        assert_eq!(
            entity["id"],
            Value::from(row.id.to_string()),
            "the payload is the hinted status's entity"
        );
        assert_eq!(
            entity["content"],
            Value::from("<p>followers only</p>"),
            "rendered for the socket's own viewer"
        );

        let other_stream = format!("{USER}:{}", stranger.id);
        let stranger_connection = for_reader(&state, &stranger, &[&other_stream]).await;
        assert!(
            stranger_connection
                .frame_for(&StreamEvent {
                    streams: vec![other_stream],
                    audience: Audience::Users(vec![stranger.id]),
                    ..hinted_update(row.id)
                })
                .await
                .is_none(),
            "the same hint on a non-follower's stream drops at the visibility gate"
        );
    }

    /// An active irreversible word filter of a streaming context drops the
    /// rendered event; the same phrase as a warn filter (not irreversible)
    /// or outside the streaming contexts leaves it on the wire.
    #[tokio::test]
    async fn an_irreversible_word_filter_drops_a_rendered_status() {
        let (_db, state) = state().await;
        let author = account(&state, "wordfilter-author").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&author, "a plain post about spoilers"),
        )
        .await
        .expect("the status fixture inserts");
        let warner = phrase_filter(&state, "wordfilter-warner", "spoilers", false, "home").await;
        let hider = phrase_filter(&state, "wordfilter-hider", "spoilers", true, "home").await;
        let outsider =
            phrase_filter(&state, "wordfilter-outsider", "spoilers", true, "account").await;

        let aloud = for_reader(&state, &warner, &[PUBLIC]).await;
        assert!(
            aloud.frame_for(&hinted_update(row.id)).await.is_some(),
            "a warn filter never drops the event; the client covers it"
        );
        let silent = for_reader(&state, &hider, &[PUBLIC]).await;
        assert!(
            silent.frame_for(&hinted_update(row.id)).await.is_none(),
            "the irreversible streaming filter matches the rendered content and drops"
        );
        let standing = for_reader(&state, &outsider, &[PUBLIC]).await;
        assert!(
            standing.frame_for(&hinted_update(row.id)).await.is_some(),
            "a filter outside the streaming contexts judges no stream"
        );
    }

    /// The viewer is the row the socket took at authenticate: the reader's
    /// profile changes afterwards ride past the live socket, which keeps
    /// rendering in the view it loaded. (Suppression sets are a separate
    /// question: they refresh every [`FILTER_TTL`].)
    ///
    /// [`FILTER_TTL`]: crate::api::streaming::FILTER_TTL
    #[tokio::test]
    async fn the_socket_holds_its_viewer_from_the_handshake() {
        let (_db, state) = state().await;
        let reader = account(&state, "frozen-viewer").await;
        let author = account(&state, "frozen-viewer-author").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&author, "a public post"),
        )
        .await
        .expect("the status fixture inserts");
        let mut connection = watching(&state, &[PUBLIC]).await;
        connection.reader = Some(Arc::clone(&reader));

        let _renamed = sqlx::query("UPDATE users SET name = 'Renamed Later' WHERE id = $1")
            .bind(reader.id)
            .execute(&state.db)
            .await
            .expect("the rename fixture runs");

        let frame = connection
            .frame_for(&hinted_update(row.id))
            .await
            .expect("the public hint still renders for the viewer");
        let entity = frame_payload(&frame);
        assert_eq!(
            entity["content"], "<p>a public post</p>",
            "the hint rendered through the socket's held viewer"
        );
        let held = connection
            .viewer()
            .expect("the socket holds its viewer from the handshake");
        assert!(
            held.name.is_empty(),
            "the rename never reached the connection: it renders from the row it loaded, \
             not a live re-read per event"
        );
    }

    /// A hint whose status author has been deactivated since the publish
    /// answers nothing to anybody.
    #[tokio::test]
    async fn a_status_hint_of_a_deactivated_author_drops() {
        let (_db, state) = state().await;
        let ghost = inactive_account(&state, "deactivated-hint-author").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&ghost, "heard from the grave"),
        )
        .await
        .expect("the status fixture inserts");
        let connection = watching(&state, &[PUBLIC]).await;

        assert!(
            connection.frame_for(&hinted_update(row.id)).await.is_none(),
            "the send-time render drops the deactivated author's status"
        );
    }
}
