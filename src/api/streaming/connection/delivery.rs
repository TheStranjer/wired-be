//! The gate one hub event passes before this connection writes it: the
//! audience and `local_only` checks, the payload the event arrives with (its
//! own body, or the entity a render hint answers to for this socket's
//! viewer), and the filter context its phrase filters judge it in.
//!
//! [`Connection::frames_for`] is the entry point that calls these; the
//! per-kind renders are the four `*_hints` children's.

use crate::{
    api::streaming::{
        connection::{
            Connection, chat_hints::render_hinted_chat,
            conversation_hints::render_hinted_conversation,
            notification_hints::render_hinted_notification, status_hints::render_hinted_status,
        },
        names::{DIRECT, HASHTAG, LIST, PUBLIC, USER, USER_NOTIFICATION, USER_PLEROMA_CHAT},
    },
    services::filter::FilterContext,
    streaming::hub::{Audience, RenderSpec, StreamEvent},
};

/// The payload to write for `event`: the event's own body when it
/// carries one (the pre-serialized path, unchanged), the hinted entity
/// rendered for this socket's viewer when it carries a [`RenderSpec`]
/// hint without a body, and no body for an event that carries neither.
/// The outer `None` is the drop signal that [`frames_for`] reads: a
/// hint the socket cannot answer for its reader has no entity to show,
/// and Mastodon's `update` *is* its entity — silence beats an
/// entity-less frame.
///
/// [`frames_for`]: Connection::frames_for
pub(super) async fn payload_for(
    connection: &Connection,
    event: &StreamEvent,
) -> Option<Option<String>> {
    if event.payload.is_some() {
        return Some(event.payload.clone());
    }
    let Some(render) = event.render.as_ref() else {
        return Some(None);
    };
    let context = filter_context(connection, event);
    render_hint(connection, render, context).await.map(Some)
}

/// The [`FilterContext`] this socket's reader judges `event` in.
///
/// It is the context of the streams the event reached this socket on:
/// `user`, `list` and `direct` streams are [`FilterContext::Home`], the
/// `user:notification` stream is [`FilterContext::Notifications`], the
/// `public*` and `hashtag*` streams are [`FilterContext::Public`]. A socket
/// holding several of them judges in that order of precedence; a chat
/// stream, or an event on none of them, has no context and no phrase
/// filter.
fn filter_context(connection: &Connection, event: &StreamEvent) -> Option<FilterContext> {
    [
        FilterContext::Home,
        FilterContext::Notifications,
        FilterContext::Public,
    ]
    .into_iter()
    .find(|context| {
        event
            .streams
            .iter()
            .filter(|stream| connection.subscriptions.contains(stream))
            .any(|stream| stream_context(stream) == Some(*context))
    })
}

/// The filter context of one hub stream name, `None` for the streams no
/// timeline filter governs.
///
/// The names are `user:<id>`, `public:local`, `hashtag:rust` and the like.
fn stream_context(stream: &str) -> Option<FilterContext> {
    if stream.starts_with(USER_NOTIFICATION) {
        Some(FilterContext::Notifications)
    } else if stream.starts_with(USER_PLEROMA_CHAT) {
        None
    } else if [USER, LIST, DIRECT]
        .iter()
        .any(|name| stream.starts_with(name))
    {
        Some(FilterContext::Home)
    } else if [PUBLIC, HASHTAG]
        .iter()
        .any(|name| stream.starts_with(name))
    {
        Some(FilterContext::Public)
    } else {
        None
    }
}

/// The entity JSON a [`RenderSpec`] hint answers to for this socket's
/// viewer.
///
/// Each kind is built by its own endpoint's render path, so one hub event
/// becomes every reader's own view. A status is judged by the
/// reader's phrase filters in `context` and a notification in the
/// notifications context, both inside their own render. `None` — the drop
/// signal — when this socket cannot be shown the entity at all.
async fn render_hint(
    connection: &Connection,
    render: &RenderSpec,
    context: Option<FilterContext>,
) -> Option<String> {
    match render {
        RenderSpec::Status(id) => render_hinted_status(connection, *id, context).await,
        RenderSpec::Notification(id) => render_hinted_notification(connection, *id).await,
        RenderSpec::Chat(id) => render_hinted_chat(connection, *id).await,
        RenderSpec::Conversation(id) => render_hinted_conversation(connection, *id).await,
    }
}

/// Whether the event's audience covers this connection's reader: a public
/// event reaches every socket, an `Everyone` event only an authenticated
/// one, and a `Users` event only the users it names.
pub(super) fn audience_allows(connection: &Connection, event: &StreamEvent) -> bool {
    match &event.audience {
        Audience::Public => true,
        Audience::Everyone => connection.reader.is_some(),
        Audience::Users(ids) => connection
            .reader_id()
            .is_some_and(|reader| ids.contains(&reader)),
    }
}

/// Whether the event's `local_only` flag lets this connection see it at
/// all: the flag marks an event about this instance's local users and
/// entities, and a socket with no authenticated reader is not one of the
/// local viewers it may reach — whatever its audience or stream names say.
pub(super) const fn local_allows(connection: &Connection, event: &StreamEvent) -> bool {
    !event.local_only || connection.reader.is_some()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use uuid::Uuid;

    use crate::{
        api::streaming::{
            filters::Filters,
            names::{DIRECT, PUBLIC, USER, USER_NOTIFICATION},
            test_support::{
                FrameFor as _, account, event, for_reader, hinted_update, mute_author, state,
                status_payload, watching,
            },
        },
        config::TEST_BASE_URL,
        models::status::{self, NewStatus},
        streaming::hub::{Audience, RenderSpec, StreamEvent},
    };

    use super::*;

    #[test]
    fn a_stream_maps_to_the_filter_context_it_is_judged_in() {
        let id = Uuid::now_v7();
        for (stream, expected) in [
            (format!("{USER}:{id}"), Some(FilterContext::Home)),
            (format!("{DIRECT}:{id}"), Some(FilterContext::Home)),
            (format!("{LIST}:{id}"), Some(FilterContext::Home)),
            (
                format!("{USER_NOTIFICATION}:{id}"),
                Some(FilterContext::Notifications),
            ),
            (PUBLIC.to_owned(), Some(FilterContext::Public)),
            ("public:local:media".to_owned(), Some(FilterContext::Public)),
            ("hashtag:rust".to_owned(), Some(FilterContext::Public)),
            (format!("{USER_PLEROMA_CHAT}:{id}"), None),
            ("nonsense".to_owned(), None),
        ] {
            assert_eq!(
                stream_context(&stream),
                expected,
                "`{stream}` is judged in its own context"
            );
        }
    }

    #[tokio::test]
    async fn an_event_is_judged_in_the_context_of_the_streams_the_socket_holds() {
        let (_db, state) = state().await;
        let connection = watching(&state, &["hashtag:rust", "user:one"]).await;

        assert_eq!(
            filter_context(&connection, &event(&["public", "hashtag:rust"], "{}")),
            Some(FilterContext::Public),
            "only the held stream counts, not the whole event's list"
        );
        assert_eq!(
            filter_context(&connection, &event(&["hashtag:rust", "user:one"], "{}")),
            Some(FilterContext::Home),
            "a socket holding a home stream and a public one judges at home"
        );
        assert_eq!(
            filter_context(&connection, &event(&["public"], "{}")),
            None,
            "an event on no held stream has no context"
        );
    }

    #[tokio::test]
    async fn a_public_event_is_rendered_the_mastodon_way() {
        let (_db, state) = state().await;
        let connection = watching(&state, &[PUBLIC]).await;

        let frame = connection
            .frame_for(&event(&["public"], r#"{"id":"7"}"#))
            .await
            .expect("a matching public event is rendered");

        assert_eq!(
            frame, r#"{"stream":["public"],"event":"update","payload":"{\"id\":\"7\"}"}"#,
            "the payload stays a JSON string inside the envelope, as Mastodon sends it"
        );
    }

    #[tokio::test]
    async fn a_delete_event_carries_the_status_id_as_its_payload() {
        let (_db, state) = state().await;
        let connection = watching(&state, &[PUBLIC]).await;
        let id = Uuid::now_v7().to_string();
        let deleted = StreamEvent {
            event: "delete".to_owned(),
            payload: Some(id.clone()),
            ..event(&["public"], "")
        };

        let frame = connection
            .frame_for(&deleted)
            .await
            .expect("a delete on a held stream is rendered");

        assert_eq!(
            frame,
            format!(r#"{{"stream":["public"],"event":"delete","payload":"{id}"}}"#),
            "its payload is the deleted status's id, as a plain string"
        );
    }

    #[tokio::test]
    async fn an_event_on_another_stream_is_not_rendered() {
        let (_db, state) = state().await;
        let connection = watching(&state, &["hashtag:rust"]).await;

        assert!(
            connection
                .frame_for(&event(&["public"], "{}"))
                .await
                .is_none(),
            "a public event does not reach a hashtag subscription"
        );
        assert!(
            connection
                .frame_for(&event(&["hashtag", "hashtag:rust"], "{}"))
                .await
                .is_some(),
            "the hashtag's own event does, on any of its streams"
        );
    }

    #[tokio::test]
    async fn a_body_less_event_carries_no_payload_key() {
        let (_db, state) = state().await;
        let connection = watching(&state, &[PUBLIC]).await;
        let filters_changed = StreamEvent {
            event: "filters_changed".to_owned(),
            payload: None,
            ..event(&["public"], "")
        };

        let frame = connection
            .frame_for(&filters_changed)
            .await
            .expect("an event with no body is still delivered");

        assert_eq!(
            frame, r#"{"stream":["public"],"event":"filters_changed"}"#,
            "and Mastodon's envelope simply omits the payload it has none of"
        );
    }

    #[tokio::test]
    async fn the_owner_socket_forwards_a_bare_filters_changed_frame() {
        let (_db, state) = state().await;
        let reader = account(&state, "filters-changed-reader").await;
        let stranger = account(&state, "filters-changed-stranger").await;
        let own_stream = format!("{USER}:{}", reader.id);
        let other_stream = format!("{USER}:{}", stranger.id);
        let owner = for_reader(&state, &reader, &[own_stream.as_str()]).await;
        let holding_stranger = for_reader(&state, &stranger, &[own_stream.as_str()]).await;
        let anonymous = watching(&state, &[own_stream.as_str()]).await;
        // The event `services::streaming::filters_changed` publishes.
        let filters_changed = StreamEvent {
            streams: vec![own_stream.clone()],
            event: "filters_changed".to_owned(),
            payload: None,
            audience: Audience::Users(vec![reader.id]),
            render: None,
            local_only: false,
        };

        assert_eq!(
            owner.frame_for(&filters_changed).await,
            Some(r#"{"stream":["user"],"event":"filters_changed"}"#.to_owned()),
            "the owner's socket writes the bare envelope under the public name, no payload key"
        );
        assert!(
            holding_stranger.frame_for(&filters_changed).await.is_none(),
            "a reader holding the stream name but outside the audience never sees it: {other_stream}"
        );
        assert!(
            anonymous.frame_for(&filters_changed).await.is_none(),
            "and neither does an anonymous socket"
        );
    }

    /// The streams of a frame, read back from its JSON text.
    fn streams_of(frame: &str) -> Vec<String> {
        let envelope: serde_json::Value = serde_json::from_str(frame).expect("the frame is JSON");
        serde_json::from_value(envelope["stream"].clone()).expect("the stream is a string array")
    }

    #[tokio::test]
    async fn a_follower_gets_one_public_named_frame_per_matched_subscription() {
        let (_db, state) = state().await;
        let author = account(&state, "frames-author").await;
        let follower = account(&state, "frames-follower").await;
        let other_follower = account(&state, "frames-other-follower").await;
        let own_stream = format!("{USER}:{}", follower.id);
        let socket = for_reader(&state, &follower, &[own_stream.as_str(), PUBLIC]).await;
        let internal = [
            PUBLIC.to_owned(),
            format!("{USER}:{}", author.id),
            own_stream.clone(),
            format!("{USER}:{}", other_follower.id),
        ];
        let post = StreamEvent {
            streams: internal.to_vec(),
            audience: Audience::Public,
            ..event(&[], r#"{"id":"1"}"#)
        };

        let frames = socket.frames_for(&post).await;

        let streams: Vec<Vec<String>> = frames.iter().map(|frame| streams_of(frame)).collect();
        assert_eq!(
            streams,
            vec![vec!["user".to_owned()], vec!["public".to_owned()]],
            "one frame for `user`, one for `public`, in the order the socket subscribed"
        );
        let ids = [author.id, follower.id, other_follower.id].map(|id| id.to_string());
        assert!(
            frames
                .iter()
                .all(|frame| ids.iter().all(|id| !frame.contains(id.as_str()))),
            "no frame carries a uuid: {frames:?}"
        );
    }

    #[tokio::test]
    async fn a_frame_never_names_a_stream_the_socket_did_not_subscribe_to() {
        let (_db, state) = state().await;
        let socket = watching(&state, &[PUBLIC]).await;

        let frames = socket
            .frames_for(&event(&["public", "public:local", "hashtag:rust"], "{}"))
            .await;

        let streams: Vec<Vec<String>> = frames.iter().map(|frame| streams_of(frame)).collect();
        assert_eq!(
            streams,
            vec![vec!["public".to_owned()]],
            "one subscription is one frame, and only the subscribed name rides it"
        );
    }

    #[tokio::test]
    async fn hashtag_and_list_frames_carry_the_family_and_its_parameter() {
        let (_db, state) = state().await;
        let reader = account(&state, "frames-list-reader").await;
        let list_id = Uuid::now_v7();
        let list_stream = format!("list:{list_id}");
        let socket = for_reader(
            &state,
            &reader,
            &["hashtag:rust", "hashtag:local:rust", list_stream.as_str()],
        )
        .await;

        let frames = socket
            .frames_for(&event(
                &["hashtag:rust", "hashtag:local:rust", list_stream.as_str()],
                "{}",
            ))
            .await;

        let streams: Vec<Vec<String>> = frames.iter().map(|frame| streams_of(frame)).collect();
        assert_eq!(
            streams,
            vec![
                vec!["hashtag".to_owned(), "rust".to_owned()],
                vec!["hashtag:local".to_owned(), "rust".to_owned()],
                vec!["list".to_owned(), list_id.to_string()],
            ],
            "Mastodon's `[family, parameter]` shape"
        );
    }

    #[tokio::test]
    async fn audience_decides_who_sees_an_event() {
        let (_db, state) = state().await;
        let reader = account(&state, "stream-reader").await;
        let other = Uuid::now_v7();
        let own_stream = format!("{USER}:{}", reader.id);
        let other_stream = format!("{USER}:{other}");
        let mut mine = watching(&state, &[own_stream.as_str()]).await;
        mine.reader = Some(Arc::clone(&reader));
        let on = event(&[own_stream.as_str()], "{}");

        let for_me = StreamEvent {
            audience: Audience::Users(vec![reader.id]),
            ..on.clone()
        };
        let for_other = StreamEvent {
            audience: Audience::Users(vec![other]),
            ..on.clone()
        };
        let everyone = StreamEvent {
            audience: Audience::Everyone,
            ..on.clone()
        };
        let on_other_stream = event(&[other_stream.as_str()], "{}");

        assert!(
            mine.frame_for(&for_me).await.is_some(),
            "its own user sees it"
        );
        assert!(
            mine.frame_for(&for_other).await.is_none(),
            "the audience stays the second gate even on a held stream name"
        );
        assert!(
            mine.frame_for(&everyone).await.is_some(),
            "an Everyone event reaches an authenticated socket"
        );
        assert!(
            mine.frame_for(&on_other_stream).await.is_none(),
            "another reader's uuid-named stream never matches"
        );

        let anonymous = watching(&state, &[own_stream.as_str()]).await;
        assert!(
            anonymous.frame_for(&for_me).await.is_none(),
            "an anonymous socket never receives a Users event"
        );
        assert!(
            anonymous.frame_for(&everyone).await.is_none(),
            "nor an Everyone one"
        );
        assert!(
            anonymous.frame_for(&on).await.is_some(),
            "but a Public event is public"
        );
    }

    #[tokio::test]
    async fn a_local_only_event_never_reaches_an_anonymous_socket() {
        let (_db, state) = state().await;
        let reader = account(&state, "local-gate-reader").await;
        let anonymous = watching(&state, &[PUBLIC]).await;
        let mut member = watching(&state, &[PUBLIC]).await;
        member.reader = Some(Arc::clone(&reader));
        let local_public = StreamEvent {
            local_only: true,
            ..event(&[PUBLIC], "{}")
        };

        assert!(
            anonymous.frame_for(&local_public).await.is_none(),
            "a local_only event is dropped for a readerless socket, whatever its audience"
        );
        let local_everyone = StreamEvent {
            audience: Audience::Everyone,
            ..local_public.clone()
        };
        assert!(
            anonymous.frame_for(&local_everyone).await.is_none(),
            "Everyone plus local_only means authenticated local viewers only"
        );
        assert!(
            member.frame_for(&local_everyone).await.is_some(),
            "and an authenticated socket is exactly such a viewer"
        );
        let not_local = StreamEvent {
            local_only: false,
            ..local_public
        };
        assert!(
            anonymous.frame_for(&not_local).await.is_some(),
            "without the flag the same event streams on as before"
        );
    }

    #[tokio::test]
    async fn a_reader_who_muted_the_author_never_sees_the_status() {
        let (_db, state) = state().await;
        let reader = account(&state, "suppressing-reader").await;
        let author = account(&state, "suppressed-author").await;
        let thread = Uuid::now_v7();
        let body = status_payload(author.id, "quiet", thread);
        mute_author(&state, &reader, &author).await;
        let mut connection = Connection::new(state.clone(), None)
            .await
            .expect("the connection loads");
        connection.reader = Some(Arc::clone(&reader));
        connection.filters = Filters::load(&state.db, Some(reader.id))
            .await
            .expect("the filters load");
        connection.subscriptions = vec![PUBLIC.to_owned()];

        assert!(
            connection
                .frame_for(&event(&["public"], &body))
                .await
                .is_none(),
            "a muted author is suppressed, on a public stream too"
        );
        assert!(
            connection
                .frame_for(&event(
                    &["public"],
                    &status_payload(Uuid::now_v7(), "loud", thread)
                ))
                .await
                .is_some(),
            "and somebody else's status still arrives"
        );
    }

    #[tokio::test]
    async fn a_render_hint_that_cannot_be_answered_drops_the_event() {
        let (_db, state) = state().await;
        let connection = watching(&state, &[USER_NOTIFICATION, DIRECT]).await;
        let hinted = StreamEvent {
            streams: vec![USER_NOTIFICATION.to_owned()],
            event: "notification".to_owned(),
            payload: None,
            audience: Audience::Public,
            render: Some(RenderSpec::Notification(Uuid::now_v7())),
            local_only: false,
        };

        assert!(
            connection.frame_for(&hinted).await.is_none(),
            "a hint no viewer can be answered for is dropped, not pushed body-less"
        );
        let hinted_chat = StreamEvent {
            render: Some(RenderSpec::Chat(Uuid::now_v7())),
            ..hinted
        };
        assert!(
            connection.frame_for(&hinted_chat).await.is_none(),
            "and the same for a chat hint on a readerless socket"
        );
        let hinted_conversation = StreamEvent {
            streams: vec![DIRECT.to_owned()],
            event: "conversation".to_owned(),
            render: Some(RenderSpec::Conversation(Uuid::now_v7())),
            ..hinted_chat
        };
        assert!(
            connection.frame_for(&hinted_conversation).await.is_none(),
            "and the same for a conversation hint, which no participation of nobody's answers"
        );
    }

    /// The reader's filters judge the rendered payload: an update hinted
    /// from a muted account never reaches the muter's socket.
    #[tokio::test]
    async fn a_rendered_update_is_judged_by_the_readers_filters() {
        let (_db, state) = state().await;
        let reader = account(&state, "hint-filter-reader").await;
        let author = account(&state, "hint-filter-author").await;
        let row = status::insert(
            &state.db,
            TEST_BASE_URL,
            &NewStatus::local(&author, "from a muted account"),
        )
        .await
        .expect("the status fixture inserts");
        mute_author(&state, &reader, &author).await;
        let mut connection = watching(&state, &[PUBLIC]).await;
        connection.reader = Some(Arc::clone(&reader));
        connection.filters = Filters::load(&state.db, Some(reader.id))
            .await
            .expect("the filters load");

        assert!(
            connection.frame_for(&hinted_update(row.id)).await.is_none(),
            "the rendered payload carries the authorship the filters read"
        );
    }
}
