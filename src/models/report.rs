//! The `reports` table: one account telling this instance's moderators about
//! another, with the statuses it cites for context and the rules it names.
//!
//! Reference: Pleroma's `CommonAPI.report`
//! (`lib/pleroma/web/common_api.ex`) over `ActivityPub.flag`
//! (`lib/pleroma/web/activity_pub/activity_pub.ex`) and `Pleroma.ReportNote`
//! (`lib/pleroma/report_note.ex`). Pleroma stores a report as a `Flag`
//! `Activity` whose `data` map holds the actor, the reported account, the
//! reported statuses and the cited rules; per `docs/ARCHITECTURE.md`
//! decision 6 that becomes one relational row here — [`Report`] — with the
//! status context in `report_statuses` and the rule references in the row's
//! own `rule_ids` array. `report_notes`, the moderators' side of the table,
//! is the admin API's ledger of its own: [`ReportNote`], [`create_note`],
//! [`notes_for`] and [`delete_note`] are its writers and readers, and no
//! client endpoint touches it.
//!
//! A report is a statement, not a mutable relationship, so [`create`] is the
//! only writer of a new row and it never deduplicates: the same account may
//! report the same account twice (Mastodon allows it, and Pleroma's `Flag`
//! activities carry no unique key either), and each filing is its own row
//! with its own id. Deleting either account takes its reports through the
//! foreign keys' cascade.
//!
//! The two text columns are closed sets the migration's `CHECK` constraints
//! state and these enums spell out: [`Category`] is why the report was filed
//! and [`ReportState`] is where the moderation stands. Both are stored as
//! their lowercase variant names, exactly like
//! [`Visibility`](crate::models::status::Visibility) and
//! [`FollowState`](crate::models::follow::FollowState).
//!
//! A report moves through moderation only by [`set_state`] and [`assign`]:
//! the first writes the state and, because the entity's `action_taken` *is*
//! [`Report::action_taken_at`] read as a flag, stamps the moment a
//! [`ReportState::Resolved`] report was acted on, naming the acting
//! moderator in [`action_taken_by`](Report::action_taken_by), and clears
//! both again when a report is reopened; the second names (or un-names) the
//! moderator who owns it. Both answer the rewritten row, `None` when no
//! report carries the id.
//!
//! Whether a report may be filed against an account at all — the reporter is
//! not the target, the cited statuses are the target's and visible to the
//! reporter, the cited rules exist — is the endpoints' question (see
//! [`crate::api::reports`]); [`create`] enforces only its own self guard and
//! what the schema's foreign keys enforce.

mod kinds;
mod listings;
mod mastodon_list;
mod moderation;
mod notes;
mod own;
mod writes;

#[cfg(test)]
mod test_support;

pub use kinds::{Category, ReportState};
pub use listings::{AdminListPage, AdminListParams, admin_list, list, statuses_for_reports};
pub use mastodon_list::{MastodonListParams, mastodon_list};
pub use moderation::{assign, set_state};
pub use notes::{create_note, delete_note, notes_for, notes_for_reports};
pub use own::{DEFAULT_OWN_PAGE_SIZE, MAX_OWN_PAGE_SIZE, OwnReportPage, list_for_reporter};
pub use writes::{create, get, set_ap_id, statuses_for};

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// 422 message [`create`] answers when an account reports itself, the wording
/// this ticket pins (Pleroma's `CommonAPI.report` has no such guard; Mastodon's
/// `Report` changeset refuses the self relationship the way [`block`] does).
///
/// [`block`]: crate::models::block::block
const SELF_REPORT: &str = "Can not report yourself";

/// One row of `reports`: `reporter_id`'s report about `target_id`.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Report {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4); what the
    /// entity's `id` and the listing's cursors carry.
    pub id: Uuid,
    /// The account that filed the report. Deleting it deletes the report.
    pub reporter_id: Uuid,
    /// The account the report is about; the entity's `target_account`.
    pub target_id: Uuid,
    /// What the reporter wrote, verbatim; empty when they said nothing.
    pub comment: String,
    /// Which kind of problem this is, from [`Category`].
    pub category: Category,
    /// Whether the reporter asked for the report to be forwarded to the
    /// target's own instance.
    pub forwarded: bool,
    /// Where the report stands, from [`ReportState`].
    pub state: ReportState,
    /// The moderator the report is assigned to; `None` is the unassigned
    /// queue.
    pub assigned_to: Option<Uuid>,
    /// When a moderator acted on the report; `None` while none has, and the
    /// entity's `action_taken` is this column read as a flag.
    pub action_taken_at: Option<DateTime<Utc>>,
    /// The moderator who acted on the report (WBE-214); `None` while no
    /// resolution has named one, and the Mastodon admin entity's
    /// `action_taken_by_account` reads this column.
    pub action_taken_by: Option<Uuid>,
    /// The instance rules the reporter cited, in the order it cited them;
    /// empty when it cited none.
    pub rule_ids: Vec<Uuid>,
    /// `ActivityPub` id of the `Flag` activity behind this report (decision 6):
    /// a `{BASE_URL}/activities/{uuid}` id for a local reporter, `None` for a
    /// remote one until the federation ticket records the id it sent.
    pub ap_id: Option<String>,
    /// When the report was filed; the entity's `created_at`.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by [`set_state`] and [`assign`].
    pub updated_at: DateTime<Utc>,
}

/// One row of `report_notes`: a moderator's note on a report, never the
/// reporter's (the row's [`comment`](Report::comment) is that).
///
/// Reference: Pleroma's `Pleroma.ReportNote`
/// (`lib/pleroma/report_note.ex`), whose `create` the admin notes endpoint
/// runs and whose `destroy` its delete endpoint runs; here the note
/// belongs to its report by a plain `report_id` and the pair is what
/// [`delete_note`] keys on.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct ReportNote {
    /// `UUIDv7` primary key, minted by [`create_note`] (decision 4).
    pub id: Uuid,
    /// The report the note is on. Deleting the report deletes its notes
    /// (the cascade the migration states).
    pub report_id: Uuid,
    /// The moderator who wrote the note; `None` is the column's no-author
    /// spelling, which no endpoint writes — [`create_note`] always names
    /// its author.
    pub user_id: Option<Uuid>,
    /// The note's text, verbatim.
    pub content: String,
    /// When the note was written.
    pub inserted_at: DateTime<Utc>,
}

/// What a reporter decides when it files a report: the content of one
/// [`create`], with both accounts and the minted id supplied by the caller of
/// [`create`] rather than by the reporter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewReport {
    /// The reporter's own words; empty for none.
    pub comment: String,
    /// Why it filed; [`Category::Other`] when it named nothing.
    pub category: Category,
    /// Whether to forward the report to the target's instance.
    pub forwarded: bool,
    /// The instance rules it cited; validated to exist by the endpoint.
    pub rule_ids: Vec<Uuid>,
    /// The statuses it cites for context; validated to be the target's and
    /// visible to the reporter by the endpoint, and deduplicated here.
    pub status_ids: Vec<Uuid>,
}
