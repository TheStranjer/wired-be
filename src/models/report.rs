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
//! belongs to the admin tickets that read and write it; nothing here touches
//! it.
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
//! [`ReportState::Resolved`] report was acted on and clears it again when a
//! report is reopened; the second names (or un-names) the moderator who owns
//! it. Both answer the rewritten row, `None` when no report carries the id.
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
/// (decision 6). `state`, `assigned_to`, `action_taken_at` and the two
/// timestamps are left to the column defaults: a filing opens the report,
/// assigns nobody and acts on nothing. `RETURNING *` brings the row back with
/// those stamps.
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
/// cursors to it, and because that is where the admin tickets' filters —
/// state, assignee, target, time window — will go.
const LIST_SQL: &str = "SELECT * FROM reports WHERE true";

/// [`set_state`]: the state, the `action_taken_at` that belongs with it, and
/// the `updated_at` bump. Resolution is the action the flag reports, so
/// moving to `resolved` stamps the moment once (a repeat keeps the first
/// stamp); reopening a report clears it, so `action_taken` never claims an
/// action a reopened report no longer stands on; closing takes no action
/// (Mastodon's `close!` leaves `action_taken` false) and leaves the column
/// as it was. No `WHERE`-clause guard on a missing row: [`set_state`]
/// reports the miss as `None`.
const SET_STATE_SQL: &str = "UPDATE reports
      SET state = $2,
          action_taken_at = CASE
              WHEN $2 = 'resolved' THEN COALESCE(action_taken_at, now())
              WHEN $2 = 'open' THEN NULL
              ELSE action_taken_at
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
/// (decision 4). The admin tickets add its filters — by [`ReportState`], by
/// [`assigned_to`](Report::assigned_to), by target and by time window — to the
/// statement's `WHERE` clause; the paging and the newest-first order are
/// already the ones those endpoints need.
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
/// time — and [`ReportState::Open`] clears it again, because a reopened report
/// has not had action taken on it. [`ReportState::Closed`] leaves the column
/// alone: Mastodon's `close!` drops a report without acting on it. Every move
/// refreshes [`updated_at`](Report::updated_at), so the row states when
/// moderation last touched it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_state(pool: &PgPool, id: Uuid, state: ReportState) -> AppResult<Option<Report>> {
    let row = sqlx::query_as::<_, Report>(SET_STATE_SQL)
        .bind(id)
        .bind(state)
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

    /// [`set_state`] moves the state and writes the `action_taken_at` the
    /// entity's `action_taken` reads: resolution stamps it once, reopening
    /// clears it, closing leaves it alone.
    #[tokio::test]
    async fn states_are_written_and_resolution_stamps_the_action() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let row = seed(&pool, &alice, &bob).await;

        let closed = set_state(&pool, row.id, ReportState::Closed)
            .await
            .unwrap()
            .expect("the report exists");
        assert_eq!(closed.state, ReportState::Closed, "the new state");
        assert_eq!(
            closed.action_taken_at, None,
            "closing takes no action, so nothing is stamped"
        );
        assert_eq!(closed.comment, row.comment, "the rest is untouched");

        let resolved = set_state(&pool, row.id, ReportState::Resolved)
            .await
            .unwrap()
            .expect("the report still exists");
        assert_eq!(resolved.state, ReportState::Resolved, "the new state");
        let acted = resolved
            .action_taken_at
            .expect("resolution stamps the action");
        assert!(acted <= Utc::now(), "and the stamp is now or older");
        assert!(
            resolved.updated_at >= row.updated_at,
            "the row restamps its updated_at"
        );

        let again = set_state(&pool, row.id, ReportState::Resolved)
            .await
            .unwrap()
            .expect("the report still exists");
        assert_eq!(
            again.action_taken_at, resolved.action_taken_at,
            "resolving twice keeps the first stamp"
        );

        let reopened = set_state(&pool, row.id, ReportState::Open)
            .await
            .unwrap()
            .expect("the report still exists");
        assert_eq!(reopened.state, ReportState::Open, "back to open");
        assert_eq!(
            reopened.action_taken_at, None,
            "and the action it no longer stands on is cleared"
        );
    }

    #[tokio::test]
    async fn writing_state_or_assignment_reports_the_miss() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        assert_eq!(
            set_state(&pool, Uuid::now_v7(), ReportState::Resolved)
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
}
