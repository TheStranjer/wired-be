//! The admin API's JSON shapes: the `GET /api/v1/pleroma/admin/moderation_log`
//! page and the `GET /api/v1/pleroma/admin/stats` counters.
//!
//! Pleroma renders both through
//! `Pleroma.Web.AdminAPI.ModerationLogView`
//! (`lib/pleroma/web/admin_api/views/moderation_log_view.ex`) and the
//! inline map of `AdminAPIController.stats`. Like that view, a log entry
//! answers the four keys `show.json` names: the row `id` (hyphenated
//! `UUIDv7` text per decision 4), the stored `data` map verbatim,
//! `inserted_at` as unix seconds, and the `message` sentence — stored as
//! built here, where Pleroma re-derives it on every read. The stats answer
//! is the `status_visibility` block over [`crate::models::status`]'s live
//! counters.

use serde::Serialize;
use serde_json::Value;

use crate::models::{moderation_log, status::StatusVisibilityCounts};

/// One entry of the moderation log listing, Pleroma's
/// `ModerationLogView` `"show.json"`.
#[derive(Debug, Serialize)]
pub struct ModerationLogEntry {
    /// The row's `UUIDv7` as lowercase hyphenated text (decision 4).
    pub id: String,
    /// The stored data map, verbatim: `actor`, `action`, `message` and the
    /// action-specific subject and extras
    /// ([`crate::models::moderation_log::record`]).
    pub data: Value,
    /// `inserted_at` as unix epoch seconds — Pleroma's
    /// `DateTime.to_unix`.
    pub time: i64,
    /// The full sentence of the action, as stored.
    pub message: String,
}

/// The whole `GET /api/v1/pleroma/admin/moderation_log` answer: one
/// newest-first page and the total the filters select.
#[derive(Debug, Serialize)]
pub struct ModerationLogPage {
    /// The page's entries, newest first.
    pub items: Vec<ModerationLogEntry>,
    /// How many entries match the filters, pages included.
    pub total: i64,
}

/// The four visibility counters, the shape Pleroma's `CounterCache` row
/// answers them in: the Mastodon names, as JSON numbers.
#[derive(Debug, Serialize)]
pub struct StatusVisibility {
    /// Live, non-reblog `public` statuses of the selected audience.
    pub public: i64,
    /// Live, non-reblog `unlisted` statuses of the selected audience.
    pub unlisted: i64,
    /// Live, non-reblog `private` statuses of the selected audience.
    pub private: i64,
    /// Live, non-reblog `direct` statuses of the selected audience.
    pub direct: i64,
}

/// The `GET /api/v1/pleroma/admin/stats` answer: the four visibility
/// counters under their one key, Pleroma's `%{"status_visibility" =>
/// counters}`.
#[derive(Debug, Serialize)]
pub struct AdminStats {
    /// The live-status counters of the selected audience.
    pub status_visibility: StatusVisibility,
}

impl ModerationLogEntry {
    /// Renders `row` in Pleroma's entry shape.
    #[must_use]
    pub fn from_row(row: &moderation_log::ModerationLog) -> Self {
        Self {
            id: row.id.to_string(),
            data: row.data.clone(),
            time: row.inserted_at.timestamp(),
            message: row.message.clone(),
        }
    }
}

impl ModerationLogPage {
    /// Renders one [`moderation_log::Page`] of the listing.
    #[must_use]
    pub fn from_page(page: &moderation_log::Page) -> Self {
        Self {
            items: page
                .items
                .iter()
                .map(ModerationLogEntry::from_row)
                .collect(),
            total: page.total,
        }
    }
}

impl AdminStats {
    /// Renders the four counters of one
    /// [`status_visibility_counts`](crate::models::status::status_visibility_counts)
    /// read.
    #[must_use]
    pub const fn from_counts(counts: &StatusVisibilityCounts) -> Self {
        Self {
            status_visibility: StatusVisibility {
                public: counts.public,
                unlisted: counts.unlisted,
                private: counts.private,
                direct: counts.direct,
            },
        }
    }
}
