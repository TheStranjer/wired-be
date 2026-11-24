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
    services::{
        filter::{FilterContext, apply_to_entity},
        render_status::{ViewerMode, render_status},
    },
};

/// The Status entity JSON of the hinted status `id`, rendered for this
/// socket's viewer. `None` — the drop signal — when the row is gone or
/// unreadable (a status deleted before this socket caught up reads as
/// gone: [`status::get_by_id`] hides tombstones), when
/// [`status_visibility::can_view`] says no for this viewer or the
/// author's account is deactivated, when the renderer has no view of it
/// at all, or when one of the reader's irreversible phrase filters
/// matches it in `context` ([`apply_to_entity`]). A warn filter's match
/// rides the entity's `filtered` array instead.
///
/// [`status::get_by_id`]: crate::models::status::get_by_id
pub(super) async fn render_hinted_status(
    connection: &Connection,
    id: Uuid,
    context: Option<FilterContext>,
) -> Option<String> {
    let row = hinted_status_row(connection, id).await?;
    if !status_admits_reader(connection, &row, id).await {
        return None;
    }
    let viewer = connection.viewer();
    match render_status(&connection.state, ViewerMode::User(viewer), &row).await {
        Ok(Some(mut entity)) => {
            let dropped = context.is_some_and(|context| {
                !apply_to_entity(
                    &mut entity,
                    &connection.filters.words,
                    context,
                    connection.reader_id(),
                )
            });
            if dropped {
                debug!(%id, "a streamed status matches the reader's irreversible word filters");
                return None;
            }
            entity_json(&entity, id)
        }
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
                FrameFor as _, account, for_reader, frame_payload, hinted_update, inactive_account,
                phrase_filter, state, watching,
            },
        },
        config::TEST_BASE_URL,
        models::{
            follow,
            media_attachment::{self, NewMediaAttachment},
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

    /// The `update` event of status `id` on the single hub stream `stream`.
    fn update_on(stream: &str, id: Uuid) -> StreamEvent {
        StreamEvent {
            streams: vec![stream.to_owned()],
            ..hinted_update(id)
        }
    }

    /// An irreversible filter judges a streamed status in the context of
    /// the stream it arrives on: a `public`-only filter drops it from the
    /// public stream and leaves it on the home (`user`) stream, a `home`
    /// one the reverse, and a filter of a non-streaming context judges
    /// neither.
    #[tokio::test]
    async fn an_irreversible_word_filter_judges_the_stream_context() {
        let (_db, state) = state().await;
        let author = account(&state, "wordfilter-author").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&author, "a plain post about spoilers"),
        )
        .await
        .expect("the status fixture inserts");
        let public_only =
            phrase_filter(&state, "wordfilter-public", "spoilers", true, "public").await;
        let home_only = phrase_filter(&state, "wordfilter-home", "spoilers", true, "home").await;
        let outsider =
            phrase_filter(&state, "wordfilter-outsider", "spoilers", true, "account").await;

        for (reader, dropped_on, kept_on) in [
            (
                &public_only,
                PUBLIC.to_owned(),
                format!("{USER}:{}", public_only.id),
            ),
            (
                &home_only,
                format!("{USER}:{}", home_only.id),
                PUBLIC.to_owned(),
            ),
        ] {
            let connection = for_reader(&state, reader, &[&dropped_on, &kept_on]).await;
            assert!(
                connection
                    .frame_for(&update_on(&dropped_on, row.id))
                    .await
                    .is_none(),
                "the filter names `{dropped_on}`'s context and drops the match"
            );
            assert!(
                connection
                    .frame_for(&update_on(&kept_on, row.id))
                    .await
                    .is_some(),
                "and the same status on `{kept_on}` is outside its context"
            );
        }
        let own_stream = format!("{USER}:{}", outsider.id);
        let standing = for_reader(&state, &outsider, &[PUBLIC, &own_stream]).await;
        for stream in [PUBLIC, own_stream.as_str()] {
            assert!(
                standing
                    .frame_for(&update_on(stream, row.id))
                    .await
                    .is_some(),
                "a filter outside the streaming contexts judges no stream"
            );
        }
    }

    /// The viewer's own post is exempt, as it is on the REST timelines.
    #[tokio::test]
    async fn the_readers_own_matching_post_is_delivered() {
        let (_db, state) = state().await;
        let reader = phrase_filter(&state, "wordfilter-own", "spoilers", true, "home").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&reader, "my own post about spoilers"),
        )
        .await
        .expect("the status fixture inserts");
        let own_stream = format!("{USER}:{}", reader.id);
        let connection = for_reader(&state, &reader, &[&own_stream]).await;

        assert!(
            connection
                .frame_for(&update_on(&own_stream, row.id))
                .await
                .is_some(),
            "the reader's own match stays on their stream"
        );
    }

    /// A phrase is matched against the plain text, not the markup: `p`
    /// is the paragraph tag of the rendered content and no word of it.
    #[tokio::test]
    async fn a_phrase_matching_only_markup_does_not_drop() {
        let (_db, state) = state().await;
        let author = account(&state, "wordfilter-markup-author").await;
        let reader = phrase_filter(&state, "wordfilter-markup", "p", true, "home").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&author, "hello world"),
        )
        .await
        .expect("the status fixture inserts");
        let own_stream = format!("{USER}:{}", reader.id);
        let connection = for_reader(&state, &reader, &[&own_stream]).await;

        let frame = connection
            .frame_for(&update_on(&own_stream, row.id))
            .await
            .expect("the markup is no text to match");
        assert_eq!(
            frame_payload(&frame)["content"],
            "<p>hello world</p>",
            "the entity carries the markup the phrase would have matched"
        );
    }

    /// A warn filter delivers the status with its `filtered` annotation.
    #[tokio::test]
    async fn a_warn_filter_annotates_the_streamed_status() {
        let (_db, state) = state().await;
        let author = account(&state, "wordfilter-warn-author").await;
        let reader = phrase_filter(&state, "wordfilter-warn", "spoilers", false, "home").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&author, "a plain post about spoilers"),
        )
        .await
        .expect("the status fixture inserts");
        let own_stream = format!("{USER}:{}", reader.id);
        let connection = for_reader(&state, &reader, &[&own_stream, PUBLIC]).await;

        let frame = connection
            .frame_for(&update_on(&own_stream, row.id))
            .await
            .expect("a warn filter never drops the event");
        let entity = frame_payload(&frame);
        let filtered = entity["filtered"]
            .as_array()
            .expect("the streamed status carries a filtered array");
        assert_eq!(filtered.len(), 1, "one warn filter matched");
        assert_eq!(
            filtered[0]["keyword_matches"][0], "spoilers",
            "and the result names the matched keyword"
        );

        let public = connection
            .frame_for(&update_on(PUBLIC, row.id))
            .await
            .expect("the public stream delivers it too");
        assert_eq!(
            frame_payload(&public)["filtered"],
            serde_json::json!([]),
            "a home-only warn filter leaves the public stream's status unannotated"
        );
    }

    /// A media description is text the filter reads, on the contexts the
    /// filter names.
    #[tokio::test]
    async fn a_media_description_match_drops_on_the_filtered_stream() {
        let (_db, state) = state().await;
        let author = account(&state, "wordfilter-media-author").await;
        let reader = phrase_filter(&state, "wordfilter-media", "secret", true, "public").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&author, "a picture"),
        )
        .await
        .expect("the status fixture inserts");
        let _attached = media_attachment::insert(
            &state.db,
            &NewMediaAttachment {
                user_id: Some(author.id),
                status_id: Some(row.id),
                url: format!("{TEST_BASE_URL}/media/fixture.png"),
                content_type: "image/png".to_owned(),
                description: Some("a secret garden".to_owned()),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .expect("the attachment fixture inserts");
        let own_stream = format!("{USER}:{}", reader.id);
        let connection = for_reader(&state, &reader, &[PUBLIC, &own_stream]).await;

        assert!(
            connection
                .frame_for(&update_on(PUBLIC, row.id))
                .await
                .is_none(),
            "the description matches in the public context and drops"
        );
        assert!(
            connection
                .frame_for(&update_on(&own_stream, row.id))
                .await
                .is_some(),
            "and the home stream is outside it"
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
