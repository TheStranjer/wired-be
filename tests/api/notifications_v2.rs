//! The grouped v2 notification API over HTTP: `GET /api/v2/notifications`
//! and its `unread_count`, `{group_key}`, `{group_key}/accounts` and
//! `{group_key}/dismiss` companions (`src/api/notifications_v2.rs`).
//!
//! Every test seeds the `notifications` rows directly through the
//! `create_notification` fixture — the endpoints that *cause* notifications
//! (favourite, react, follow) belong to their own tickets — and asserts the
//! envelope of `accounts` / `statuses` / `notification_groups`, the key
//! shapes (`<type>-<status_id>` grouped, `ungrouped-<id>` for everything
//! else), the sample cap, the cursor pages keyed on
//! `most_recent_notification_id`, the marker-based unread count, and the
//! two scope gates.

#![cfg(test)]

/// The listing filters, `expand_accounts`, emoji reaction groups and the gates.
mod filters_and_gates;
/// The grouped listing's envelope: merging, keys, expansions and samples.
mod grouping;
/// The single-group show, its accounts and the dismissal.
mod groups;
/// The block, domain-block and mute exclusions of every grouped read.
mod hidden_causes;
/// The group page cursors and the marker-based unread count.
mod paging_unread;
/// Shared consts, fixtures, seeds and callers every submodule draws on.
mod support;
