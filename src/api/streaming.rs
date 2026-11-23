//! The streaming WebSocket: `GET /api/v1/streaming`.
//!
//! The reference is Pleroma's `Pleroma.Web.MastodonAPI.WebsocketHandler`
//! (`lib/pleroma/web/mastodon_api/websocket_handler.ex`) for the handshake,
//! the client messages and the heartbeat, and `Pleroma.Web.Streamer`
//! (`lib/pleroma/web/streamer.ex`) for the stream rules and the per-reader
//! filter. Where the ticket pins a wording it wins (decision 16): the 401
//! body, the two error frames and the 30 s ping are the ticket's.
//!
//! One connection is:
//!
//! - **a reader of the hub**. Every connection takes a
//!   [`Hub::subscribe`](crate::streaming::hub::Hub::subscribe) receiver and
//!   forwards the events it may see as Mastodon's text frame,
//!   `{"stream":["user:<reader uuid>"],"event":"update","payload":"<json
//!   string>"}`. An event passes when [`stream_matches`] finds one of the
//!   connection's subscriptions on it, its [`Audience`] covers the socket's
//!   reader (a `Users` event reaches those users only), a `local_only` event
//!   has a reader at all — an anonymous socket never sees one — and the
//!   reader's own `Filters` say nothing against it. An event that carries a
//!   [`render`](crate::streaming::hub::StreamEvent::render) hint instead of
//!   a payload has its entity rendered for this connection's OWN viewer at
//!   send time — one hub event, each socket its own view, the way Pleroma's
//!   `Streamer` renders in the socket process: a hinted status through
//!   [`render_status`], a hinted notification through the notifications
//!   endpoint's builder, a hinted chat through the chats endpoint's builder
//!   and a hinted conversation through the conversations endpoint's one.
//!   A hint the socket cannot answer for its reader — no row, not visible,
//!   a deactivated actor, or text one of the reader's irreversible phrase
//!   filters of the stream's context matches (`services::filter`, the REST
//!   timelines' own rules; a warn match annotates `filtered` instead) —
//!   drops the event rather than sending an entity-less frame. The socket's viewer is its authenticated [`User`]
//!   row, loaded once when the socket opens or authenticates.
//! - **its own subscription set**, opened at the handshake by `stream` (plus
//!   `tag` / `list` / `instance`) and changed afterwards by the `subscribe`
//!   and `unsubscribe` frames, capped at [`MAX_SUBSCRIPTIONS`].
//! - **an authentication context**. `access_token` — or, the way the Mastodon
//!   web client sends it, the first `Sec-WebSocket-Protocol` value, which is
//!   echoed back as the chosen subprotocol — names a user. A token that does
//!   not resolve rejects the upgrade with 401
//!   `{"error":"Invalid access token"}`; a public stream needs no token at
//!   all. `{"type":"pleroma:authenticate"}` gives an anonymous socket a user
//!   without reconnecting.
//! - **a heartbeat**. A WebSocket Ping every [`PING_INTERVAL`] (what
//!   [`Config::streaming_ping_interval_secs`] defaults to), and a socket that
//!   has not answered the ping it was last sent is closed.
//!
//! The producing side — who publishes what on which stream — is
//! [`crate::streaming::hub`]; this module only ever reads that bus.
//!
//! This module is a tree. `names` holds the stream names, the error frames,
//! the accepted scopes and the public stream rule; `upgrade` the handshake —
//! its `Upgrade` extractor, the `connect` endpoint and the token it reads;
//! `connection` the socket's own state, with its client messages in
//! `connection/subscriptions`, its event gate in `connection/delivery` and
//! the send-time renders in `connection/status_hints`,
//! `connection/notification_hints`, `connection/chat_hints` and
//! `connection/conversation_hints`; `args` the parameter, message and frame
//! shapes; `filters` the reader's suppression sets; `facts` what one payload
//! says about the entity it carries; and `run` the socket loop that ties them
//! together. `test_support` holds the fixtures the unit tests share.
//!
//! [`stream_matches`]: crate::streaming::hub::stream_matches
//! [`Audience`]: crate::streaming::hub::Audience
//! [`User`]: crate::models::user::User
//! [`render_status`]: crate::services::render_status::render_status
//! [`Config::streaming_ping_interval_secs`]: crate::config::Config::streaming_ping_interval_secs

use axum::{Router, routing::get};

use crate::state::AppState;

use self::upgrade::connect;

mod args;
mod connection;
mod facts;
mod filters;
mod names;
mod run;
mod upgrade;

#[cfg(test)]
mod test_support;

pub use self::names::{FILTER_TTL, MAX_SUBSCRIPTIONS, PING_INTERVAL};

/// The `/api/v1/streaming` router: the WebSocket endpoint and the health
/// probe Mastodon's streaming server answers on the same prefix. Merged into
/// [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/streaming", get(connect))
        .route("/streaming/health", get(health))
}

/// `GET /api/v1/streaming/health` — `200` with the text body `OK`, the probe
/// Mastodon's clients poll before opening a socket.
async fn health() -> &'static str {
    "OK"
}

#[cfg(test)]
mod tests {
    use super::health;

    #[tokio::test]
    async fn the_health_probe_answers_ok_as_text() {
        let body = health().await;

        assert_eq!(body, "OK", "the probe's body is exactly OK");
    }
}
