//! The admin federation endpoints over HTTP
//! (`/api/v1/pleroma/admin/relay`, `/instances/{instance}` and
//! `/reload_emoji`, WBE-203): the relay follow / unfollow lifecycle
//! driven through the admin API against a loopback mock relay, the
//! `{actor, followed_back}` ledger, the instance deletion staging its
//! `users.delete_remote` sweeps and clearing the `instances` row, the
//! emoji reload resyncing a freshly written pack into `custom_emojis`,
//! every refusal the ticket pins, and the `AdminUser` / scope gate in
//! front of all five routes.
//!
//! The mock is the one `relays.rs` runs — an `Application` actor at
//! `/relay`, a capturing inbox at `/relay/inbox`, a `Group` outside the
//! relay shapes and a path that answers 404 — reached through the stock
//! test config whose HTTP client tolerates loopback. The relay's follow
//! back at the instance actor (the `followed_back` fact) is staged
//! through the `follows` model: the ledger read only asks that question,
//! and no inbound handshake is needed to state the answer.

#![cfg(test)]

mod emoji_and_gates;
mod instances;
mod relays;
mod support;
