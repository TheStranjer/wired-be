//! Flat notifications over HTTP: `GET /api/v1/notifications` and its
//! single-row read, `POST /api/v1/notifications/clear`, the two
//! dismissals, `DELETE /api/v1/notifications/destroy_multiple` and `POST
//! /api/pleroma/notifications/read` (`src/api/notifications.rs`).
//!
//! Every test seeds the `notifications` rows directly through the
//! `create_notification` fixture and observes them through both the flat
//! v1 listing and the grouped v2 one (`src/api/notifications_v2.rs`
//! shares the rows and the soft stamp): the listing answers newest first
//! with the `Link` header keyed on the row ids, the `types[]` /
//! `exclude_types[]` / `include_types[]` / `account_id` filters and the
//! `with_muted` exclusions narrow it, dismissed rows vanish from every
//! read while the rows stay, the `read` clauses mark seen by id order
//! (`id` reads that row and its elders, `max_id` reads up to the cursor
//! and answers the marked array), deleting a status takes its
//! notifications with it, the reads demand `read:notifications` and every
//! write route demands `write:notifications`.

#![cfg(test)]

/// The clear, single-dismiss, legacy-dismiss and `destroy_multiple` routes.
mod dismiss;
/// The credential and scope gates of every route.
mod gates;
/// The flat listing: order, shape, cursors, limits, filters and exclusions.
mod listing;
/// The Pleroma `read` route and the cascade of a deleted status.
mod read;
/// The show route and every notification type rendered through the listing.
mod show_types;
/// Shared consts, fixtures, seeds and helpers every submodule draws on.
mod support;
