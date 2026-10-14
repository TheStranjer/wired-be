//! Real-time event delivery: the streaming bus behind the `/api/v1/streaming`
//! WebSocket endpoint.
//!
//! [`hub`] holds the [`Hub`](hub::Hub) — one in-process
//! `tokio::sync::broadcast` channel per app instance, bridged across
//! instances over Postgres `NOTIFY` (decision 11: no Redis). It is the
//! Rust shape of Pleroma's `Pleroma.Web.Streamer`
//! (`lib/pleroma/web/streamer.ex`), whose `broadcast/4` fans an event out
//! through `PubSub` topics; here the topics are the stream names every
//! [`StreamEvent`](hub::StreamEvent) carries instead.
//!
//! Producers build a [`StreamEvent`](hub::StreamEvent) and hand it to
//! [`Hub::publish`](hub::Hub::publish); WebSocket connections (a later
//! ticket) take [`Hub::subscribe`](hub::Hub::subscribe) receivers and
//! filter them with [`stream_matches`](hub::stream_matches) and the
//! event's [`Audience`](hub::Audience).

pub mod hub;
