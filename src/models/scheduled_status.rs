//! The `scheduled_statuses` table: create-status requests parked until a
//! future moment.
//!
//! Pleroma keeps these as `Pleroma.ScheduledActivity`
//! (`lib/pleroma/scheduled_activity.ex`); this layout follows
//! `docs/ARCHITECTURE.md` decisions 2, 4 and 5.
//!
//! The row is the whole request kept for replay. [`NewScheduledStatus`]
//! carries the normalised create-status params exactly as the endpoint
//! received them (request keys, `media_ids` as an array of id strings; no
//! `scheduled_at`, no `preview`) beside the account and the due time, and
//! [`insert`] stores them verbatim: nothing is written to `statuses` or
//! `media_attachments` here. When a row's [`scheduled_at`] passes, a job-queue
//! worker (decision 11, later ticket) reads it with [`fetch_due`], creates the
//! status through the ordinary create path, and removes the row with
//! [`delete`].
//!
//! Two reads serve the endpoints that manage schedules rather than fire them:
//! [`count_pending_for_user`] (with [`utc_day_bounds`] and
//! [`count_pending_for_user_between`] for the per-UTC-day variant) answers
//! the create-time limit checks, and [`reserved_media_ids`] is the media
//! reservation gate: since attachments are only referenced by pending rows,
//! an id any schedule names in its `params -> 'media_ids'` array must not be
//! posted by anything else until that schedule fires or is cancelled.
//!
//! The management endpoints themselves ([`crate::api::scheduled_statuses`])
//! need four more, and these are the account-scoped ones: [`page_for_user`]
//! walks one account's schedules as a [`PageParams`] keyset page,
//! [`get_owned`] reads a row only for the account it belongs to,
//! [`reschedule`] moves a row's fire moment in the same scoped statement, and
//! [`delete_owned`] cancels it. Scoping is what makes a foreign schedule
//! indistinguishable from a missing one, which is the 404 those endpoints
//! owe; the unscoped [`get_by_id`] and [`delete`] stay the worker's, since a
//! fire knows the row by id alone and has no account to compare against.
//!
//! [`scheduled_at`]: ScheduledStatus::scheduled_at
//! [`PageParams`]: crate::pagination::PageParams

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

/// The create-time limit counters, the due sweep and the media gate.
mod limits_due;
/// One account's schedules as a keyset page.
mod pages;
/// The `INSERT` and the account-scoped reads and writes of the whole rows.
mod rows;

/// The fixtures the children's tests share.
#[cfg(test)]
mod test_support;

pub use limits_due::{
    count_pending_for_user, count_pending_for_user_between, fetch_due, reserved_media_ids,
    utc_day_bounds,
};
pub use pages::page_for_user;
pub use rows::{
    delete, delete_all_for_user, delete_owned, get_by_id, get_owned, insert, reschedule,
};

/// One row of `scheduled_statuses`: a create-status request waiting for its
/// moment.
#[derive(Debug, sqlx::FromRow)]
pub struct ScheduledStatus {
    /// `UUIDv7` primary key, minted by [`insert`] (decision 4).
    pub id: Uuid,
    /// The account the post is scheduled for; deleting it takes the row.
    pub user_id: Uuid,
    /// The earliest moment the status should fire.
    pub scheduled_at: DateTime<Utc>,
    /// The normalised create-status params as received; see [`insert`] for
    /// the keys it holds and the ones it deliberately does not.
    pub params: Value,
    /// When the schedule was created.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed: [`inserted_at`](Self::inserted_at)'s equal
    /// while the schedule stands as it was created, and later than it once
    /// [`reschedule`] has moved its fire moment.
    pub updated_at: DateTime<Utc>,
}

/// What to insert: everything [`insert`] needs beyond the id it mints and
/// the timestamps the database stamps (the shape of
/// [`NewJob`](super::job::NewJob)).
#[derive(Debug)]
pub struct NewScheduledStatus {
    /// The account the status is scheduled for; a foreign key, so the
    /// account must exist.
    pub user_id: Uuid,
    /// When the status should fire.
    pub scheduled_at: DateTime<Utc>,
    /// The params to store verbatim for the eventual replay.
    pub params: Value,
}
