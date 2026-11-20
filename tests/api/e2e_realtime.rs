#![cfg(test)]

//! End-to-end realtime scenarios (WBE-226): the streaming WebSocket driven
//! through a real listener, Web Push delivered end to end against a mock
//! push service, the client-to-server `ActivityPub` walk, a `Link`-header
//! conformance sweep over every shared-`PageParams` listing, and the
//! emoji-pack lifecycles.
//!
//! Unlike every other module of this crate, this one serves the real
//! application over a `TcpListener` bound on `127.0.0.1:0` and talks to it
//! over actual sockets: `tokio-tungstenite` for the streaming frames and
//! `reqwest` for HTTP, because these scenarios are precisely the ones a
//! `oneshot` request cannot express — a WebSocket needs the live connection
//! to hand over, a push delivery leaves the process through the instance's
//! own outbound client, and the `Link` conformance sweep reads response
//! headers the way a remote client does. Every fact a scenario observes is
//! produced through the public HTTP API; the databases are still leased
//! slots of the fixture (decision 13), held by the served app for its whole
//! life, and the streaming events ride the hub the served router reads.
//!
//! The per-endpoint refusals live in the area files (`streaming.rs`,
//! `push.rs`, `activitypub.rs`, `emoji_packs.rs`, each listing's own); this
//! module proves the journeys: a post made over HTTP arriving rendered on
//! five different sockets, a follow over HTTP waking an encrypted push
//! payload a browser key decrypts, an outbox `Create` with an uploaded
//! attachment reading back as a real status with its media, twenty-five
//! rows walking their cursors through the `Link` header, and a pack built,
//! shared, archived, downloaded from a mock instance and reacted with.

/// The client-to-server `ActivityPub` scenarios.
mod c2s;
/// The emoji-pack lifecycle scenarios.
mod emoji_packs;
/// The pagination conformance sweep.
mod keyset_sweep;
/// The served application and its HTTP helpers.
mod server;
/// The WebSocket client and the world builders.
mod sockets;
/// The recipient-scoped streaming scenarios.
mod streaming_private;
/// The public-facing streaming scenarios.
mod streaming_public;
/// The Web Push scenarios.
mod web_push;
