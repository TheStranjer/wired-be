//! End-to-end interaction scenarios (WBE-223 ticket scenario 2): ten
//! complete journeys across the client API, each driven only through the
//! test router and asserted through every surface that renders a status —
//! the direct `GET /api/v1/statuses/{id}`, the home timeline and the
//! public timeline — so the Mastodon counts, the viewer flags
//! (`favourited`, `reblogged`, `bookmarked`, `pinned`, `muted`,
//! `pleroma.thread_muted`), the reaction arrays and the rest of the
//! `pleroma` fields agree on every page, for every viewer, all the way
//! through the stack.
//!
//! The scenarios: favourite/unfavourite (`GET /api/v1/favourites`);
//! reblog/undo (nested `reblog`, once on a follower's home); bookmark +
//! Pleroma bookmark folder (round-tripped through `GET
//! /api/v1/bookmarks`); pin/unpin (the profile's `?pinned=true` page and
//! its pin order); thread mute/unmute (`POST …/mute`); emoji reactions
//! (the `count` inside `pleroma.emoji_reactions` is the reaction count,
//! Pleroma 2.10's spelling, which keeps the object's `reaction_count`
//! off the wire); a poll vote and its locked second ballot; an edit and
//! the history/source that keep it readable; a `statuses.expire` fire
//! drained through [`common::run_all_jobs`] after the job's `run_at` is
//! backdated, which must take the status off every surface; and a delete
//! — its 401/403/404 guards, the boost that falls with it, and the
//! counters left settled.
//!
//! The per-endpoint refusals are pinned by the area files
//! (`favourites.rs`, `reblogs.rs`, …); this module owns the cross-surface
//! coherence, like `e2e_scheduled.rs` owns the queue seam.

#![cfg(test)]

mod expiry_deletes;
mod favourites_reblogs_bookmarks;
mod pins_mutes_reactions;
mod polls_edits;
mod support;
