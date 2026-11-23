//! The stream names one socket speaks, the frames it is answered with, the
//! scopes the private streams accept, and the rule that expands a public
//! stream name onto the hub topic behind it.
//!
//! The names are Mastodon's (`public`, `public:local`, `hashtag`, `user`,
//! `list`, …); the frames and the 401 body are the wording decision 16 pins;
//! the private-stream scope set is Pleroma's `@user_streams` clause in
//! `get_topic` (`lib/pleroma/web/mastodon_api/websocket_handler.ex`).
//! `Connection::topic_for` is the caller that pairs these rules with the
//! reader-bound streams.

use std::time::Duration;

use crate::{
    models::hashtag,
    oauth::scopes::{READ_NOTIFICATIONS, READ_STATUSES},
};

use super::args::StreamArgs;

/// The heartbeat period of one streaming socket: Pleroma's `@tick`
/// (`lib/pleroma/web/mastodon_api/websocket_handler.ex`).
///
/// It is what [`Config::streaming_ping_interval_secs`] defaults to, and a test
/// keeps the two spans equal.
///
/// [`Config::streaming_ping_interval_secs`]: crate::config::Config::streaming_ping_interval_secs
pub const PING_INTERVAL: Duration = Duration::from_secs(30);
/// How long a connection's `Filters` stay in force before they are read
/// again.
///
/// The ticket's "query once per connection, refreshed every 5 minutes", so a
/// block or a mute lands on a live socket within this window.
pub const FILTER_TTL: Duration = Duration::from_secs(300);
/// How many streams one socket may hold at once (the ticket's limit, which is
/// Mastodon's `MAX_STREAMS_PER_CONNECTION`).
pub const MAX_SUBSCRIPTIONS: usize = 50;
/// 401 body for a presented token that names no live user, and for a private
/// stream on a socket that has no token at all (the ticket's wording).
pub(super) const INVALID_TOKEN: &str = "Invalid access token";
/// The frame a `subscribe` / `unsubscribe` naming a stream this endpoint does
/// not know (or missing the parameter it needs) answers with.
pub(super) const UNKNOWN_STREAM_FRAME: &str =
    r#"{"error":"bad_request","error_description":"unknown stream"}"#;
/// The frame a subscription this socket may not make answers with: the stream
/// is refused and the connection goes on holding what it already has.
pub(super) const UNAUTHORIZED_FRAME: &str = r#"{"error":"unauthorized"}"#;
/// The frame the subscription past [`MAX_SUBSCRIPTIONS`] answers with, refused
/// the same way an unknown stream is.
pub(super) const FULL_FRAME: &str =
    r#"{"error":"bad_request","error_description":"too many subscriptions"}"#;
/// 400 body for a request on the streaming path that is not a WebSocket
/// upgrade: Cowboy's own wording, which is what Pleroma answers here.
pub(super) const NOT_AN_UPGRADE: &str = "Expected a WebSocket upgrade";
/// The query parameter and message field naming the stream to watch.
pub(super) const STREAM_FIELD: &str = "stream";
/// The query parameter a bearer token may arrive in.
pub(super) const ACCESS_TOKEN_FIELD: &str = "access_token";
/// The `subscribe` message's `type`.
pub(super) const SUBSCRIBE: &str = "subscribe";
/// The `unsubscribe` message's `type`.
pub(super) const UNSUBSCRIBE: &str = "unsubscribe";
/// The message that gives an anonymous socket a user (Pleroma's
/// `pleroma:authenticate`).
pub(super) const AUTHENTICATE: &str = "pleroma:authenticate";
/// The public timeline, watched by anyone.
pub(super) const PUBLIC: &str = "public";
/// The local public timeline.
pub(super) const PUBLIC_LOCAL: &str = "public:local";
/// The public timeline with media attached.
pub(super) const PUBLIC_MEDIA: &str = "public:media";
/// The local public timeline with media attached.
pub(super) const PUBLIC_LOCAL_MEDIA: &str = "public:local:media";
/// The public timeline of remote instances, narrowed by `instance`.
pub(super) const PUBLIC_REMOTE: &str = "public:remote";
/// The remote media timeline, narrowed by `instance`.
pub(super) const PUBLIC_REMOTE_MEDIA: &str = "public:remote:media";
/// One hashtag's statuses (`tag` names it).
pub(super) const HASHTAG: &str = "hashtag";
/// One hashtag's local statuses (`tag` names it).
pub(super) const HASHTAG_LOCAL: &str = "hashtag:local";
/// The reader's own timeline; the socket holds it as the hub's
/// `user:<reader uuid>`.
pub(super) const USER: &str = "user";
/// The reader's notifications; the socket holds it as the hub's
/// `user:notification:<reader uuid>`.
pub(super) const USER_NOTIFICATION: &str = "user:notification";
/// The reader's chats (Pleroma's `user:pleroma_chat`); the socket holds it
/// as the hub's `user:pleroma_chat:<reader uuid>`.
pub(super) const USER_PLEROMA_CHAT: &str = "user:pleroma_chat";
/// The reader's direct messages; the socket holds it as the hub's
/// `direct:<reader uuid>`.
pub(super) const DIRECT: &str = "direct";
/// One of the reader's lists (`list` names it).
pub(super) const LIST: &str = "list";

/// The scope one private user stream requires: `read:notifications` for
/// [`USER_NOTIFICATION`], `read:statuses` for every other one (`user`,
/// `user:pleroma_chat`, `direct`), as Pleroma's `Streamer` requires. A plain
/// `read` token covers both through [`Scopes::contains`].
pub(super) fn required_scope(name: &str) -> &'static str {
    if name == USER_NOTIFICATION {
        READ_NOTIFICATIONS
    } else {
        READ_STATUSES
    }
}

/// Whether `name` is one of the private user streams, the streams that need a
/// user and a read scope: Pleroma's `@user_streams` list.
pub(super) fn is_user_stream(name: &str) -> bool {
    matches!(name, USER | USER_NOTIFICATION | USER_PLEROMA_CHAT | DIRECT)
}

/// The hub stream name a public stream expands to, `None` when `name` names
/// no public stream or lacks the parameter it needs. A remote stream is
/// narrowed to `public:remote:<instance>` when the client sent `instance` and
/// stays `public:remote` when it did not (Pleroma requires the parameter; the
/// ticket calls it optional), and a hashtag stream is
/// `hashtag:<normalized tag>`.
pub(super) fn public_topic(name: &str, args: &StreamArgs) -> Option<String> {
    match name {
        PUBLIC | PUBLIC_LOCAL | PUBLIC_MEDIA | PUBLIC_LOCAL_MEDIA => Some(name.to_owned()),
        PUBLIC_REMOTE | PUBLIC_REMOTE_MEDIA => args
            .instance
            .as_deref()
            .map(|host| format!("{name}:{}", host.trim().to_lowercase()))
            .or_else(|| Some(name.to_owned())),
        HASHTAG | HASHTAG_LOCAL => args
            .tag
            .as_deref()
            .and_then(hashtag::normalize_name)
            .map(|tag| format!("{name}:{tag}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::config::DEFAULT_STREAMING_PING_INTERVAL_SECS;

    use super::*;

    #[test]
    fn the_config_default_and_the_heartbeat_constant_agree() {
        assert_eq!(
            PING_INTERVAL.as_secs(),
            u64::try_from(DEFAULT_STREAMING_PING_INTERVAL_SECS).expect("a positive default"),
            "PING_INTERVAL and STREAMING_PING_INTERVAL_SECS' default are one span"
        );
    }

    #[test]
    fn a_public_stream_needs_no_token_and_keeps_its_name() {
        for name in [PUBLIC, PUBLIC_LOCAL, PUBLIC_MEDIA, PUBLIC_LOCAL_MEDIA] {
            assert_eq!(
                public_topic(name, &StreamArgs::default()).as_deref(),
                Some(name),
                "`{name}` is its own stream name"
            );
        }
    }

    #[test]
    fn a_remote_stream_is_narrowed_by_instance_when_one_is_given() {
        assert_eq!(
            public_topic(PUBLIC_REMOTE, &StreamArgs::default()).as_deref(),
            Some("public:remote"),
            "no instance keeps the plain stream"
        );
        let args = StreamArgs {
            instance: Some(" Remote.Example ".to_owned()),
            ..StreamArgs::default()
        };
        assert_eq!(
            public_topic(PUBLIC_REMOTE_MEDIA, &args).as_deref(),
            Some("public:remote:media:remote.example"),
            "the host is trimmed and lowercased, the way a domain is stored"
        );
    }

    #[test]
    fn a_hashtag_stream_needs_a_usable_tag() {
        let args = StreamArgs {
            tag: Some("#Rust".to_owned()),
            ..StreamArgs::default()
        };
        assert_eq!(
            public_topic(HASHTAG, &args).as_deref(),
            Some("hashtag:rust"),
            "the tag is normalized onto the end of the name"
        );
        assert_eq!(
            public_topic(HASHTAG_LOCAL, &args).as_deref(),
            Some("hashtag:local:rust"),
            "and so it is on the local flavour"
        );
        assert_eq!(
            public_topic(HASHTAG, &StreamArgs::default()),
            None,
            "a hashtag with no tag names no stream"
        );
    }

    #[test]
    fn each_private_stream_requires_its_own_scope() {
        assert_eq!(required_scope(USER_NOTIFICATION), READ_NOTIFICATIONS);
        for name in [USER, USER_PLEROMA_CHAT, DIRECT] {
            assert_eq!(
                required_scope(name),
                READ_STATUSES,
                "`{name}` needs read:statuses"
            );
        }
    }

    #[test]
    fn only_the_known_streams_resolve_at_all() {
        assert_eq!(public_topic("nonsense", &StreamArgs::default()), None);
        assert_eq!(
            public_topic(USER, &StreamArgs::default()),
            None,
            "a user stream is never a public topic"
        );
        for name in [USER, USER_NOTIFICATION, USER_PLEROMA_CHAT, DIRECT] {
            assert!(is_user_stream(name), "`{name}` is a private user stream");
        }
        assert!(
            !is_user_stream(PUBLIC) && !is_user_stream(LIST),
            "the public streams and `list` are classified elsewhere"
        );
    }
}
