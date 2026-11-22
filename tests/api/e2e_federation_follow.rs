//! End-to-end federation of relationships (WBE-224 scenario 2 and the relay
//! half of scenario 7): follow handshakes, withdrawals and blocks over real
//! HTTP.
//!
//! Everything here runs against [`remote::MockPeer`] — a loopback instance
//! with its own keys, actor documents, `WebFinger` and capturing inboxes —
//! through this instance's own public surface: the peer posts signed
//! activities to `POST /inbox`, this instance's answers reach the peer's inbox
//! through the real federation worker, and every fact is read back through
//! `GET /api/v1/accounts/lookup`, `/api/v1/follow_requests`,
//! `/api/v1/accounts/relationships`, `/api/v1/accounts/{id}` and
//! `/api/v1/timelines/public`, or from the peer's capture. Nothing waits on a
//! clock: [`common::run_all_jobs`] drains each queue by hand.
//!
//! What the scenarios pin:
//!
//! - a peer's `Follow` of a local account grants on the spot, is visible as a
//!   follower everywhere the client API says so, and is answered by one signed
//!   `Accept` at the follower's own inbox, embedding the `Follow` and addressed
//!   to the follower alone;
//! - a locked account holds the same `Follow` in `GET /api/v1/follow_requests`
//!   and sends nothing until its owner authorizes it;
//! - `POST /api/v1/follows` of a handle resolves the peer, delivers a signed
//!   `Follow`, and the peer's `Accept` back through `POST /inbox` grants the
//!   relationship and steps both counters;
//! - `POST …/unfollow` sends the `Undo` of that very activity, and a peer's own
//!   `Undo` takes its follow — row and both counters — back;
//! - `POST …/block` publishes a signed `Block` and severs the pair, a peer's
//!   `Block` of a local account reads as `blocked_by` and stops carrying her
//!   posts to him, and his `Undo` of the block lets her be followed again;
//! - [`follow_relay`] against the peer's `Service` relay follows over the real
//!   fetch and delivery, the peer's `Accept` accepts, the accepted relay joins
//!   the fan-out of the next public `Create` at exactly one inbox — its own,
//!   which is the shared endpoint the follower expands to too — its `Announce`
//!   lands as an ordinary public remote status with no boost behind it, and
//!   [`unfollow_relay`] sends the `Undo` and closes the fan-out.
//!
//! The relay rows (the `relays` subscription and the instance actor's own
//! `follows` row) are read from their models: no client endpoint lists them,
//! which is what `admin_federation.rs` exists for.
//!
//! The per-endpoint refusals belong to the area files (`follow_federation.rs`,
//! `follow_federation_out.rs`, `follow_requests.rs`, `blocks.rs`, `relays.rs`);
//! this module owns the round trips.
//!
//! [`follow_relay`]: wired_be::services::relay::follow_relay
//! [`unfollow_relay`]: wired_be::services::relay::unfollow_relay

#![cfg(test)]

mod support;
