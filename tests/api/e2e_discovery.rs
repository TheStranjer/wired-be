//! End-to-end discovery and federation scenarios (WBE-223, ticket
//! scenarios 6, 7 and 8): the search trio over the full visibility matrix,
//! the v1 phrase filters applied server-side to the timeline listings
//! Pleroma's `Pleroma.Filter` restricts, the Atom/RSS feeds and the
//! rich-media link card fed by the job queue, and a remote mention
//! resolved through `WebFinger` against a loopback mock peer whose inbox
//! the signed `Create` is then delivered to.
//!
//! Everything is driven over HTTP through [`common`]'s router (the world
//! is built with real creates, follows and filter writes), the mock
//! upstream follows the loopback mock pattern of `follow_by_uri.rs`, and
//! background work is drained with [`common::run_all_jobs`]. The
//! per-endpoint refusals stay owned by the area files (`search.rs`,
//! `filters.rs`, `timelines.rs`, `feeds.rs`, `rich_media.rs`); this module
//! owns the cross-surface journeys, like `e2e_interactions.rs` owns the
//! interaction ones.

#![cfg(test)]

mod support;
