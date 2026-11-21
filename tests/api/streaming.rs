#![cfg(test)]

//! The streaming WebSocket over a real socket (`src/api/streaming.rs`).
//!
//! These are the suite's only tests that are not `oneshot` requests: a
//! WebSocket is a connection, not a request, so each test binds a listener on
//! `127.0.0.1:0`, serves the real application on it, and drives it with
//! `tokio-tungstenite`. The database is still a leased slot of the fixture
//! (decision 13), and the events under test are published on the hub the served
//! router reads — the same bus the endpoints will publish on.
//!
//! Delivery is ordered but not synchronous, so the negative assertions never
//! "wait and see": each publishes the event that must be skipped *before* a
//! control event on the same subscription, then reads until the control
//! arrives. The hub offers every event to every subscriber in order, so an
//! event that had no effect before the control never will.
//!
//! The area is one module per surface: [`harness`] for the served
//! application and the socket and event helpers every test calls,
//! [`handshake_auth`] for the upgrade and its token carriers,
//! [`subscriptions`] for the frames that move streams in and out,
//! [`delivery`] for the filters, the audience gates and the heartbeat, and
//! [`live_events`] for the events the real services publish.
//!
//! [`harness`]: harness
//! [`handshake_auth`]: handshake_auth
//! [`subscriptions`]: subscriptions
//! [`delivery`]: delivery
//! [`live_events`]: live_events

/// The heartbeat: pings on a live socket and the close of a silent one,
/// beside the filters, the `Everyone` audience and the `local_only` gate.
mod delivery;

/// The upgrade and its authentication: the health probe, the refusals, the
/// public connect and the query-string and subprotocol bearer carriers.
mod handshake_auth;

/// The served application and the consts, socket helpers and event makers
/// every submodule of this area shares.
mod harness;

/// The service events over a live socket: the create and delete lifecycle,
/// the send-time render, the favourite's notification and the announcement.
mod live_events;

/// The subscription frames: `subscribe` and `unsubscribe`, the list stream,
/// the refusal frames, the cap and `pleroma:authenticate`.
mod subscriptions;
