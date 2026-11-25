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
use chrono::Utc;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::{
    models::{oauth_token, user},
    state::AppState,
    streaming::hub::StreamEvent,
};

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
        Input::Refresh => {
            let flow = refresh(connection).await;
            if flow == Flow::Stop {
                debug!("closing a streaming socket whose credential is no longer valid");
                let _closed = socket.send(Message::Close(None)).await;
            }
            flow
        }
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
        Ok(event) => {
            for frame in connection.frames_for(&event).await {
                if write(socket, Message::Text(frame.into())).await == Flow::Stop {
                    return Flow::Stop;
                }
            }
            Flow::Keep
        }
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

/// Whether the credential the socket authenticated with still stands: its
/// token row exists and has not expired, and its user exists and is active.
/// An anonymous socket holds no credential and always stands. A database
/// failure is not proof of revocation: the socket is kept and the next
/// refresh asks again.
async fn credential_stands(connection: &Connection) -> bool {
    let Some(token_id) = connection.token_id else {
        return true;
    };
    let db = &connection.state.db;
    let token = match oauth_token::get_by_id(db, token_id).await {
        Ok(found) => found,
        Err(err) => {
            warn!(%err, "streaming token could not be re-checked; the socket stays");
            return true;
        }
    };
    let Some(token) = token.filter(|row| row.valid_until > Utc::now()) else {
        return false;
    };
    let Some(user_id) = token.user_id else {
        return false;
    };
    match user::get_by_id(db, user_id).await {
        Ok(found) => found.is_some_and(|row| row.is_active.get()),
        Err(err) => {
            warn!(%err, "streaming user could not be re-checked; the socket stays");
            true
        }
    }
}

/// The refresh tick: a socket whose token was revoked or has expired, or
/// whose account was deactivated or deleted, is over ([`Flow::Stop`]; the
/// caller sends the close frame). Every other socket reads its reader's
/// suppression sets again.
async fn refresh(connection: &mut Connection) -> Flow {
    if !credential_stands(connection).await {
        return Flow::Stop;
    }
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
    use crate::api::streaming::test_support::{account, state, state_with_config, token_for};
    use crate::config::Config;
    use crate::{auth, oauth::scopes::READ};

    use super::*;

    /// A connection authenticated the way the handshake does it: the raw
    /// bearer string resolved to its credential.
    async fn connected(state: &AppState, raw: &str) -> Connection {
        let credential = auth::credential(state, raw)
            .await
            .expect("the token lookup runs")
            .expect("the fixture token is live");
        Connection::new(state.clone(), Some(credential))
            .await
            .expect("the connection opens")
    }

    #[tokio::test]
    async fn a_live_token_keeps_the_socket_and_an_anonymous_socket_always_stays() {
        let (_db, state) = state().await;
        let reader = account(&state, "refresh-live").await;
        let raw = token_for(&state, &reader, READ).await;
        let mut connection = connected(&state, &raw).await;
        let mut anonymous = Connection::new(state.clone(), None).await.unwrap();

        assert!(connection.token_id.is_some(), "the token's id is held");
        assert_eq!(
            refresh(&mut connection).await,
            Flow::Keep,
            "a live token and an active user keep the socket open"
        );
        assert!(anonymous.token_id.is_none(), "an anonymous socket has none");
        assert_eq!(
            refresh(&mut anonymous).await,
            Flow::Keep,
            "an anonymous socket has no credential to lose"
        );
    }

    #[tokio::test]
    async fn a_revoked_token_stops_the_socket_on_the_next_refresh() {
        let (_db, state) = state().await;
        let reader = account(&state, "refresh-revoked").await;
        let raw = token_for(&state, &reader, READ).await;
        let mut connection = connected(&state, &raw).await;
        let token_id = connection.token_id.expect("the token's id is held");

        oauth_token::delete_token(&state.db, token_id)
            .await
            .expect("the token is revoked");

        assert_eq!(
            refresh(&mut connection).await,
            Flow::Stop,
            "a token whose row is gone closes the socket"
        );
    }

    #[tokio::test]
    async fn an_expired_token_stops_the_socket_on_the_next_refresh() {
        let (_db, state) = state().await;
        let reader = account(&state, "refresh-expired").await;
        let raw = token_for(&state, &reader, READ).await;
        let mut connection = connected(&state, &raw).await;
        let _aged = sqlx::query(
            "UPDATE oauth_tokens SET valid_until = now() - interval '1 hour' WHERE id = $1",
        )
        .bind(connection.token_id)
        .execute(&state.db)
        .await
        .expect("the token is back-dated");

        assert_eq!(
            refresh(&mut connection).await,
            Flow::Stop,
            "a token past its expiry closes the socket"
        );
    }

    #[tokio::test]
    async fn a_deactivated_account_stops_the_socket_on_the_next_refresh() {
        let (_db, state) = state().await;
        let reader = account(&state, "refresh-deactivated").await;
        let raw = token_for(&state, &reader, READ).await;
        let mut connection = connected(&state, &raw).await;
        let _deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(reader.id)
            .execute(&state.db)
            .await
            .expect("the account is deactivated");

        assert_eq!(
            refresh(&mut connection).await,
            Flow::Stop,
            "a token of a deactivated user closes the socket"
        );
    }

    #[tokio::test]
    async fn a_socket_that_authenticated_late_is_checked_too() {
        let (_db, state) = state().await;
        let reader = account(&state, "refresh-late").await;
        let raw = token_for(&state, &reader, READ).await;
        let mut connection = Connection::new(state.clone(), None).await.unwrap();
        let replies = connection
            .on_client_text(&format!(
                r#"{{"type":"pleroma:authenticate","token":"{raw}"}}"#
            ))
            .await;
        assert!(replies.is_empty(), "the token is taken up");
        let token_id = connection
            .token_id
            .expect("authenticate holds the token id");

        assert_eq!(refresh(&mut connection).await, Flow::Keep, "still live");
        oauth_token::delete_token(&state.db, token_id)
            .await
            .expect("the token is revoked");

        assert_eq!(
            refresh(&mut connection).await,
            Flow::Stop,
            "a late-authenticated socket closes on revoke as well"
        );
    }

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
            heartbeat_period(&state_with_config(&pool, tuned)),
            Duration::from_secs(3),
            "the configured span is what the socket pings on"
        );
        assert_eq!(
            heartbeat_period(&state_with_config(&pool, zero)),
            Duration::from_secs(1),
            "a zero period would spin the socket loop, so it is read as one second"
        );
    }
}
