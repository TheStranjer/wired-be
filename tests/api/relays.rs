//! Relays over federation (WBE-186): the instance actor's follow of a
//! remote relay, the publisher's fan-out to accepted relays, the relay's
//! `Announce` fold, and the relay actor's own endpoints.
//!
//! Everything runs against a loopback mock of a relay instance: its
//! actor document at `/relay` (plus a `Service`, a `Person` and a `Group`
//! for the type gate), an inbox that records every delivery, and the
//! `Note` a relay re-broadcast is fetched from — the stock test config
//! lets the instance's own HTTP client reach loopback. What this pins,
//! per the ticket:
//!
//! - [`follow_relay`] fetches the relay actor, writes the instance
//!   actor's `follows` row with its minted activity id and the `relays`
//!   row `pending`, and sends the `Follow` — `to` the relay's inbox,
//!   `object` its actor id — as one signed delivery; a restated follow
//!   keeps one row and one queued send; the relay's `Accept`, folded by
//!   the existing handler with nobody following the relay, turns both
//!   rows accepted;
//! - the publisher adds the accepted relay's inbox to the fan-out of
//!   public `Create`, `Announce`, `Update` and `Delete` activities and of
//!   the actor's own `Update`, and nothing at all while the subscription
//!   is `pending` or the activity addresses nobody public;
//! - an `Announce` from an accepted relay folds by fetching its object
//!   through `resolve_status` and ingesting it as an ordinary public
//!   remote status, and creates no reblog row; a non-relay actor's
//!   `Announce` of the same object still boosts, and a `pending` relay is
//!   no relay yet;
//! - [`unfollow_relay`] sends the `Undo` of that very `Follow`, deletes
//!   the `follows` row and the `relays` row, and takes the relay out of
//!   the fan-out;
//! - `GET /relay` answers the instance actor document,
//!   `GET /relay/following` and `GET /relay/followers` answer empty
//!   `OrderedCollection`s, and `POST /relay/inbox` is the shared inbox
//!   handler — all with the usual `Accept` negotiation.
//!
//! The admin endpoints that call these functions are the admin tickets'
//! range; here they are exercised directly on the [`AppState`].
//!
//! The area is one module per surface: [`follow`] for the subscription
//! handshake and its withdrawal, [`fan_out`] for the publisher's fan-out
//! and the relay `Announce` fold, [`routes`] for the `/relay` endpoints,
//! and [`support`] for the mock relay and the fixtures they share.
//!
//! [`AppState`]: wired_be::state::AppState
//! [`fan_out`]: fan_out
//! [`follow`]: follow
//! [`follow_relay`]: wired_be::services::relay::follow_relay
//! [`routes`]: routes
//! [`support`]: support
//! [`unfollow_relay`]: wired_be::services::relay::unfollow_relay

#![cfg(test)]

/// The publisher's fan-out to accepted relays — public and non-public
/// activities alike — and the three relay `Announce` fold tests.
mod fan_out;

/// The follow handshake: the walk to accepted, the restated follow, the
/// followable shapes and the withdrawal.
mod follow;

/// The relay actor document, its subscription collections and the
/// shared inbox at `POST /relay/inbox`.
mod routes;

/// The mock relay, the consts and the fixtures every submodule of this
/// area shares.
mod support;
