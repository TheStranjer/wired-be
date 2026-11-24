//! The event a streaming connection receives: [`StreamEvent`], who may see it
//! ([`Audience`]), how its entity is rendered per viewer ([`RenderSpec`]) and
//! the stream-name filter [`stream_matches`].

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Who may receive a [`StreamEvent`] once it reaches a connection.
///
/// The hub delivers every event to every subscriber; filtering by audience
/// is the connection's job, exactly as it is the socket process's job in
/// Pleroma's `Streamer`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    /// Anyone may see it: the public and hashtag timelines.
    Public,
    /// Only these local users may see it: direct messages, notifications,
    /// user streams, chat messages.
    Users(Vec<Uuid>),
    /// Every authenticated local user may see it: instance-wide events
    /// such as announcements.
    Everyone,
}

/// Which entity a [`StreamEvent`] points at by id, so the body is rendered
/// at the delivery end for each reader rather than serialized once into a
/// payload that rides to every listener of the stream.
///
/// Pleroma's streamer sends this (its `render` push field) in place of the
/// entity, and its `Streamer` renders in the socket process per user; the
/// status emitters of [`crate::services::streaming`] publish
/// [`RenderSpec::Status`] with an empty payload and
/// [`crate::api::streaming`] renders each hinted entity — status,
/// notification, chat or conversation — for its own socket's reader at send
/// time. The conversation producer's slice still has to land; the framing
/// answers its hints in the meantime.
///
/// The variants serialize to `{"status": "<uuid>"}`,
/// `{"notification": "<uuid>"}`, `{"chat": "<uuid>"}` and
/// `{"conversation": "<uuid>"}`: `snake_case`
/// tags, like [`Audience`]'s wire forms, so the value rides the JSON
/// envelope unchanged.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderSpec {
    /// The event points at the status with this id.
    Status(Uuid),
    /// The event points at the notification with this id.
    Notification(Uuid),
    /// The event points at the chat with this id — each reader's own row
    /// of the conversation, which is what the socket renders.
    Chat(Uuid),
    /// The event points at the conversation participation with this id —
    /// the reader's own row of the direct-message thread, which is what
    /// the socket renders (`GET /api/v1/conversations` answers with this
    /// same entity).
    Conversation(Uuid),
}

/// One event on the streaming bus, in the shape a Mastodon streaming
/// client receives it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamEvent {
    /// The stream names this event belongs to (`"public"`,
    /// `"public:local"`, `"user:<uuid>"`, `"hashtag:rust"`, `"list:<id>"`,
    /// `"direct:<uuid>"`, …); a connection subscribed to any of them
    /// receives it — see [`stream_matches`].
    pub streams: Vec<String>,
    /// The Mastodon event name: `update`, `delete`, `notification`,
    /// `status.update`, `conversation`, `filters_changed`, `announcement`,
    /// `pleroma:chat_update`, …
    pub event: String,
    /// The entity, already serialized as a JSON string: Mastodon sends
    /// the entity as a STRING inside the envelope, not as nested JSON,
    /// and clients `JSON.parse` it. `None` for events that carry no body
    /// (`filters_changed` and friends).
    pub payload: Option<String>,
    /// Who the event is for; see [`Audience`].
    pub audience: Audience,
    /// The event's render hint: the entity to fetch by id when the body
    /// does not ride the socket (see [`RenderSpec`]). `None` for ordinary
    /// entity pushes; an event may carry `render`, `payload`, or neither.
    pub render: Option<RenderSpec>,
    /// Whether the event concerns this instance's local users and entities
    /// only (Pleroma's `local_only` push flag). The streaming WebSocket
    /// drops such an event on a socket with no authenticated reader.
    pub local_only: bool,
}

/// Whether a connection subscribed to the streams in `subscribed` should
/// receive `event`: true when any of the event's target streams is one
/// the connection holds.
///
/// Audience is deliberately not consulted here — the connection checks
/// that against its own authenticated user; this helper answers the
/// stream-name question only.
#[must_use]
pub fn stream_matches(subscribed: &[String], event: &StreamEvent) -> bool {
    event
        .streams
        .iter()
        .any(|stream| subscribed.contains(stream))
}

/// The subscriptions of `subscribed` that `event` belongs to, each once and
/// in the order the connection opened them: the names a connection writes
/// one frame for (Mastodon sends one per matched channel).
#[must_use]
pub fn matched_streams<'subscribed>(
    subscribed: &'subscribed [String],
    event: &StreamEvent,
) -> Vec<&'subscribed str> {
    let mut matched: Vec<&str> = Vec::new();
    for name in subscribed {
        if event.streams.contains(name) && !matched.contains(&name.as_str()) {
            matched.push(name.as_str());
        }
    }
    matched
}

/// The `stream` array a client reads for the internal hub stream `internal`:
/// Mastodon's public names, never the per-reader uuid the hub routes by.
///
/// `user:<uuid>` is `["user"]`, `user:notification:<uuid>` is
/// `["user:notification"]`, `user:pleroma_chat:<uuid>` is
/// `["user:pleroma_chat"]`, `direct:<uuid>` is `["direct"]`,
/// `hashtag:<tag>` is `["hashtag", "<tag>"]`, `hashtag:local:<tag>` is
/// `["hashtag:local", "<tag>"]`, `list:<id>` is `["list", "<id>"]` and
/// `public:remote[:media]:<host>` is `["public:remote[:media]", "<host>"]`.
/// Every other name (`public`, `public:local`, `public:media`, …) is its own
/// public name.
#[must_use]
pub fn public_stream_name(internal: &str) -> Vec<String> {
    for family in ["user:notification", "user:pleroma_chat", "user", "direct"] {
        let owned = internal
            .strip_prefix(family)
            .and_then(|rest| rest.strip_prefix(':'))
            .is_some_and(|rest| Uuid::parse_str(rest).is_ok());
        if owned {
            return vec![family.to_owned()];
        }
    }
    if internal == "public:remote:media" {
        // The unqualified media stream is its own name, not `public:remote` + "media".
        return vec![internal.to_owned()];
    }
    for family in [
        "hashtag:local",
        "hashtag",
        "list",
        "public:remote:media",
        "public:remote",
    ] {
        let qualifier = internal
            .strip_prefix(family)
            .and_then(|rest| rest.strip_prefix(':'))
            .filter(|rest| !rest.is_empty());
        if let Some(qualifier) = qualifier {
            return vec![family.to_owned(), qualifier.to_owned()];
        }
    }
    vec![internal.to_owned()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::hub::test_support::event_on;

    #[test]
    fn public_stream_name_hides_the_readers_uuid() {
        let id = Uuid::now_v7();
        for (internal, public) in [
            (format!("user:{id}"), vec!["user"]),
            (format!("user:notification:{id}"), vec!["user:notification"]),
            (format!("user:pleroma_chat:{id}"), vec!["user:pleroma_chat"]),
            (format!("direct:{id}"), vec!["direct"]),
        ] {
            assert_eq!(public_stream_name(&internal), public, "{internal}");
        }
    }

    #[test]
    fn public_stream_name_splits_the_qualified_families() {
        let id = Uuid::now_v7().to_string();
        for (internal, public) in [
            ("hashtag:rust".to_owned(), vec!["hashtag", "rust"]),
            (
                "hashtag:local:rust".to_owned(),
                vec!["hashtag:local", "rust"],
            ),
            ("hashtag:local".to_owned(), vec!["hashtag", "local"]),
            (format!("list:{id}"), vec!["list", id.as_str()]),
            (
                "public:remote:remote.example".to_owned(),
                vec!["public:remote", "remote.example"],
            ),
            (
                "public:remote:media:remote.example".to_owned(),
                vec!["public:remote:media", "remote.example"],
            ),
        ] {
            assert_eq!(public_stream_name(&internal), public, "{internal}");
        }
    }

    #[test]
    fn public_stream_name_keeps_the_names_that_are_already_public() {
        for name in [
            "public",
            "public:local",
            "public:media",
            "public:local:media",
            "public:remote",
            "public:remote:media",
            "user",
            "user:notification",
            "direct",
            "hashtag",
        ] {
            assert_eq!(public_stream_name(name), vec![name], "{name}");
        }
        assert_eq!(
            public_stream_name("user:not-a-uuid"),
            vec!["user:not-a-uuid"],
            "a suffix that is no uuid is not a reader's"
        );
    }

    #[test]
    fn matched_streams_lists_each_held_stream_of_the_event_once() {
        let subscribed = vec![
            "user:a".to_owned(),
            "public".to_owned(),
            "hashtag:rust".to_owned(),
            "public".to_owned(),
        ];

        assert_eq!(
            matched_streams(&subscribed, &event_on(&["public", "user:a", "list:1"])),
            vec!["user:a", "public"],
            "the connection's order, each name once, nothing it did not hold"
        );
        assert!(
            matched_streams(&subscribed, &event_on(&["direct"])).is_empty(),
            "no shared stream matches nothing"
        );
    }

    #[test]
    fn stream_matches_requires_one_shared_stream() {
        let subscribed = vec!["public".to_owned(), "hashtag:rust".to_owned()];

        assert!(
            stream_matches(&subscribed, &event_on(&["public"])),
            "an event on a held stream matches"
        );
        assert!(
            stream_matches(&subscribed, &event_on(&["direct", "hashtag:rust"])),
            "one held name among many matches"
        );
        assert!(
            !stream_matches(&subscribed, &event_on(&["user"])),
            "an event on other streams does not"
        );
        assert!(
            !stream_matches(&subscribed, &event_on(&[])),
            "an event on no stream matches nothing"
        );
        assert!(
            !stream_matches(&[], &event_on(&["public"])),
            "an unsubscribed connection receives nothing"
        );
    }

    #[test]
    fn audience_serializes_to_its_wire_forms() {
        assert_eq!(
            serde_json::to_string(&Audience::Public).unwrap(),
            "\"public\"",
            "public is a bare tag"
        );
        assert_eq!(
            serde_json::to_string(&Audience::Everyone).unwrap(),
            "\"everyone\"",
            "everyone is a bare tag"
        );
        let id = Uuid::now_v7();
        let users = serde_json::to_string(&Audience::Users(vec![id])).unwrap();
        assert_eq!(
            users,
            format!("{{\"users\":[\"{id}\"]}}"),
            "users carries the allowed ids"
        );
        let back: Audience = serde_json::from_str(&users).unwrap();
        assert_eq!(back, Audience::Users(vec![id]), "and reads back");
    }

    #[test]
    fn render_spec_serializes_to_its_wire_forms() {
        let id = Uuid::now_v7();
        for (spec, name) in [
            (RenderSpec::Status(id), "status"),
            (RenderSpec::Notification(id), "notification"),
            (RenderSpec::Chat(id), "chat"),
            (RenderSpec::Conversation(id), "conversation"),
        ] {
            let text = serde_json::to_string(&spec).unwrap();
            assert_eq!(
                text,
                format!("{{\"{name}\":\"{id}\"}}"),
                "{name} is a snake_case tag over the uuid"
            );
            let back: RenderSpec = serde_json::from_str(&text).unwrap();
            assert_eq!(back, spec, "and reads back");
        }
    }
}
