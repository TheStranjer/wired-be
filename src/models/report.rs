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

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::user::User,
    pagination::PageParams,
    util::id::new_id,
};

/// 422 message [`create`] answers when an account reports itself, the wording
/// this ticket pins (Pleroma's `CommonAPI.report` has no such guard; Mastodon's
/// `Report` changeset refuses the self relationship the way [`block`] does).
///
/// [`block`]: crate::models::block::block
const SELF_REPORT: &str = "Can not report yourself";

/// [`create`]: the id this module mints (decision 4), what the reporter
/// filed, and the `ActivityPub` id of the `Flag` this report will federate as
/// (decision 6). `state`, `assigned_to`, `action_taken_at`,
/// `action_taken_by` and the two timestamps are left to the column defaults:
/// a filing opens the report, assigns nobody and acts on nothing.
/// `RETURNING *` brings the row back with those stamps.
const INSERT_SQL: &str = "INSERT INTO reports (
     id, reporter_id, target_id, comment, category, forwarded, rule_ids, ap_id
) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *";

/// [`create`]'s second write: the statuses the report cites, in the same
/// transaction as the row they belong to, so a report is never visible with
/// half its context. `ON CONFLICT (report_id, status_id) DO NOTHING` makes a
/// status cited twice in one request one row (the pair is the table's key).
/// Unknown ids are the caller's question and this statement's failure: the
/// foreign key refuses them and the transaction rolls back.
const ATTACH_STATUSES_SQL: &str = "INSERT INTO report_statuses (report_id, status_id)
     SELECT $1::uuid, id FROM unnest($2::uuid[]) AS requested(id)
ON CONFLICT (report_id, status_id) DO NOTHING";

/// [`get`]: the one report behind a primary key.
const GET_SQL: &str = "SELECT * FROM reports WHERE id = $1";

/// [`set_ap_id`]: the mirror write of an inbound `Flag` — the filed report
/// keeps its identity, only the activity id it stands for arrives. Unknown
/// ids match no row and change nothing.
const SET_AP_ID_SQL: &str = "UPDATE reports SET ap_id = $2 WHERE id = $1";

/// [`statuses_for`]: the statuses one report cites, oldest status first —
/// the `UUIDv7` text sorts in creation order (decision 4), which is the order
/// Mastodon's `status_ids` array reads in. The join on `statuses` is what
/// keeps a deleted post out of the answer: its `report_statuses` row survives
/// the cascade-free tombstone, and a report does not cite a status nobody can
/// see.
const STATUSES_FOR_SQL: &str = "SELECT rs.status_id FROM report_statuses rs
     JOIN statuses s ON s.id = rs.status_id AND s.deleted_at IS NULL
     WHERE rs.report_id = $1 ORDER BY rs.status_id";

/// [`list`]: every report, newest first. The statement keeps its `WHERE`
/// clause even unfiltered because [`PageParams::apply_keyset`] appends its
/// cursors to it. The admin listing's filters — state, rule, assignee — are
/// [`admin_list`]'s own statement, which pages by offset window.
const LIST_SQL: &str = "SELECT * FROM reports WHERE true";

/// [`set_state`]: the state, the `action_taken_at` and `action_taken_by`
/// that belong with it, and the `updated_at` bump. Resolution is the action
/// the flag reports, so moving to `resolved` stamps the moment once (a
/// repeat keeps the first stamp, and with it the first acting moderator);
/// reopening a report clears both, so neither the `action_taken` flag nor
/// the entity's `action_taken_by_account` may claim an action a reopened
/// report no longer stands on; closing takes no action (Mastodon's `close!`
/// leaves `action_taken` false) and leaves both columns as they were. No
/// `WHERE`-clause guard on a missing row: [`set_state`] reports the miss as
/// `None`.
const SET_STATE_SQL: &str = "UPDATE reports
      SET state = $2,
          action_taken_at = CASE
              WHEN $2 = 'resolved' THEN COALESCE(action_taken_at, now())
              WHEN $2 = 'open' THEN NULL
              ELSE action_taken_at
          END,
          action_taken_by = CASE
              WHEN $2 = 'resolved' THEN COALESCE(action_taken_by, $3)
              WHEN $2 = 'open' THEN NULL
              ELSE action_taken_by
          END,
          updated_at = now()
    WHERE id = $1
  RETURNING *";

/// [`assign`]: the moderator who owns the report, `NULL` handing it back to
/// the unassigned queue, and the `updated_at` bump that goes with any write.
const ASSIGN_SQL: &str = "UPDATE reports
      SET assigned_to = $2, updated_at = now()
    WHERE id = $1
  RETURNING *";

/// The start of [`admin_list`]'s page statement; [`push_admin_filters`]
/// appends the `WHERE` clause, then the newest-first order and the
/// `LIMIT`/`OFFSET` window the admin report listing pages by (the same
/// offset walk the moderation log takes, keyed on the report's own id).
const ADMIN_SELECT_SQL: &str = "SELECT * FROM reports";

/// The start of the total statement behind [`admin_list`]: the same
/// filters, counted without the window.
const ADMIN_COUNT_SQL: &str = "SELECT count(*) FROM reports";

/// [`mastodon_list`]: the start of the Mastodon admin listing's page
/// statement. Its `WHERE true` is where [`push_mastodon_filters`] hangs the
/// state and account filters and [`PageParams::apply_keyset`] the cursors,
/// the newest-first order and the window — the same keyset walk over the
/// report's own id (decision 4) that [`list`] takes, under the admin
/// listing's filters.
const MASTODON_SELECT_SQL: &str = "SELECT * FROM reports WHERE true";

/// [`create_note`]: the minted id (decision 4), the report, the moderator
/// who wrote it and the text; `inserted_at` is the column default's and
/// `RETURNING *` brings the stored row back.
const INSERT_NOTE_SQL: &str = "INSERT INTO report_notes (id, report_id, user_id, content)
     VALUES ($1, $2, $3, $4) RETURNING *";

/// [`notes_for_reports`]: the notes of many reports at once, each report's
/// own notes oldest first (`UUIDv7` text sorts in creation order, decision
/// 4 — the order `report_notes_report_id_idx` states). An empty id list is
/// answered by skipping the read.
const NOTES_FOR_SQL: &str = "SELECT * FROM report_notes
     WHERE report_id = ANY($1) ORDER BY id";

/// [`delete_note`]: the one writer, keyed on the pair so a note of another
/// report is as absent here as it is to a caller that never saw it;
/// `RETURNING *` hands the deleted row back to the moderation-log entry
/// that names its text.
const DELETE_NOTE_SQL: &str = "DELETE FROM report_notes
     WHERE id = $1 AND report_id = $2 RETURNING *";

/// [`statuses_for_reports`]: the batched twin of [`statuses_for`] — the
/// same live-status join and the same oldest-first order, for a whole
/// page of reports in one read, keyed `(report_id, status_id)`.
const STATUSES_FOR_MANY_SQL: &str = "SELECT rs.report_id, rs.status_id
     FROM report_statuses rs
     JOIN statuses s ON s.id = rs.status_id AND s.deleted_at IS NULL
     WHERE rs.report_id = ANY($1)
     ORDER BY rs.report_id, rs.status_id";

/// Why the reporter filed a report.
///
/// Stored in the `category` text column as the enum's lowercase variant name
/// (`spam`, `legal`, `violation` or `other`, which is exactly what the
/// column's `CHECK` constraint allows). The four are Mastodon's
/// `REPORT_CATEGORIES`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Category {
    /// Unsolicited bulk or repetitive content.
    Spam,
    /// Content that breaks applicable law in the instance's jurisdiction.
    Legal,
    /// Content that breaks an instance rule (the report's
    /// [`rule_ids`](Report::rule_ids) name which).
    Violation,
    /// Anything else; the column default, and what an absent `category`
    /// becomes.
    Other,
}

/// The message [`FromStr`] answers for text the column would not accept.
const UNKNOWN_CATEGORY: &str = "unknown report category";

impl Category {
    /// Every category, in the order the column's `CHECK` constraint lists
    /// them.
    pub const ALL: [Self; 4] = [Self::Spam, Self::Legal, Self::Violation, Self::Other];

    /// The stored (and wire) text of this category: its lowercase name, which
    /// is what the `category` column holds and what the entity reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spam => "spam",
            Self::Legal => "legal",
            Self::Violation => "violation",
            Self::Other => "other",
        }
    }
}

/// [`as_str`](Self::as_str) is the text form, so `format!("{category}")` and
/// the database agree.
impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parses the stored text back, case-sensitively: the column only ever holds
/// these four spellings and clients send them verbatim.
impl FromStr for Category {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "spam" => Ok(Self::Spam),
            "legal" => Ok(Self::Legal),
            "violation" => Ok(Self::Violation),
            "other" => Ok(Self::Other),
            _ => Err(UNKNOWN_CATEGORY),
        }
    }
}

/// Where a report stands with the moderators.
///
/// Stored in the `state` text column as the enum's lowercase variant name
/// (`open`, `closed` or `resolved`, which is exactly what the column's
/// `CHECK` constraint allows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum ReportState {
    /// Filed and waiting: what [`create`] leaves a report at.
    Open,
    /// Dropped without action; nothing was done about the report.
    Closed,
    /// Action was taken, which is the moment [`set_state`] stamps
    /// [`action_taken_at`](Report::action_taken_at) with.
    Resolved,
}

/// The message [`FromStr`] answers for text the column would not accept.
const UNKNOWN_STATE: &str = "unknown report state";

impl ReportState {
    /// Every state, in the order the column's `CHECK` constraint lists them.
    pub const ALL: [Self; 3] = [Self::Open, Self::Closed, Self::Resolved];

    /// The stored (and wire) text of this state: its lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Resolved => "resolved",
        }
    }
}

/// [`as_str`](Self::as_str) is the text form, so `format!("{state}")` and the
/// database agree.
impl fmt::Display for ReportState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parses the stored text back, case-sensitively: the column only ever holds
/// these three spellings and the admin endpoints send them verbatim.
impl FromStr for ReportState {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            "resolved" => Ok(Self::Resolved),
            _ => Err(UNKNOWN_STATE),
        }
    }
}

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

/// The filters and window of one [`admin_list`] read, as
/// `GET /api/v1/pleroma/admin/reports` parses them (Pleroma's
/// `ReportController.index` params through `Utils.get_reports`).
#[derive(Debug)]
pub struct AdminListParams {
    /// Only reports in this state; `None` is every state.
    pub state: Option<ReportState>,
    /// Only reports citing this instance rule (Pleroma's `rule_id` filter
    /// over the Flag's `context.rules`, here the row's own
    /// [`rule_ids`](Report::rule_ids) array).
    pub rule_id: Option<Uuid>,
    /// Only reports assigned to this account (Pleroma's
    /// `restrict_assigned_account`, which matches the id stored on the
    /// Flag; here the [`assigned_to`](Report::assigned_to) column).
    pub assigned_account: Option<Uuid>,
    /// One-based page number; the caller defaults and validates it (1).
    pub page: i64,
    /// Rows per page; the caller defaults and caps it.
    pub page_size: i64,
}

/// The page [`admin_list`] answers: one window of reports, newest filing
/// first, and the total number of rows the same filters select.
#[derive(Debug)]
pub struct AdminListPage {
    /// The reports of this page.
    pub reports: Vec<Report>,
    /// How many reports the filters match, pages included.
    pub total: i64,
}

/// The filters of one [`mastodon_list`] read, as
/// `GET /api/v1/admin/reports` parses them.
///
/// These are Mastodon's admin-report parameters spelled the way the ticket
/// pins them: `resolved`, `account_id` and `target_account_id`.
#[derive(Debug)]
pub struct MastodonListParams {
    /// The `resolved` filter: `Some(true)` keeps the acted-on states
    /// (`resolved` *and* `closed` — the entity's `action_taken` is this same
    /// pair read from the state), and `None` or `Some(false)` keeps the open
    /// queue, so an unfiltered request lists what still needs action.
    pub resolved: Option<bool>,
    /// Only reports filed by this account (Mastodon's `account_id` names the
    /// reporter); `None` is every reporter.
    pub reporter_id: Option<Uuid>,
    /// Only reports about this account (Mastodon's `target_account_id`);
    /// `None` is every target.
    pub target_id: Option<Uuid>,
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

/// Files `reporter`'s report about `target` and returns the stored row.
///
/// The write is the row and its status context in one transaction, so a
/// report is never readable without the statuses it cites. Filing twice is
/// two reports: nothing here deduplicates a pair, and every filing mints its
/// own id (decision 4).
///
/// `base_url` is the instance origin, i.e. [`Config::base_url`], which the
/// model layer is handed rather than reading configuration itself; a reporter
/// this instance owns mints a fresh `{base_url}/activities/{uuid}` for the
/// `Flag` this report federates as (decision 6), a remote one stores `None`
/// until the federation ticket mirrors the activity it arrived with.
///
/// [`Config::base_url`]: crate::config::Config::base_url
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `"Can not report yourself"` when the two
/// users are the same account. Any database failure (connection, encoding, a
/// cited status or rule that names no row) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(
    pool: &PgPool,
    base_url: &str,
    reporter: &User,
    target: &User,
    new: &NewReport,
) -> AppResult<Report> {
    if reporter.id == target.id {
        return Err(AppError::Unprocessable(SELF_REPORT.to_owned()));
    }
    let mut tx = pool.begin().await?;
    let row = sqlx::query_as::<_, Report>(INSERT_SQL)
        .bind(new_id())
        .bind(reporter.id)
        .bind(target.id)
        .bind(&new.comment)
        .bind(new.category)
        .bind(new.forwarded)
        .bind(&new.rule_ids)
        .bind(local_activity_id(base_url, reporter).as_deref())
        .fetch_one(&mut *tx)
        .await?;
    if !new.status_ids.is_empty() {
        let _attached = sqlx::query(ATTACH_STATUSES_SQL)
            .bind(row.id)
            .bind(&new.status_ids)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(row)
}

/// The report behind the primary key `id`, `None` when no report carries it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get(pool: &PgPool, id: Uuid) -> AppResult<Option<Report>> {
    let row = sqlx::query_as::<_, Report>(GET_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Records `ap_id` as the `Flag` activity the stored report `id` stands
/// for: the federation mirror's write (decision 6).
///
/// [`create`] leaves a remote reporter's row without an id because that id
/// is the one the reporter's own instance minted, and this is where the
/// inbound `Flag` activity's id lands on it. The unique `ap_id` index
/// refuses a second report under one id, and an unknown `id` matches no
/// row and changes nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding, an `ap_id` another row
/// already carries) arrives as [`AppError::Database`](crate::error::AppError).
pub async fn set_ap_id(pool: &PgPool, id: Uuid, ap_id: &str) -> AppResult<()> {
    sqlx::query(SET_AP_ID_SQL)
        .bind(id)
        .bind(ap_id)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The ids of the live statuses `report_id` cites, oldest status first.
///
/// What the entity's `status_ids` array and (later) a moderation view of the
/// report's context read. A report that cited nothing, or one whose every
/// cited post has since been deleted, answers the empty `Vec`; the
/// `report_statuses` rows themselves stay, because deleting a post does not
/// un-cite it for the record.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn statuses_for(pool: &PgPool, report_id: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(STATUSES_FOR_SQL)
        .bind(report_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(status_id,)| status_id).collect())
}

/// Every report, newest first and `page`'s `limit` at a time.
///
/// The moderation queue's unfiltered walk, keyed on the report row's own id
/// (decision 4). The admin listing's filters — by [`ReportState`], by
/// [`assigned_to`](Report::assigned_to), by cited rule — live in
/// [`admin_list`], which is offset-paged the way the admin API pages.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list(pool: &PgPool, page: PageParams) -> AppResult<Vec<Report>> {
    let mut builder = QueryBuilder::<Postgres>::new(LIST_SQL);
    page.apply_keyset(&mut builder, "id");
    let rows = builder.build_query_as::<Report>().fetch_all(pool).await?;
    Ok(page.finish(rows))
}

/// Moves the report `id` to `state` and returns the rewritten row, `None`
/// when no report carries that id.
///
/// [`ReportState::Resolved`] stamps [`action_taken_at`](Report::action_taken_at)
/// with the moment of the write — once, so resolving twice keeps the first
/// time and the first actor — and names `acted_by` as the moderator who took
/// the action in [`action_taken_by`](Report::action_taken_by), likewise only
/// while the column is still empty. [`ReportState::Open`] clears both again,
/// because a reopened report has had no action taken on it by anyone.
/// [`ReportState::Closed`] leaves the columns alone: Mastodon's `close!`
/// drops a report without acting on it. Every move refreshes
/// [`updated_at`](Report::updated_at), so the row states when moderation
/// last touched it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_state(
    pool: &PgPool,
    id: Uuid,
    state: ReportState,
    acted_by: Option<Uuid>,
) -> AppResult<Option<Report>> {
    let row = sqlx::query_as::<_, Report>(SET_STATE_SQL)
        .bind(id)
        .bind(state)
        .bind(acted_by)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Assigns the report `id` to `assignee` — `None` hands it back to the
/// unassigned queue — and returns the rewritten row, `None` when no report
/// carries that id.
///
/// Assignment is not a state change: the report stays in whatever
/// [`ReportState`] it was in, and only [`updated_at`](Report::updated_at)
/// moves with it. Assigning an account that no `users` row holds is refused
/// by the foreign key, so an assignment can never name nobody.
///
/// # Errors
///
/// Any database failure (connection, encoding, an assignee that does not
/// exist) arrives as [`AppError::Database`](crate::error::AppError).
pub async fn assign(pool: &PgPool, id: Uuid, assignee: Option<Uuid>) -> AppResult<Option<Report>> {
    let row = sqlx::query_as::<_, Report>(ASSIGN_SQL)
        .bind(id)
        .bind(assignee)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Every report matching `params`' filters, newest filing first, one
/// `page`/`page_size` window of them beside the total the filters select.
///
/// The admin moderation queue's walk (Pleroma's `Utils.get_reports`, whose
/// state / rule / assigned-account filters and offset window these are):
/// the ordering and the keyset are the ones [`list`] already states, with
/// the offset paging the admin listings use on top.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn admin_list(pool: &PgPool, params: &AdminListParams) -> AppResult<AdminListPage> {
    let total = count_admin_matching(pool, params).await?;
    let offset = (params.page - 1).saturating_mul(params.page_size);
    let mut builder = QueryBuilder::<Postgres>::new(ADMIN_SELECT_SQL);
    push_admin_filters(&mut builder, params);
    let _window: &mut _ = builder
        .push(" ORDER BY id DESC LIMIT ")
        .push_bind(params.page_size)
        .push(" OFFSET ")
        .push_bind(offset);
    let reports = builder.build_query_as::<Report>().fetch_all(pool).await?;
    Ok(AdminListPage { reports, total })
}

/// The number of rows `params`' filters select, whatever page they land on.
async fn count_admin_matching(pool: &PgPool, params: &AdminListParams) -> AppResult<i64> {
    let mut builder = QueryBuilder::<Postgres>::new(ADMIN_COUNT_SQL);
    push_admin_filters(&mut builder, params);
    let total = builder.build_query_scalar::<i64>().fetch_one(pool).await?;
    Ok(total)
}

/// Appends the shared `WHERE` clause of [`admin_list`] and
/// [`count_admin_matching`]: the state on its column, the rule as a `@>`
/// containment on the [`rule_ids`](Report::rule_ids) array (Pleroma's
/// `selected_rule_ids && [rule_id]` intersection), and the assignee on
/// [`assigned_to`](Report::assigned_to). The always-true predicate leads so
/// every filter is one more `AND`.
fn push_admin_filters(builder: &mut QueryBuilder<Postgres>, params: &AdminListParams) {
    let _where: &mut _ = builder.push(" WHERE true");
    if let Some(state) = params.state {
        let _state: &mut _ = builder.push(" AND state = ").push_bind(state);
    }
    if let Some(rule_id) = params.rule_id {
        let _rule: &mut _ = builder.push(" AND rule_ids @> ").push_bind(vec![rule_id]);
    }
    if let Some(assigned) = params.assigned_account {
        let _assignee: &mut _ = builder.push(" AND assigned_to = ").push_bind(assigned);
    }
}

/// Every report `params`' filters select, newest filing first and `page`'s
/// `limit` at a time — the walk behind `GET /api/v1/admin/reports`.
///
/// The keyset is [`list`]'s, on the report row's own id (decision 4), which
/// is what the Mastodon admin listing's `Link` header keys its cursors on;
/// the filters are the ticket's reading of Mastodon's
/// `GET /api/v1/admin/reports` parameters:
///
/// - `resolved` is a state pair, not a state: `Some(true)` keeps every
///   acted-on report (`resolved` and `closed` — the entity's `action_taken`
///   is that same pair read off the state), and `Some(false)` and `None`
///   keep the open queue. An unfiltered request therefore lists what still
///   needs action, which is Pleroma's `restrict_state` fall-through too.
/// - `account_id` names the reporter, `target_account_id` the reported:
///   both are this schema's `UUIDv7` text, validated by the caller.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mastodon_list(
    pool: &PgPool,
    params: &MastodonListParams,
    page: PageParams,
) -> AppResult<Vec<Report>> {
    let mut builder = QueryBuilder::<Postgres>::new(MASTODON_SELECT_SQL);
    push_mastodon_filters(&mut builder, params);
    page.apply_keyset(&mut builder, "id");
    let rows = builder.build_query_as::<Report>().fetch_all(pool).await?;
    Ok(page.finish(rows))
}

/// Appends [`mastodon_list`]'s `WHERE` clause to the always-true predicate
/// the statement leads with: the state filter as the pair or its complement
/// (never bare equality, since the acted-on set is two states), and the two
/// account filters on their columns.
fn push_mastodon_filters(builder: &mut QueryBuilder<Postgres>, params: &MastodonListParams) {
    if params.resolved == Some(true) {
        let _acted: &mut _ = builder.push(" AND state IN ('resolved', 'closed')");
    } else {
        let _open: &mut _ = builder.push(" AND state = 'open'");
    }
    if let Some(reporter_id) = params.reporter_id {
        let _reporter: &mut _ = builder.push(" AND reporter_id = ").push_bind(reporter_id);
    }
    if let Some(target_id) = params.target_id {
        let _target: &mut _ = builder.push(" AND target_id = ").push_bind(target_id);
    }
}

/// The ids of the live statuses the reports in `report_ids` cite, as
/// `(report_id, status_id)` pairs ordered by report and then by status.
///
/// [`statuses_for`] for a whole page in one read, with the same
/// live-status join and the same oldest-status-first order.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn statuses_for_reports(
    pool: &PgPool,
    report_ids: &[Uuid],
) -> AppResult<Vec<(Uuid, Uuid)>> {
    if report_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, (Uuid, Uuid)>(STATUSES_FOR_MANY_SQL)
        .bind(report_ids)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Writes `user`'s note on the report `report_id` and returns the stored
/// row.
///
/// Notes are staff-only (Pleroma's `ReportNote.create` is reachable only
/// through the admin API) and each writing is its own row; nothing here
/// caps the text — the endpoint's validation does.
///
/// # Errors
///
/// Any database failure (connection, encoding, a report or author that
/// names no row) arrives as [`AppError::Database`](crate::error::AppError);
/// the foreign keys are the model's guards.
pub async fn create_note(
    pool: &PgPool,
    report_id: Uuid,
    user_id: Uuid,
    content: &str,
) -> AppResult<ReportNote> {
    let row = sqlx::query_as::<_, ReportNote>(INSERT_NOTE_SQL)
        .bind(new_id())
        .bind(report_id)
        .bind(user_id)
        .bind(content)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// The notes on the report `report_id`, oldest first — the order
/// `report_notes_report_id_idx` states and the entity answers them in.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn notes_for(pool: &PgPool, report_id: Uuid) -> AppResult<Vec<ReportNote>> {
    notes_for_reports(pool, std::slice::from_ref(&report_id)).await
}

/// The notes on every report in `report_ids`, oldest first within each
/// report: [`notes_for`] for a whole page in one read, so the listing
/// renders every entity's `notes` array without a query per row.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn notes_for_reports(pool: &PgPool, report_ids: &[Uuid]) -> AppResult<Vec<ReportNote>> {
    if report_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, ReportNote>(NOTES_FOR_SQL)
        .bind(report_ids)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Deletes the note `note_id` of the report `report_id` and returns the
/// removed row, `None` when the pair names no note — a note of another
/// report is the same miss as no note at all.
///
/// Pleroma's `notes_delete` destroys by note id alone; keying the pair
/// here is what lets the 404 answer a note that is not this report's, and
/// what keeps one request from deleting across reports.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_note(
    pool: &PgPool,
    report_id: Uuid,
    note_id: Uuid,
) -> AppResult<Option<ReportNote>> {
    let row = sqlx::query_as::<_, ReportNote>(DELETE_NOTE_SQL)
        .bind(note_id)
        .bind(report_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The `ap_id` to store for a new report: a freshly minted
/// `{base_url}/activities/{uuid}` (decision 6) for a reporter this instance
/// owns, `None` for a remote one, whose own activity id the federation layer
/// records from the `Flag` activity it processed.
fn local_activity_id(base_url: &str, reporter: &User) -> Option<String> {
    reporter
        .local
        .get()
        .then(|| format!("{base_url}/activities/{}", new_id()))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            instance_rule,
            status::{self, NewStatus},
            user::{Flag, NewUser, insert, local_ap_id},
        },
        pagination::MAX_LIMIT,
        util::id::parse_id,
    };

    /// The `ActivityPub` path segment a local reporter's `ap_id` is built
    /// under, as [`create`] must write it (decision 6).
    const ACTIVITY_PREFIX: &str = "/activities/";

    /// Inserts a local, unlocked user named `nickname`.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// Inserts a mirror of `nickname` on `remote.example`: the account this
    /// instance does not own, whose `Flag` activity id is not ours to mint.
    async fn remote_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@remote.example"),
                ap_id: format!("https://remote.example/users/{nickname}"),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// A public post by `author`, for the reports that cite something.
    async fn post(pool: &PgPool, author: &User, text: &str) -> status::Status {
        status::insert(pool, TEST_BASE_URL, &NewStatus::local(author, text))
            .await
            .expect("the status fixture inserts")
    }

    /// The report content a filing has no more of than it must: no comment,
    /// no category, no flags, nothing cited.
    fn bare() -> NewReport {
        NewReport {
            comment: String::new(),
            category: Category::Other,
            forwarded: false,
            rule_ids: Vec::new(),
            status_ids: Vec::new(),
        }
    }

    /// Files `reporter`'s report about `target` and checks the row came back.
    async fn seed(pool: &PgPool, reporter: &User, target: &User) -> Report {
        create(pool, TEST_BASE_URL, reporter, target, &bare())
            .await
            .expect("the fixture report files")
    }

    /// The stored report behind `id`, which the test expects to exist.
    async fn stored(pool: &PgPool, id: Uuid) -> Report {
        get(pool, id).await.unwrap().expect("the report exists")
    }

    /// Every row of the table, newest filing first — the order a page walks,
    /// read straight from storage so same-millisecond `UUIDv7`s cannot make an
    /// expectation depend on the order the fixtures were written in.
    async fn stored_rows(pool: &PgPool) -> Vec<Report> {
        sqlx::query_as::<_, Report>("SELECT * FROM reports ORDER BY id DESC")
            .fetch_all(pool)
            .await
            .expect("the rows read")
    }

    /// The ids of `rows`, in order.
    fn ids_of(rows: &[Report]) -> Vec<Uuid> {
        rows.iter().map(|row| row.id).collect()
    }

    /// The number of rows of `reports`, which only ever holds fixture rows in
    /// these tests.
    async fn report_count(pool: &PgPool) -> i64 {
        let row: (i64,) = sqlx::query_as("SELECT count(*) FROM reports")
            .fetch_one(pool)
            .await
            .unwrap();
        row.0
    }

    /// The number of rows of `report_statuses`: the citations filed.
    async fn citation_count(pool: &PgPool) -> i64 {
        let row: (i64,) = sqlx::query_as("SELECT count(*) FROM report_statuses")
            .fetch_one(pool)
            .await
            .unwrap();
        row.0
    }

    /// Page params built from a query string, the way the extractor makes them.
    fn page(query: &str) -> PageParams {
        PageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture page parses")
    }

    /// The `{TEST_BASE_URL}/activities/{uuid}` id [`create`] minted for a
    /// local reporter, checked for its shape and reduced to the id part.
    fn activity_id(row: &Report) -> String {
        let prefix = format!("{TEST_BASE_URL}{ACTIVITY_PREFIX}");
        let ap_id = row
            .ap_id
            .clone()
            .expect("a local reporter's row carries a minted activity id");
        assert!(
            ap_id.starts_with(&prefix),
            "the activity id is this instance's, got {ap_id}"
        );
        let tail = ap_id
            .strip_prefix(&prefix)
            .expect("the prefix was just checked");
        assert!(
            parse_id(tail).is_some(),
            "the tail of the activity id is an id: {tail}"
        );
        tail.to_owned()
    }

    /// A filed report round-trips every column it was given, and the defaults
    /// the reporter left alone come back from the database.
    #[tokio::test]
    async fn a_report_is_filed_and_read_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let rule = instance_rule::create(&pool, "No harassment", "", 1)
            .await
            .expect("the rule fixture inserts");
        let cited = post(&pool, &bob, "the post being reported").await;

        let row = create(
            &pool,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewReport {
                comment: "this is harassment".to_owned(),
                category: Category::Violation,
                forwarded: true,
                rule_ids: vec![rule.id],
                status_ids: vec![cited.id],
            },
        )
        .await
        .expect("the report files");

        assert!(row.id.get_version_num() == 7, "the id is a UUIDv7");
        assert_eq!(row.reporter_id, alice.id, "who filed it");
        assert_eq!(row.target_id, bob.id, "who it is about");
        assert_eq!(row.comment, "this is harassment", "the comment verbatim");
        assert_eq!(row.category, Category::Violation, "the cited category");
        assert!(row.forwarded, "the forward request");
        assert_eq!(row.state, ReportState::Open, "a fresh report is open");
        assert_eq!(row.assigned_to, None, "and unassigned");
        assert_eq!(row.action_taken_at, None, "with nothing done about it");
        assert_eq!(row.rule_ids, vec![rule.id], "the cited rule");
        assert!(row.ap_id.is_some(), "a local reporter mints a Flag id");
        assert!(
            parse_id(&activity_id(&row)).is_some(),
            "the minted id is a UUID"
        );
        assert!(row.inserted_at <= Utc::now(), "the database stamps it");
        assert_eq!(
            stored(&pool, row.id).await,
            row,
            "get() reads back the row it wrote"
        );
        assert_eq!(
            statuses_for(&pool, row.id).await.unwrap(),
            vec![cited.id],
            "the cited status is attached"
        );

        let listed = list(&pool, page("")).await.unwrap();
        assert_eq!(ids_of(&listed), vec![row.id], "the listing holds it");
    }

    /// A filing names its reporter, its target and nothing else: the defaults
    /// the reporter left alone come back from the database.
    #[tokio::test]
    async fn a_bare_filing_takes_every_column_default() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;

        let row = seed(&pool, &alice, &bob).await;

        assert_eq!(row.reporter_id, alice.id, "who filed it");
        assert_eq!(row.target_id, bob.id, "who it is about");
        assert_eq!(row.comment, "", "no comment is the empty comment");
        assert_eq!(row.category, Category::Other, "the default category");
        assert!(!row.forwarded, "reports are not forwarded by default");
        assert_eq!(row.state, ReportState::Open, "a fresh report is open");
        assert_eq!(row.assigned_to, None, "and nobody owns it yet");
        assert_eq!(row.action_taken_at, None, "with nothing done about it");
        assert!(row.rule_ids.is_empty(), "no rule cited is no rules");
        assert!(
            row.inserted_at <= Utc::now() && row.updated_at <= Utc::now(),
            "both timestamps are stamped"
        );
        assert!(
            statuses_for(&pool, row.id).await.unwrap().is_empty(),
            "and no status attached"
        );
        assert_eq!(citation_count(&pool).await, 0);
    }

    #[tokio::test]
    async fn a_remote_reporter_mints_no_activity_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = remote_user(&pool, "carol").await;
        let alice = local_user(&pool, "alice").await;

        let row = seed(&pool, &carol, &alice).await;

        assert!(
            row.ap_id.is_none(),
            "a remote reporter's id is the one its own instance minted"
        );
        assert!(
            stored(&pool, row.id).await.ap_id.is_none(),
            "and nothing was stored in its place"
        );
    }

    /// The federation mirror's write: [`set_ap_id`] lands the delivered
    /// `Flag`'s id on the row [`create`] left bare, keeps the row itself,
    /// and is refused by the unique index when the id already stands.
    #[tokio::test]
    async fn set_ap_id_lands_the_flag_id_on_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = remote_user(&pool, "carol").await;
        let alice = local_user(&pool, "alice").await;
        let bob = remote_user(&pool, "bob").await;
        let row = seed(&pool, &carol, &alice).await;
        let flag = "https://remote.example/activities/flag-42";

        set_ap_id(&pool, row.id, flag).await.unwrap();
        let mirrored = stored(&pool, row.id).await;
        assert_eq!(mirrored.ap_id.as_deref(), Some(flag), "the id landed");
        assert_eq!(mirrored.id, row.id, "the row itself did not move");
        assert_eq!(
            mirrored.reporter_id, row.reporter_id,
            "and nothing else did"
        );

        set_ap_id(&pool, row.id, flag)
            .await
            .expect("restating the same id is no error");

        let second = seed(&pool, &carol, &bob).await;
        let taken = set_ap_id(&pool, second.id, flag)
            .await
            .expect_err("one Flag ids one report");
        assert!(
            matches!(taken, AppError::Database(_)),
            "the unique index is the guard: {taken}"
        );

        set_ap_id(&pool, new_id(), "https://remote.example/activities/orphan")
            .await
            .expect("an unknown report id changes nothing and is no error");
    }

    /// Filing is not idempotent on the pair: a second report is a second row
    /// with a second id and a second `Flag` id (Mastodon allows repeat
    /// reports, and so does Pleroma's `Flag`).
    #[tokio::test]
    async fn the_same_pair_may_report_twice() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;

        let first = seed(&pool, &alice, &bob).await;
        let second = seed(&pool, &alice, &bob).await;

        assert_ne!(first.id, second.id, "two filings, two ids");
        assert_ne!(first.ap_id, second.ap_id, "and two activity ids");
        assert_eq!(report_count(&pool).await, 2, "both are stored");
        assert_eq!(
            ids_of(&list(&pool, page("")).await.unwrap()),
            vec![second.id, first.id],
            "newest filing first"
        );
    }

    /// A status cited twice in one filing is one row, and the context reads
    /// back in the statuses' own creation order rather than the request's.
    #[tokio::test]
    async fn citing_one_status_twice_attaches_it_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let first = post(&pool, &bob, "first").await;
        let second = post(&pool, &bob, "second").await;

        let row = create(
            &pool,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewReport {
                status_ids: vec![second.id, first.id, second.id],
                ..bare()
            },
        )
        .await
        .expect("the repeat citation files");

        assert_eq!(
            statuses_for(&pool, row.id).await.unwrap(),
            vec![first.id, second.id],
            "oldest status first, each cited once"
        );
        assert_eq!(citation_count(&pool).await, 2);
    }

    /// A cited status that names no row is the foreign key's refusal, and the
    /// transaction's: the report row goes with it, so no report is ever stored
    /// with half its context.
    #[tokio::test]
    async fn citing_an_unknown_status_files_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;

        let err = create(
            &pool,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewReport {
                status_ids: vec![Uuid::now_v7()],
                ..bare()
            },
        )
        .await
        .expect_err("the foreign key refuses");
        assert!(
            matches!(err, AppError::Database(_)),
            "a constraint failure is the database variant: {err}"
        );
        assert_eq!(report_count(&pool).await, 0, "the row rolled back");
        assert_eq!(citation_count(&pool).await, 0);
    }

    #[tokio::test]
    async fn reporting_yourself_is_refused() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;

        let err = create(&pool, TEST_BASE_URL, &alice, &alice, &bare())
            .await
            .expect_err("nobody reports themselves");
        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "a self-report is the 422 the endpoint renders: {err}"
        );
        assert_eq!(err.to_string(), "Can not report yourself", "its wording");
        assert_eq!(report_count(&pool).await, 0, "and it stored nothing");
    }

    #[tokio::test]
    async fn getting_an_unknown_report_is_no_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        assert_eq!(
            get(&pool, Uuid::now_v7()).await.unwrap(),
            None,
            "an id no report carries is the None answer, not an error"
        );
        assert!(
            statuses_for(&pool, Uuid::now_v7())
                .await
                .unwrap()
                .is_empty(),
            "and its context is the empty list"
        );
        assert!(
            list(&pool, page("")).await.unwrap().is_empty(),
            "a fresh table lists nothing"
        );
    }

    /// [`set_state`] moves the state and writes the `action_taken_at` and
    /// `action_taken_by` the Mastodon admin entity reads: resolution stamps
    /// them once, reopening clears both, closing leaves them alone.
    #[tokio::test]
    async fn states_are_written_and_resolution_stamps_the_action() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let root = local_user(&pool, "root").await;
        let other = local_user(&pool, "other").await;
        let row = seed(&pool, &alice, &bob).await;
        assert_eq!(
            row.action_taken_by, None,
            "a fresh report names no acting moderator"
        );

        let closed = set_state(&pool, row.id, ReportState::Closed, Some(root.id))
            .await
            .unwrap()
            .expect("the report exists");
        assert_eq!(closed.state, ReportState::Closed, "the new state");
        assert_eq!(
            closed.action_taken_at, None,
            "closing takes no action, so nothing is stamped"
        );
        assert_eq!(
            closed.action_taken_by, None,
            "and nobody is named for an action closing did not take"
        );
        assert_eq!(closed.comment, row.comment, "the rest is untouched");

        let resolved = set_state(&pool, row.id, ReportState::Resolved, Some(root.id))
            .await
            .unwrap()
            .expect("the report still exists");
        assert_eq!(resolved.state, ReportState::Resolved, "the new state");
        let acted = resolved
            .action_taken_at
            .expect("resolution stamps the action");
        assert!(acted <= Utc::now(), "and the stamp is now or older");
        assert_eq!(
            resolved.action_taken_by,
            Some(root.id),
            "and names the acting moderator"
        );
        assert!(
            resolved.updated_at >= row.updated_at,
            "the row restamps its updated_at"
        );

        let again = set_state(&pool, row.id, ReportState::Resolved, Some(other.id))
            .await
            .unwrap()
            .expect("the report still exists");
        assert_eq!(
            again.action_taken_at, resolved.action_taken_at,
            "resolving twice keeps the first stamp"
        );
        assert_eq!(
            again.action_taken_by, resolved.action_taken_by,
            "and the first actor with it"
        );

        let reopened = set_state(&pool, row.id, ReportState::Open, None)
            .await
            .unwrap()
            .expect("the report still exists");
        assert_eq!(reopened.state, ReportState::Open, "back to open");
        assert_eq!(
            reopened.action_taken_at, None,
            "and the action it no longer stands on is cleared"
        );
        assert_eq!(
            reopened.action_taken_by, None,
            "and with it the moderator that action named"
        );
    }

    #[tokio::test]
    async fn writing_state_or_assignment_reports_the_miss() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        assert_eq!(
            set_state(&pool, Uuid::now_v7(), ReportState::Resolved, None)
                .await
                .unwrap(),
            None,
            "an unknown id updates no state"
        );
        assert_eq!(
            assign(&pool, Uuid::now_v7(), Some(Uuid::now_v7()))
                .await
                .unwrap(),
            None,
            "and no assignment"
        );
        assert_eq!(report_count(&pool).await, 0);
    }

    /// [`assign`] names the moderator who owns the report and `None` hands it
    /// back; the state is not part of it.
    #[tokio::test]
    async fn a_report_is_assigned_and_unassigned() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let moddie = local_user(&pool, "moddie").await;
        let row = seed(&pool, &alice, &bob).await;

        let assigned = assign(&pool, row.id, Some(moddie.id))
            .await
            .unwrap()
            .expect("the report exists");
        assert_eq!(assigned.assigned_to, Some(moddie.id), "the assignee");
        assert_eq!(assigned.state, ReportState::Open, "the state is untouched");
        assert_eq!(assigned.ap_id, row.ap_id, "and so is the activity id");

        let back = assign(&pool, row.id, None)
            .await
            .unwrap()
            .expect("the report exists");
        assert_eq!(
            back.assigned_to, None,
            "handing it back leaves it unassigned"
        );

        let orphan = assign(&pool, row.id, Some(Uuid::now_v7()))
            .await
            .expect_err("an assignee that does not exist is refused");
        assert!(
            matches!(orphan, AppError::Database(_)),
            "the foreign key is the model's guard: {orphan}"
        );
    }

    /// Deleting either account takes its reports through the cascade, from
    /// both ends; an assignment does not keep the reporter's account alive,
    /// and a cited status row goes with the report.
    #[tokio::test]
    async fn deleting_either_side_takes_the_report() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let cited = post(&pool, &bob, "reported post").await;
        let by_alice = create(
            &pool,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewReport {
                status_ids: vec![cited.id],
                ..bare()
            },
        )
        .await
        .expect("the fixture report files");
        let by_carol = seed(&pool, &carol, &alice).await;

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the reporter deletes");
        assert_eq!(deleted.rows_affected(), 1, "one account went");
        assert!(
            get(&pool, by_alice.id).await.unwrap().is_none(),
            "alice's own report went with her as its reporter"
        );
        assert!(
            get(&pool, by_carol.id).await.unwrap().is_none(),
            "and carol's report about her went with her as its target"
        );
        assert_eq!(
            citation_count(&pool).await,
            0,
            "and the status context went with the report"
        );

        let about_bob = seed(&pool, &carol, &bob).await;
        let gone = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the target deletes");
        assert_eq!(gone.rows_affected(), 1, "and so did the reported account");
        assert!(
            get(&pool, about_bob.id).await.unwrap().is_none(),
            "the report about it cascaded from the target end too"
        );
        assert!(
            list(&pool, page("")).await.unwrap().is_empty(),
            "and the table holds nothing of the deleted accounts"
        );
    }

    /// An assignment is a `users` reference without a delete rule, which is
    /// what the migration states: the assignee cannot go away while a report
    /// still names them.
    #[tokio::test]
    async fn an_assignee_cannot_be_deleted_under_its_report() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let moddie = local_user(&pool, "moddie").await;
        let row = seed(&pool, &alice, &bob).await;
        let assigned = assign(&pool, row.id, Some(moddie.id))
            .await
            .unwrap()
            .expect("the report exists");
        assert_eq!(
            assigned.assigned_to,
            Some(moddie.id),
            "the write answers the assignment it made"
        );

        let err = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(moddie.id)
            .execute(&pool)
            .await
            .expect_err("the assignment holds its assignee");
        assert!(
            matches!(AppError::from(err), AppError::Database(_)),
            "deleting an assignee is refused by the foreign key"
        );
        assert_eq!(
            stored(&pool, row.id).await.assigned_to,
            Some(moddie.id),
            "and the assignment stands"
        );

        let released = assign(&pool, row.id, None)
            .await
            .unwrap()
            .expect("the report exists");
        assert_eq!(
            released.assigned_to, None,
            "the write answers the release it made"
        );
        let free = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(moddie.id)
            .execute(&pool)
            .await
            .expect("an unassigned moderator deletes");
        assert_eq!(free.rows_affected(), 1, "once released, the account goes");
    }

    /// The moderation queue walks newest filing first and by report id, the
    /// cursors [`PageParams`] speaks.
    #[tokio::test]
    async fn a_page_of_reports_newest_first_walks_by_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        for nickname in ["bob", "carol", "dave"] {
            let target = local_user(&pool, nickname).await;
            let _filing = seed(&pool, &alice, &target).await;
        }
        let stored = stored_rows(&pool).await;
        assert_eq!(stored.len(), 3, "three fixture reports");

        let first = list(&pool, page("limit=2")).await.unwrap();
        assert_eq!(
            ids_of(&first),
            vec![stored[0].id, stored[1].id],
            "the two newest filings"
        );

        let older = list(&pool, page(&format!("max_id={}", stored[1].id)))
            .await
            .unwrap();
        assert_eq!(
            ids_of(&older),
            vec![stored[2].id],
            "max_id continues where the page ended"
        );

        let newer = list(&pool, page(&format!("min_id={}", stored[2].id)))
            .await
            .unwrap();
        assert_eq!(
            ids_of(&newer),
            vec![stored[0].id, stored[1].id],
            "a min_id page is answered newest-first too"
        );

        let tail = list(&pool, page(&format!("max_id={}", stored[2].id)))
            .await
            .unwrap();
        assert!(tail.is_empty(), "walking off the end is no error");
    }

    /// The two enums and the database agree on spellings: what [`as_str`]
    /// states is what the column holds, what [`FromStr`] accepts is exactly
    /// the `CHECK` constraint's set, and sqlx stores the same text.
    #[tokio::test]
    async fn the_text_enums_are_the_columns_own_set() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;

        for category in Category::ALL {
            assert_eq!(
                category.to_string(),
                category.as_str(),
                "Display is the stored text"
            );
            let parsed: Category = category
                .as_str()
                .parse()
                .expect("every stored name parses back");
            assert_eq!(parsed, category, "and comes back the same variant");

            let row = create(
                &pool,
                TEST_BASE_URL,
                &alice,
                &bob,
                &NewReport { category, ..bare() },
            )
            .await
            .expect("the fixture report files");
            assert_eq!(
                stored(&pool, row.id).await.category,
                category,
                "`{}` round-trips through the column",
                category.as_str()
            );
        }
        assert_eq!(
            report_count(&pool).await,
            i64::try_from(Category::ALL.len()).expect("four categories fit"),
            "one filing per category"
        );

        for state in ReportState::ALL {
            assert_eq!(state.to_string(), state.as_str(), "Display is text");
            let parsed: ReportState = state
                .as_str()
                .parse()
                .expect("every stored name parses back");
            assert_eq!(parsed, state, "and comes back the same variant");
        }

        for unknown in ["", "SPAM", "Violation", "whatever", "spam "] {
            assert!(
                unknown.parse::<Category>().is_err(),
                "`{unknown}` is not a category"
            );
        }
        for unknown in ["", "OPEN", "fixed", "other"] {
            assert!(
                unknown.parse::<ReportState>().is_err(),
                "`{unknown}` is not a state"
            );
        }
    }

    /// A deactivated reporter's filing is still a row: `is_active` is the
    /// endpoint's question, not this table's, and the foreign key only needs
    /// the account to exist.
    #[tokio::test]
    async fn a_deactivated_account_may_still_be_a_reporter() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dormant = insert(
            &pool,
            &NewUser {
                is_active: Flag::new(false),
                ..NewUser::local("dormant", &local_ap_id(TEST_BASE_URL, "dormant"))
            },
        )
        .await
        .expect("the deactivated fixture inserts");
        let bob = local_user(&pool, "bob").await;

        let row = seed(&pool, &dormant, &bob).await;
        assert_eq!(row.reporter_id, dormant.id, "the filing is stored");
    }

    /// A cited post that is deleted stops being part of the report's context
    /// while the report itself stands: [`statuses_for`] reads live rows.
    #[tokio::test]
    async fn a_deleted_status_leaves_the_context_it_was_cited_in() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let kept = post(&pool, &bob, "still here").await;
        let doomed = post(&pool, &bob, "about to go").await;
        let row = create(
            &pool,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewReport {
                status_ids: vec![kept.id, doomed.id],
                ..bare()
            },
        )
        .await
        .expect("the fixture report files");
        assert_eq!(statuses_for(&pool, row.id).await.unwrap().len(), 2);

        let tombstone = sqlx::query("UPDATE statuses SET deleted_at = now() WHERE id = $1")
            .bind(doomed.id)
            .execute(&pool)
            .await
            .expect("the fixture deletes the post");
        assert_eq!(tombstone.rows_affected(), 1, "one post went");

        assert_eq!(
            statuses_for(&pool, row.id).await.unwrap(),
            vec![kept.id],
            "the surviving citation is the answer"
        );
        assert_eq!(
            citation_count(&pool).await,
            2,
            "the record of what was cited stays"
        );
    }

    /// The listing params with no filter: [`admin_list`]'s plain walk of
    /// the whole queue.
    fn listing() -> AdminListParams {
        AdminListParams {
            state: None,
            rule_id: None,
            assigned_account: None,
            page: 1,
            page_size: 20,
        }
    }

    /// The report ids [`admin_list`] answers for `params`, in order.
    async fn listed_ids(pool: &PgPool, params: &AdminListParams) -> Vec<Uuid> {
        let page = admin_list(pool, params).await.expect("the listing runs");
        ids_of(&page.reports)
    }

    /// [`admin_list`] walks newest filing first and windows by `page` /
    /// `page_size` beside the unwindowed total; an off-the-end page is
    /// empty without shrinking the count.
    #[tokio::test]
    async fn the_admin_listing_windows_the_queue() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        for nickname in ["carol", "dave", "erin"] {
            let target = local_user(&pool, nickname).await;
            let _filing = seed(&pool, &alice, &target).await;
        }
        let _last = seed(&pool, &bob, &alice).await;
        let stored: Vec<Uuid> =
            sqlx::query_as::<_, (Uuid,)>("SELECT id FROM reports ORDER BY id DESC")
                .fetch_all(&pool)
                .await
                .expect("the stored ids read")
                .into_iter()
                .map(|(id,)| id)
                .collect();
        let all = listing();

        let page = admin_list(&pool, &all).await.unwrap();
        assert_eq!(page.total, 4, "the whole table");
        assert_eq!(ids_of(&page.reports), stored, "newest filing first");

        let window = AdminListParams {
            page_size: 2,
            ..all
        };
        assert_eq!(
            listed_ids(&pool, &window).await,
            stored[..2],
            "the first two"
        );

        let second = AdminListParams {
            page: 2,
            page_size: 2,
            ..all
        };
        assert_eq!(
            listed_ids(&pool, &second).await,
            stored[2..],
            "the next page walks the window"
        );

        let past = AdminListParams {
            page: 3,
            page_size: 2,
            ..all
        };
        assert!(listed_ids(&pool, &past).await.is_empty(), "off the end");
        let counted = admin_list(&pool, &past).await.unwrap();
        assert_eq!(counted.total, 4, "still counting all");
    }

    /// [`admin_list`]'s three filters each keep exactly their own
    /// reports: the state on the `state` column, the cited rule on the
    /// `rule_ids` containment, the assignee on the `assigned_to` column.
    #[tokio::test]
    async fn the_admin_listing_filters_by_state_rule_and_assignee() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let moddie = local_user(&pool, "moddie").await;
        let rule = instance_rule::create(&pool, "No harassment", "", 1)
            .await
            .expect("the rule fixture inserts");
        let first = seed(&pool, &alice, &bob).await;
        let citing = create(
            &pool,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewReport {
                rule_ids: vec![rule.id],
                ..bare()
            },
        )
        .await
        .expect("the citing report files");
        let closed = seed(&pool, &alice, &bob).await;
        let _moved = set_state(&pool, closed.id, ReportState::Closed, None)
            .await
            .unwrap()
            .expect("the report exists");
        let _assigned = assign(&pool, first.id, Some(moddie.id))
            .await
            .unwrap()
            .expect("the report exists");
        let all = listing();

        let by_state = AdminListParams {
            state: Some(ReportState::Closed),
            ..all
        };
        assert_eq!(
            listed_ids(&pool, &by_state).await,
            vec![closed.id],
            "only the closed filing"
        );
        let by_rule = AdminListParams {
            rule_id: Some(rule.id),
            ..all
        };
        assert_eq!(
            listed_ids(&pool, &by_rule).await,
            vec![citing.id],
            "only the report citing the rule"
        );
        let by_assignee = AdminListParams {
            assigned_account: Some(moddie.id),
            ..all
        };
        assert_eq!(
            listed_ids(&pool, &by_assignee).await,
            vec![first.id],
            "only the assigned report"
        );
    }

    /// The [`mastodon_list`] params with nothing but the state filter set:
    /// the account filters are the tests' to add.
    fn mastodon_listing(resolved: Option<bool>) -> MastodonListParams {
        MastodonListParams {
            resolved,
            reporter_id: None,
            target_id: None,
        }
    }

    /// The report ids [`mastodon_list`] answers for `params` on `page`, in
    /// order.
    async fn mastodon_ids(
        pool: &PgPool,
        params: &MastodonListParams,
        page: PageParams,
    ) -> Vec<Uuid> {
        ids_of(
            &mastodon_list(pool, params, page)
                .await
                .expect("the listing runs"),
        )
    }

    /// The four filings the [`mastodon_list`] tests filter and page: every
    /// state and both accounts represented, newest filing first as the
    /// listing answers them — carol's closed one about alice, alice's
    /// resolved one about carol (acted on by root), bob's open one about
    /// carol, and the oldest filing, alice's open one about bob.
    async fn mastodon_seed(pool: &PgPool) -> (Report, Report, Report, Report) {
        let alice = local_user(pool, "alice").await;
        let bob = local_user(pool, "bob").await;
        let carol = local_user(pool, "carol").await;
        let root = local_user(pool, "root").await;
        let open_by_alice = seed(pool, &alice, &bob).await;
        let open_by_bob = seed(pool, &bob, &carol).await;
        let resolved = seed(pool, &alice, &carol).await;
        let _acted = set_state(pool, resolved.id, ReportState::Resolved, Some(root.id))
            .await
            .unwrap()
            .expect("the report exists");
        let closed = seed(pool, &carol, &alice).await;
        let _moved = set_state(pool, closed.id, ReportState::Closed, None)
            .await
            .unwrap()
            .expect("the report exists");
        (closed, resolved, open_by_bob, open_by_alice)
    }

    /// [`mastodon_list`]'s state filter is the ticket's pair, not a state:
    /// `resolved=true` keeps both acted-on states, and `resolved=false`,
    /// its absence and every junk value keep the open queue.
    #[tokio::test]
    async fn the_mastodon_admin_listing_selects_the_resolved_pair() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (closed, resolved, open_by_bob, open_by_alice) = mastodon_seed(&pool).await;

        let open = mastodon_ids(&pool, &mastodon_listing(None), page("")).await;
        assert_eq!(
            open,
            vec![open_by_bob.id, open_by_alice.id],
            "the unfiltered queue is the open pair, newest first"
        );
        let explicit_open = mastodon_ids(&pool, &mastodon_listing(Some(false)), page("")).await;
        assert_eq!(
            explicit_open, open,
            "resolved=false is the same queue as absence"
        );
        let acted = mastodon_ids(&pool, &mastodon_listing(Some(true)), page("")).await;
        assert_eq!(
            acted,
            vec![closed.id, resolved.id],
            "resolved=true keeps both acted-on states"
        );
    }

    /// The two account filters keep their own reporter's and their own
    /// target's filings, they stack with the state filter, an id that
    /// names no account matches no row, and the keyset pages the filtered
    /// walk rather than the whole table.
    #[tokio::test]
    async fn the_mastodon_admin_listing_filters_accounts_and_pages() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (closed, resolved, open_by_bob, open_by_alice) = mastodon_seed(&pool).await;
        let alice = open_by_alice.reporter_id;
        let carol = open_by_bob.target_id;

        let by_reporter = MastodonListParams {
            reporter_id: Some(alice),
            ..mastodon_listing(None)
        };
        assert_eq!(
            mastodon_ids(&pool, &by_reporter, page("")).await,
            vec![open_by_alice.id],
            "the reporter filter keeps alice's open filing, not her resolved one"
        );
        let acted_by_alice = MastodonListParams {
            reporter_id: Some(alice),
            ..mastodon_listing(Some(true))
        };
        assert_eq!(
            mastodon_ids(&pool, &acted_by_alice, page("")).await,
            vec![resolved.id],
            "the two filters stack"
        );
        let by_target = MastodonListParams {
            target_id: Some(carol),
            ..mastodon_listing(None)
        };
        assert_eq!(
            mastodon_ids(&pool, &by_target, page("")).await,
            vec![open_by_bob.id],
            "the target filter keeps reports about that account"
        );
        let nobody = MastodonListParams {
            target_id: Some(Uuid::now_v7()),
            ..mastodon_listing(None)
        };
        assert!(
            mastodon_ids(&pool, &nobody, page("")).await.is_empty(),
            "an id that names no account matches no row"
        );

        let paged = mastodon_ids(&pool, &mastodon_listing(Some(true)), page("limit=1")).await;
        assert_eq!(paged, vec![closed.id], "the window takes the newest match");
        let next = mastodon_ids(
            &pool,
            &mastodon_listing(Some(true)),
            page(&format!("limit=1&max_id={}", closed.id)),
        )
        .await;
        assert_eq!(
            next,
            vec![resolved.id],
            "and the cursor continues the filtered walk"
        );
    }

    /// The batched citation read answers the same live-only, oldest-first
    /// context as [`statuses_for`], keyed per report, and an empty id list
    /// reads nothing.
    #[tokio::test]
    async fn the_batched_citation_read_groups_the_live_statuses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let kept = post(&pool, &bob, "kept").await;
        let doomed = post(&pool, &bob, "doomed").await;
        let citing = create(
            &pool,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewReport {
                status_ids: vec![doomed.id, kept.id],
                ..bare()
            },
        )
        .await
        .expect("the fixture report files");
        let plain = seed(&pool, &carol, &alice).await;

        let pairs = statuses_for_reports(&pool, &[citing.id, plain.id])
            .await
            .unwrap();
        assert_eq!(
            pairs,
            vec![(citing.id, kept.id), (citing.id, doomed.id)],
            "the citing report's pair, oldest status first"
        );

        let _gone = sqlx::query("UPDATE statuses SET deleted_at = now() WHERE id = $1")
            .bind(doomed.id)
            .execute(&pool)
            .await
            .expect("the fixture deletes the post");
        let after = statuses_for_reports(&pool, &[citing.id, plain.id])
            .await
            .unwrap();
        assert_eq!(
            after,
            vec![(citing.id, kept.id)],
            "the deleted post leaves the batched answer too"
        );
        assert!(
            statuses_for_reports(&pool, &[]).await.unwrap().is_empty(),
            "no ids read nothing"
        );
    }

    /// Notes round-trip: [`create_note`] stamps the author and the text,
    /// [`notes_for`] reads them back oldest first per report, the batched
    /// read groups the same rows, and [`delete_note`] answers the removed
    /// row — only for its own report's pair.
    #[tokio::test]
    async fn notes_are_written_read_and_deleted_per_report() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let moddie = local_user(&pool, "moddie").await;
        let one = seed(&pool, &alice, &bob).await;
        let two = seed(&pool, &bob, &alice).await;

        let first = create_note(&pool, one.id, moddie.id, "looked")
            .await
            .expect("the first note writes");
        let second = create_note(&pool, one.id, moddie.id, "again")
            .await
            .expect("the second note writes");
        let _other = create_note(&pool, two.id, alice.id, "elsewhere")
            .await
            .expect("the foreign note writes");
        assert_eq!(first.id.get_version_num(), 7, "the note id is a UUIDv7");
        assert_eq!(first.report_id, one.id, "it names its report");
        assert_eq!(first.user_id, Some(moddie.id), "and its author");
        assert_eq!(first.content, "looked", "the text verbatim");

        let notes = notes_for(&pool, one.id).await.unwrap();
        assert_eq!(
            notes.iter().map(|note| note.id).collect::<Vec<Uuid>>(),
            vec![first.id, second.id],
            "oldest first"
        );
        let batched = notes_for_reports(&pool, &[one.id, two.id]).await.unwrap();
        assert_eq!(batched.len(), 3, "every note of both reports");
        assert!(
            notes_for_reports(&pool, &[]).await.unwrap().is_empty(),
            "no ids read nothing"
        );

        assert!(
            delete_note(&pool, two.id, first.id)
                .await
                .unwrap()
                .is_none(),
            "the foreign pair deletes nothing"
        );
        let removed = delete_note(&pool, one.id, first.id)
            .await
            .unwrap()
            .expect("the pair names the note");
        assert_eq!(removed, first, "the removed row comes back");
        assert_eq!(
            notes_for(&pool, one.id).await.unwrap().len(),
            1,
            "and it is gone"
        );
        assert!(
            delete_note(&pool, one.id, Uuid::now_v7())
                .await
                .unwrap()
                .is_none(),
            "an unknown note id is the same miss"
        );
    }

    /// A note's author and report are the model's guards: a report that
    /// names nothing is the foreign key's refusal, and deleting the
    /// report takes its notes with it.
    #[tokio::test]
    async fn a_note_names_a_real_report_and_cascades_with_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let row = seed(&pool, &alice, &bob).await;
        let _note = create_note(&pool, row.id, alice.id, "kept with the report")
            .await
            .expect("the note writes");

        let orphan = create_note(&pool, Uuid::now_v7(), alice.id, "nowhere")
            .await
            .expect_err("a report that names nothing is refused");
        assert!(
            matches!(orphan, AppError::Database(_)),
            "the foreign key is the guard: {orphan}"
        );

        let gone = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the target deletes");
        assert_eq!(gone.rows_affected(), 1, "the account goes");
        assert!(
            notes_for(&pool, row.id).await.unwrap().is_empty(),
            "and its report's notes cascaded away with it"
        );
    }
}
