//! The socket loop: one task that owns the socket and waits on the client,
//! the hub, the heartbeat and the filter refresh at once.
//!
//! [`run`] is what [`connect`] hands the finished handshake to; every write to
//! the socket happens in this module, which is why the socket never needs
//! splitting into halves.
//!
//! [`connect`]: super::upgrade::connect

use std::{sync::Arc, time::Duration};

use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::{state::AppState, streaming::hub::StreamEvent};

use super::{
    connection::{Connection, Flow, Input},
    filters::Filters,
    names::PING_INTERVAL,
};

/// The heartbeat period of one streaming socket:
/// [`Config::streaming_ping_interval_secs`], whose default is
/// [`PING_INTERVAL`]. A configured `0` is read as one second: a
/// zero-length period would spin the socket loop.
///
/// [`Config::streaming_ping_interval_secs`]: crate::config::Config::streaming_ping_interval_secs
pub(super) fn heartbeat_period(state: &AppState) -> Duration {
    let configured = state.config.streaming_ping_interval_secs;
    let secs = u64::try_from(configured).unwrap_or(PING_INTERVAL.as_secs());
    Duration::from_secs(secs.max(1))
}

/// Serves `socket` until either peer ends the connection.
///
/// One task owns the socket and waits on four things at once: the client's
/// frames, the hub's `events` (taken by [`connect`], before the handshake was
/// answered), the heartbeat and the filter refresh. Every write to the socket
/// happens here, which is why the socket never needs splitting into halves.
///
/// [`connect`]: super::upgrade::connect
pub(super) async fn run(
    mut socket: WebSocket,
    mut connection: Connection,
    mut events: broadcast::Receiver<Arc<StreamEvent>>,
) {
    debug!(
        subscriptions = connection.subscriptions.len(),
        authenticated = connection.reader.is_some(),
        "streaming connection opened"
    );
    loop {
        let input = tokio::select! {
            received = socket.recv() => Input::Client(received),
            event = events.recv() => Input::Bus(event),
            _ = connection.pings.tick() => Input::Ping,
            _ = connection.refreshes.tick() => Input::Refresh,
        };
        if serve_one(&mut socket, &mut connection, input).await == Flow::Stop {
            break;
        }
    }
    debug!("streaming connection closed");
}

/// Handles one [`Input`] and reports whether the connection lives on.
async fn serve_one(socket: &mut WebSocket, connection: &mut Connection, input: Input) -> Flow {
    match input {
        Input::Client(received) => from_client(socket, connection, received).await,
        Input::Bus(received) => from_hub(socket, connection, received).await,
        Input::Ping => heartbeat(socket, connection).await,
        Input::Refresh => refresh_filters(connection).await,
    }
}

/// A frame from the client: any frame at all is proof the socket answers its
/// pings, a text frame may change the connection, and the end of the stream
/// ends it.
async fn from_client(
    socket: &mut WebSocket,
    connection: &mut Connection,
    received: Option<Result<Message, axum::Error>>,
) -> Flow {
    let Some(message) = received else {
        return Flow::Stop;
    };
    connection.awaiting_pong = false;
    match message {
        Ok(Message::Text(text)) => {
            for frame in connection.on_client_text(text.as_str()).await {
                if write(socket, Message::Text(frame.into())).await == Flow::Stop {
                    return Flow::Stop;
                }
            }
            Flow::Keep
        }
        Ok(Message::Close(_)) => Flow::Stop,
        // An inbound `Ping` is answered by the websocket layer itself; a
        // `Pong`, a binary frame or a protocol-level frame carries nothing
        // this endpoint acts on. Their only meaning is that the client lives.
        Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_)) => Flow::Keep,
        Err(err) => {
            debug!(%err, "streaming socket failed");
            Flow::Stop
        }
    }
}

/// An event from the hub: written when this connection may see it, dropped
/// silently when it may not. A lagged receiver skips what fell out of the
/// buffer and keeps streaming.
async fn from_hub(
    socket: &mut WebSocket,
    connection: &Connection,
    received: Result<Arc<StreamEvent>, broadcast::error::RecvError>,
) -> Flow {
    match received {
        Ok(event) => match connection.frame_for(&event).await {
            None => Flow::Keep,
            Some(frame) => write(socket, Message::Text(frame.into())).await,
        },
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            warn!(skipped, "streaming connection fell behind the hub");
            Flow::Keep
        }
        Err(broadcast::error::RecvError::Closed) => Flow::Stop,
    }
}

/// The heartbeat tick: a socket with a ping still outstanding never answered
/// the last one and is closed, every other socket gets the next ping.
async fn heartbeat(socket: &mut WebSocket, connection: &mut Connection) -> Flow {
    if connection.awaiting_pong {
        debug!("closing a streaming socket that never answered a ping");
        let _closed = socket.send(Message::Close(None)).await;
        return Flow::Stop;
    }
    connection.awaiting_pong = true;
    write(socket, Message::Ping(Bytes::new())).await
}

/// Reads the reader's suppression sets again.
async fn refresh_filters(connection: &mut Connection) -> Flow {
    let Some(reader) = connection.reader_id() else {
        return Flow::Keep;
    };
    match Filters::load(&connection.state.db, Some(reader)).await {
        Ok(filters) => connection.filters = filters,
        Err(err) => warn!(%err, "streaming filters could not be refreshed; the old set stays"),
    }
    Flow::Keep
}

/// Writes one frame to the socket; a failed write ends the connection.
async fn write(socket: &mut WebSocket, message: Message) -> Flow {
    match socket.send(message).await {
        Ok(()) => Flow::Keep,
        Err(err) => {
            debug!(%err, "streaming write failed");
            Flow::Stop
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::api::streaming::test_support::app_state_with;
    use crate::config::Config;

    use super::*;

    #[tokio::test]
    async fn a_configured_heartbeat_is_the_socket_loops_period() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        let mut tuned = Config::for_tests();
        tuned.streaming_ping_interval_secs = 3;
        let mut zero = Config::for_tests();
        zero.streaming_ping_interval_secs = 0;

        assert_eq!(
            heartbeat_period(&app_state_with(&pool, tuned)),
            Duration::from_secs(3),
            "the configured span is what the socket pings on"
        );
        assert_eq!(
            heartbeat_period(&app_state_with(&pool, zero)),
            Duration::from_secs(1),
            "a zero period would spin the socket loop, so it is read as one second"
        );
    }
}
