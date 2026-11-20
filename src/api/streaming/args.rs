//! The shapes one streaming socket reads and writes: the two refusals a
//! stream can meet, the parameters that qualify a stream name, the client's
//! message and the frame written out.
//!
//! [`Refusal`] is the one place the ticket's two error bodies and their two
//! HTTP answers are spelled: a message-level refusal is a frame, an
//! upgrade-level one is the 400 or the 401.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::AppError;

use super::names::{INVALID_TOKEN, UNAUTHORIZED_FRAME, UNKNOWN_STREAM_FRAME};

/// Why one requested stream cannot be watched: the two refusals Pleroma's
/// `get_topic` answers (`:bad_topic` and `:unauthorized`) and the two frames
/// the ticket pins for them.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    /// No stream of that name, or a parameter it needs is missing or unusable
    /// (Pleroma's `:bad_topic`).
    Unknown,
    /// The stream exists and this socket may not watch it (Pleroma's
    /// `:unauthorized`).
    Unauthorized,
}

impl Refusal {
    /// The text frame a message-level refusal is answered with.
    pub(super) const fn frame(self) -> &'static str {
        match self {
            Self::Unknown => UNKNOWN_STREAM_FRAME,
            Self::Unauthorized => UNAUTHORIZED_FRAME,
        }
    }

    /// The HTTP error an upgrade-time refusal is answered with: an upgrade is
    /// no frame, so the same two refusals become the 400 and the 401.
    pub(super) fn into_error(self) -> AppError {
        match self {
            Self::Unknown => AppError::BadRequest(UNKNOWN_STREAM_FRAME.to_owned()),
            Self::Unauthorized => AppError::Unauthorized(INVALID_TOKEN.to_owned()),
        }
    }
}

/// The parameters that qualify a stream name — `tag`, `list` and `instance` —
/// read from the handshake query string or from a client message: the same
/// three keys in both places, as they are in the one `params` map Pleroma's
/// socket handler is handed.
#[derive(Debug, Default)]
pub(super) struct StreamArgs {
    /// The hashtag of a `hashtag` or `hashtag:local` stream.
    pub(super) tag: Option<String>,
    /// The list id of a `list` stream.
    pub(super) list: Option<String>,
    /// The host of a `public:remote` or `public:remote:media` stream.
    pub(super) instance: Option<String>,
}

impl StreamArgs {
    /// The qualifying parameters of the handshake query string; an empty
    /// value is no value at all.
    pub(super) fn from_query(params: &HashMap<String, String>) -> Self {
        let take = |key: &str| params.get(key).filter(|value| !value.is_empty()).cloned();
        Self {
            tag: take("tag"),
            list: take("list"),
            instance: take("instance"),
        }
    }

    /// The qualifying parameters of one client message.
    fn of(message: &ClientMessage) -> Self {
        Self {
            tag: message.tag.clone(),
            list: message.list.clone(),
            instance: message.instance.clone(),
        }
    }
}

/// One text frame the client sent: a `subscribe` / `unsubscribe` of a stream,
/// or a `pleroma:authenticate` with a token. Keys the message does not need
/// are ignored, and so is a frame that is none of these — Pleroma logs those
/// and keeps the connection.
#[derive(Debug, Deserialize)]
pub(super) struct ClientMessage {
    /// The message kind: `subscribe`, `unsubscribe` or `pleroma:authenticate`.
    #[serde(rename = "type")]
    pub(super) kind: String,
    /// The stream the message names.
    #[serde(default)]
    pub(super) stream: Option<String>,
    /// The `tag` of a hashtag stream.
    #[serde(default)]
    pub(super) tag: Option<String>,
    /// The `list` of a list stream.
    #[serde(default)]
    pub(super) list: Option<String>,
    /// The `instance` of a remote stream.
    #[serde(default)]
    pub(super) instance: Option<String>,
    /// The token of a `pleroma:authenticate` message.
    #[serde(default)]
    pub(super) token: Option<String>,
}

impl ClientMessage {
    /// The qualifying parameters this message carries.
    pub(super) fn args(&self) -> StreamArgs {
        StreamArgs::of(self)
    }
}

/// One frame written to the socket, in the shape Mastodon's streaming clients
/// read: the streams the event belongs to, the event name, and the entity as a
/// JSON *string* the client parses again.
#[derive(Debug, Serialize)]
pub(super) struct Frame<'event> {
    /// The event's own stream names, as published (Mastodon's `stream` array).
    pub(super) stream: &'event [String],
    /// `update`, `delete`, `notification`, `status.update`, …
    pub(super) event: &'event str,
    /// The entity as its JSON text; a `delete` event carries the deleted
    /// status's id here. Omitted for the events that carry no body at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) payload: Option<&'event str>,
}

#[cfg(test)]
mod tests {
    use axum::{http::StatusCode, response::IntoResponse as _};

    use super::*;

    #[test]
    fn a_refusal_is_the_ticket_frame_or_the_ticket_http_error() {
        assert_eq!(Refusal::Unknown.frame(), UNKNOWN_STREAM_FRAME);
        assert_eq!(Refusal::Unauthorized.frame(), UNAUTHORIZED_FRAME);
        assert_eq!(
            Refusal::Unknown.into_error().into_response().status(),
            StatusCode::BAD_REQUEST,
            "an unknown stream at the handshake is a 400"
        );
        assert_eq!(
            Refusal::Unauthorized.into_error().into_response().status(),
            StatusCode::UNAUTHORIZED,
            "and a private stream without a token the 401 the ticket pins"
        );
    }
}
