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

use chrono::{DateTime, Duration as ChronoDuration, NaiveTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, pagination::PageParams, util::id::new_id};

/// The `INSERT` behind [`insert`]: the minted id plus the three payload
/// fields, with `RETURNING *` so the row comes back with its database-side
/// timestamps.
const INSERT_SQL: &str = "INSERT INTO scheduled_statuses (id, user_id, scheduled_at, params)
     VALUES ($1, $2, $3, $4)
  RETURNING *";

/// [`count_pending_for_user`]: every row of one account is pending — a fire
/// or a cancel removes it — so the count is the whole user slice of the
/// `(user_id, scheduled_at)` index.
const COUNT_FOR_USER_SQL: &str = "SELECT count(*) FROM scheduled_statuses WHERE user_id = $1";

/// [`count_pending_for_user_between`]: the same count restricted to the
/// half-open range `from <= scheduled_at < to`, so a UTC-day window built by
/// [`utc_day_bounds`] counts exactly that day and adjacent windows never
/// double-count an instant. The range leads on `(user_id, scheduled_at)`,
/// the index it is written for.
const COUNT_FOR_USER_BETWEEN_SQL: &str = "SELECT count(*) FROM scheduled_statuses
      WHERE user_id = $1 AND scheduled_at >= $2 AND scheduled_at < $3";

/// [`fetch_due`]: the worker's sweep — everything due by `now`, oldest first,
/// a bounded page. `scheduled_at` orders the fire sequence; `id` (a `UUIDv7`,
/// so creation order) breaks a same-instant tie stably.
const FETCH_DUE_SQL: &str = "SELECT * FROM scheduled_statuses
      WHERE scheduled_at <= $1
   ORDER BY scheduled_at, id
      LIMIT $2";

/// [`page_for_user`]: the start of one account's schedule listing — the
/// owner bound first, then [`PageParams`]' keyset conditions, ordering and
/// `LIMIT` on `id` appended after it. The ordering is the id column's, not
/// the fire moment's; see [`page_for_user`] for why.
///
/// [`PageParams`]: crate::pagination::PageParams
const PAGE_SQL: &str = "SELECT * FROM scheduled_statuses WHERE user_id = ";

/// [`get_owned`]: one row, only for the account it belongs to — the join-free
/// form of Pleroma's `ScheduledActivity.get(user, id)`, whose `nil` is the
/// endpoint's 404 whether the id was unknown or somebody else's.
const GET_OWNED_SQL: &str = "SELECT * FROM scheduled_statuses WHERE id = $1 AND user_id = $2";

/// [`reschedule`]: the moved row, in the statement that moves it. The scope
/// and the stamp are the point: `updated_at` records the change — the one
/// place a schedule's two timestamps come apart — and matching on `user_id`
/// too is what turns a foreign id into `None` rather than into somebody
/// else's moved schedule.
const RESCHEDULE_SQL: &str = "UPDATE scheduled_statuses
      SET scheduled_at = $2, updated_at = now()
    WHERE id = $1 AND user_id = $3
RETURNING *";

/// [`delete_owned`]: [`delete`]'s account-scoped twin, for the endpoint that
/// cancels; a foreign or already-gone id deletes nothing, rows-affected
/// answers `0`, and the caller reports the 404.
const DELETE_OWNED_SQL: &str = "DELETE FROM scheduled_statuses WHERE id = $1 AND user_id = $2";

/// [`reserved_media_ids`]: the submitted ids, keeping submission order, that
/// some pending row names inside its `params -> 'media_ids'` array. The
/// containment test is against a one-element array of the id's text, the
/// shape the params store; ids under any other key — `in_reply_to_id`, or a
/// non-`media_ids` array — never match, and a row whose params carry no
/// `media_ids` key at all answers `NULL` to `->` and matches nothing.
const RESERVED_MEDIA_SQL: &str = "SELECT m.id FROM UNNEST($1::uuid[]) AS m(id)
      WHERE EXISTS (
          SELECT 1 FROM scheduled_statuses s
           WHERE s.params -> 'media_ids' @> JSONB_BUILD_ARRAY(m.id::text)
      )";

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

/// Inserts one schedule and returns the stored row.
///
/// The id is a fresh `UUIDv7` (decision 4); the timestamps come from the
/// column defaults. `params` is stored exactly as handed in.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError). The `user_id` foreign key
/// is the failure reachable here: an unknown account.
pub async fn insert(pool: &PgPool, new_status: &NewScheduledStatus) -> AppResult<ScheduledStatus> {
    let stored = sqlx::query_as::<_, ScheduledStatus>(INSERT_SQL)
        .bind(new_id())
        .bind(new_status.user_id)
        .bind(new_status.scheduled_at)
        .bind(&new_status.params)
        .fetch_one(pool)
        .await?;
    Ok(stored)
}

/// Fetches the schedule whose primary key is `id`, `None` when no row
/// matches — an unknown id, or one whose account or fire moment is gone.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> AppResult<Option<ScheduledStatus>> {
    let fetched =
        sqlx::query_as::<_, ScheduledStatus>("SELECT * FROM scheduled_statuses WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(fetched)
}

/// One account's schedules as a [`PageParams`] keyset page, newest-created
/// first.
///
/// This is Pleroma's `MastodonAPI.get_scheduled_activities`:
/// `ScheduledActivity.for_user_query` through `Pagination.fetch_paginated`,
/// which carries no ordering of its own, so Pleroma's default `id desc` is
/// what the listing answers. The keyset, the cursors and the `Link` header all
/// live on `id`, and that is what fixes the order: a listing read in
/// [`scheduled_at`] order cannot be walked by id comparisons, so the time
/// order and the cursors cannot both hold. Pleroma settles it for the id
/// column, and since a `UUIDv7` is minted in creation order the page still
/// runs a burst of scheduling back through the order it was submitted in. For
/// the read that genuinely needs the due-time order there is [`fetch_due`].
///
/// `page.finish` is what turns the `min_id` page — read ascending off the
/// index — back into the newest-first order the response goes out in.
///
/// [`scheduled_at`]: ScheduledStatus::scheduled_at
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn page_for_user(
    pool: &PgPool,
    user_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<ScheduledStatus>> {
    let mut builder = QueryBuilder::<Postgres>::new(PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // the result keeps these statements from being unused (decision 14).
    let _bound: &mut _ = builder.push_bind(user_id);
    page.apply_keyset(&mut builder, "id");
    let rows = builder
        .build_query_as::<ScheduledStatus>()
        .fetch_all(pool)
        .await?;
    Ok(page.finish(rows))
}

/// Fetches one schedule by id, **only for the account that owns it**.
///
/// `None` then means an unknown id and somebody else's id alike — the single
/// answer the management endpoints turn into their 404, so a probe learns
/// nothing about a row it may not touch. Pleroma's
/// `ScheduledActivity.get(user, id)` is the same one `nil` for both.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_owned(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
) -> AppResult<Option<ScheduledStatus>> {
    let fetched = sqlx::query_as::<_, ScheduledStatus>(GET_OWNED_SQL)
        .bind(id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(fetched)
}

/// Deletes schedule `id`, reporting whether a row went.
///
/// Cancelling the schedule of a status that never fired is the ordinary
/// case; an unknown (or already-fired, or already-cancelled) id deletes
/// nothing and answers `false`, so the write is idempotent.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, id: Uuid) -> AppResult<bool> {
    let deleted = sqlx::query("DELETE FROM scheduled_statuses WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Moves schedule `id` of `user_id` to fire at `scheduled_at`, returning the
/// moved row.
///
/// `None` is [`get_owned`]'s pair of answers again: no such row, or one
/// belonging to somebody else.
///
/// Only the moment and `updated_at` change: the parked `params` are exactly
/// what the create normalised, and moving when a post fires rewrites nothing
/// of what it says. Rescheduling past a moment already reached is the
/// caller's decision, not this write's — the endpoint checks the window
/// before it gets here.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn reschedule(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
    scheduled_at: DateTime<Utc>,
) -> AppResult<Option<ScheduledStatus>> {
    let moved = sqlx::query_as::<_, ScheduledStatus>(RESCHEDULE_SQL)
        .bind(id)
        .bind(scheduled_at)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(moved)
}

/// Cancels `user_id`'s schedule `id`, reporting whether a row went.
///
/// `false` — the endpoint's 404 — for an id the account does not own (or
/// never owned) and for one whose fire or cancel already took it away.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_owned(pool: &PgPool, id: Uuid, user_id: Uuid) -> AppResult<bool> {
    let deleted = sqlx::query(DELETE_OWNED_SQL)
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// How many schedules `user_id` has pending.
///
/// Every row of this table is pending by construction — the worker deletes
/// a row as it fires it — so this is the account's whole schedule count, the
/// number the create endpoint weighs against its limit. An account with no
/// schedule counts `0`.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn count_pending_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<i64> {
    let counted = sqlx::query_scalar::<_, i64>(COUNT_FOR_USER_SQL)
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    Ok(counted)
}

/// The UTC calendar day holding `when`, as the half-open bounds
/// `[midnight, next midnight)`.
///
/// Pure arithmetic on the timestamp: [`count_pending_for_user_between`] takes
/// the pair directly, so a day window is exactly one UTC date — a schedule
/// at midnight belongs to the day it starts, and an instant at the closing
/// midnight belongs to the next day, never to both.
#[must_use]
pub fn utc_day_bounds(when: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let midnight = when.date_naive().and_time(NaiveTime::MIN);
    // `NaiveDateTime + TimeDelta` saturates, and a real timestamp is nowhere
    // near the overflow limit, so the next midnight always exists here.
    let next_midnight = midnight + ChronoDuration::days(1);
    (midnight.and_utc(), next_midnight.and_utc())
}

/// How many schedules `user_id` has due within the half-open range
/// `from <= scheduled_at < to` — [`count_pending_for_user`] for a window,
/// most usefully the UTC day from [`utc_day_bounds`].
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn count_pending_for_user_between(
    pool: &PgPool,
    user_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> AppResult<i64> {
    let counted = sqlx::query_scalar::<_, i64>(COUNT_FOR_USER_BETWEEN_SQL)
        .bind(user_id)
        .bind(from)
        .bind(to)
        .fetch_one(pool)
        .await?;
    Ok(counted)
}

/// The schedules due at or before `now`, oldest first, at most `limit` of
/// them — the page a firing worker claims.
///
/// A row appears exactly once per sweep in which its moment has passed, and
/// the caller removes it with [`delete`] as it replays it, so the next sweep
/// sees the schedules behind it.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn fetch_due(
    pool: &PgPool,
    now: DateTime<Utc>,
    limit: i64,
) -> AppResult<Vec<ScheduledStatus>> {
    let due = sqlx::query_as::<_, ScheduledStatus>(FETCH_DUE_SQL)
        .bind(now)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(due)
}

/// Which of `ids` some pending schedule already reserves — the media gate a
/// create or an upload check runs before letting an attachment go to a new
/// status.
///
/// A submitted id is reserved when any pending row's `params` names it inside
/// its `media_ids` array, whoever that row's account is: the reference is
/// what holds media back, and `media_attachments` is never written by this
/// table. The answer keeps `ids`' order and contains only members of `ids`
/// (a row reserving an id nobody submitted is not asked about, and ids
/// already stored in a fired schedule of the row itself are no issue — the
/// caller passes only candidate ids, and at create time the row does not
/// exist yet). An empty `ids` reserves nothing and answers empty.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn reserved_media_ids(pool: &PgPool, ids: &[Uuid]) -> AppResult<Vec<Uuid>> {
    let reserved = sqlx::query_scalar::<_, Uuid>(RESERVED_MEDIA_SQL)
        .bind(ids)
        .fetch_all(pool)
        .await?;
    Ok(reserved)
}

#[cfg(test)]
mod tests {
    use chrono::{SubsecRound as _, TimeZone as _};
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{NewUser, User, insert as insert_user, local_ap_id},
    };

    /// The instance origin the fixtures create users under.
    const BASE: &str = TEST_BASE_URL;

    /// A local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("the fixture user inserts")
    }

    /// A schedule for `who`, due `when`, with `params` stored as given.
    async fn schedule(
        pool: &PgPool,
        who: &User,
        when: DateTime<Utc>,
        params: Value,
    ) -> ScheduledStatus {
        insert(
            pool,
            &NewScheduledStatus {
                user_id: who.id,
                scheduled_at: when,
                params,
            },
        )
        .await
        .expect("the fixture schedule inserts")
    }

    /// A schedule written and left behind: these tests assert through the
    /// model's counts, sweeps and gate, not through the row.
    async fn pending(pool: &PgPool, who: &User, when: DateTime<Utc>, params: Value) {
        let row = schedule(pool, who, when, params).await;
        assert_eq!(row.user_id, who.id, "the fixture row is its account's");
    }

    /// A fixed instant to hang day-boundary fixtures on: hour-minute-second
    /// of 2026-05-17 UTC.
    fn at(h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 17, h, mi, s).unwrap()
    }

    #[tokio::test]
    async fn insert_get_delete_round_trips_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let params = json!({
            "status": "hello tomorrow",
            "visibility": "unlisted",
            "sensitive": false,
            "spoiler_text": "",
            "language": "en",
            "media_ids": [Uuid::now_v7().to_string()],
        });

        let created = schedule(&pool, &alice, at(9, 0, 0), params.clone()).await;
        assert_eq!(created.user_id, alice.id, "scheduled for its owner");
        assert_eq!(
            created.scheduled_at,
            at(9, 0, 0),
            "due at the instant given"
        );
        assert_eq!(
            created.params, params,
            "the params round trip through jsonb"
        );
        assert!(created.inserted_at <= created.updated_at, "write-once row");

        let fetched = get_by_id(&pool, created.id)
            .await
            .expect("the read runs")
            .expect("the row is there");
        assert_eq!(fetched.id, created.id, "the primary key comes back");
        assert_eq!(fetched.params, params, "unchanged");
        assert_eq!(fetched.scheduled_at, created.scheduled_at, "unchanged");

        assert!(
            delete(&pool, created.id).await.expect("the delete runs"),
            "the row went"
        );
        assert!(
            get_by_id(&pool, created.id)
                .await
                .expect("the second read runs")
                .is_none(),
            "and is gone afterwards"
        );

        let again = delete(&pool, created.id)
            .await
            .expect("the repeat delete runs");
        assert!(!again, "deleting what is already gone reports nothing went");
    }

    #[tokio::test]
    async fn delete_of_an_unknown_id_is_false() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        let gone = delete(&pool, Uuid::now_v7())
            .await
            .expect("the delete runs");
        assert!(!gone, "an id the table never held deletes nothing");
    }

    /// The page parameters of `query`, on the listing's cap.
    fn page(query: &str) -> PageParams {
        PageParams::from_query(Some(query), crate::pagination::MAX_LIMIT)
            .expect("the fixture query paginates")
    }

    /// The ids of `rows`, in the order they came back.
    fn ids(rows: &[ScheduledStatus]) -> Vec<Uuid> {
        rows.iter().map(|row| row.id).collect()
    }

    /// Rewinds schedule `id`'s `updated_at` to a fixed past instant and hands
    /// the row back, so a later write's restamping is something a test can
    /// see rather than a race against the clock's resolution.
    async fn backdate(pool: &PgPool, id: Uuid) -> ScheduledStatus {
        let rewound = sqlx::query(
            "UPDATE scheduled_statuses SET updated_at = '2000-01-01T00:00:00Z' WHERE id = $1",
        )
        .bind(id)
        .execute(pool)
        .await
        .expect("the fixture rewinds");
        assert_eq!(
            rewound.rows_affected(),
            1,
            "the fixture row was there to rewind"
        );
        get_by_id(pool, id)
            .await
            .expect("the row reads")
            .expect("the row is stored")
    }

    #[tokio::test]
    async fn page_for_user_walks_one_account_newest_id_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        // Created oldest-first with the due times the other way round: the
        // page is keyed on `id`, so the newest id leads no matter when the
        // row fires.
        let first = schedule(&pool, &alice, at(20, 0, 0), json!({ "status": "a" })).await;
        let second = schedule(&pool, &alice, at(19, 0, 0), json!({ "status": "b" })).await;
        let third = schedule(&pool, &alice, at(18, 0, 0), json!({ "status": "c" })).await;
        let theirs = schedule(&pool, &bob, at(17, 0, 0), json!({ "status": "d" })).await;

        let all = page_for_user(&pool, alice.id, page(""))
            .await
            .expect("the first page reads");
        assert_eq!(
            ids(&all),
            vec![third.id, second.id, first.id],
            "the account's own rows, newest id first, another account's left out"
        );

        let older = page_for_user(&pool, alice.id, page(&format!("max_id={}", second.id)))
            .await
            .expect("the cursor page reads");
        assert_eq!(
            ids(&older),
            vec![first.id],
            "max_id continues where the page ended"
        );

        let newer = page_for_user(&pool, alice.id, page(&format!("min_id={}", second.id)))
            .await
            .expect("the forward page reads");
        assert_eq!(
            ids(&newer),
            vec![third.id],
            "min_id answers the newer page, still newest first"
        );

        let unseen = page_for_user(&pool, alice.id, page(&format!("since_id={}", first.id)))
            .await
            .expect("the unseen page reads");
        assert_eq!(
            ids(&unseen),
            vec![third.id, second.id],
            "since_id reads everything made after its cursor"
        );

        let small = page_for_user(&pool, alice.id, page("limit=2"))
            .await
            .expect("the limited page reads");
        assert_eq!(
            ids(&small),
            vec![third.id, second.id],
            "and the limit cuts the newest page"
        );

        assert!(
            page_for_user(&pool, Uuid::now_v7(), page(""))
                .await
                .expect("a stranger's page reads")
                .is_empty(),
            "an account with no schedule lists nothing"
        );
        assert_eq!(
            ids(&page_for_user(&pool, bob.id, page(""))
                .await
                .expect("bob's page reads"),),
            vec![theirs.id],
            "the page is each account's own"
        );
    }

    #[tokio::test]
    async fn get_owned_answers_only_to_its_owner() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let row = schedule(&pool, &alice, at(9, 0, 0), json!({ "status": "a" })).await;

        let owned = get_owned(&pool, row.id, alice.id)
            .await
            .expect("the owner's read runs")
            .expect("the owner sees the row");
        assert_eq!(owned.id, row.id, "the row comes back whole");
        assert_eq!(owned.scheduled_at, row.scheduled_at, "same moment");
        assert_eq!(owned.params, row.params, "same params");

        assert!(
            get_owned(&pool, row.id, bob.id)
                .await
                .expect("the stranger's read runs")
                .is_none(),
            "another account's schedule is indistinguishable from no schedule"
        );
        assert!(
            get_owned(&pool, Uuid::now_v7(), alice.id)
                .await
                .expect("the unknown read runs")
                .is_none(),
            "and so is an id the table never held"
        );
    }

    #[tokio::test]
    async fn reschedule_moves_the_moment_and_restamps_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let params = json!({ "status": "moved", "visibility": "unlisted" });
        let created = schedule(&pool, &alice, at(9, 0, 0), params.clone()).await;
        let stale = backdate(&pool, created.id).await;

        let moved = reschedule(&pool, created.id, alice.id, at(15, 30, 0))
            .await
            .expect("the reschedule runs")
            .expect("the owner's row moved");
        assert_eq!(moved.id, created.id, "the same row, not a new one");
        assert_eq!(
            moved.scheduled_at,
            at(15, 30, 0),
            "due at the instant it was given"
        );
        assert_eq!(moved.params, params, "the parked request is untouched");
        assert_eq!(moved.user_id, alice.id, "and so is its account");
        assert_eq!(
            moved.inserted_at, created.inserted_at,
            "created when created"
        );
        assert!(
            moved.updated_at > stale.updated_at,
            "and the write stamps updated_at"
        );

        let back = reschedule(&pool, created.id, alice.id, at(9, 0, 0))
            .await
            .expect("the second reschedule runs")
            .expect("a schedule can move again");
        assert_eq!(
            back.scheduled_at,
            at(9, 0, 0),
            "including to an earlier moment"
        );

        assert!(
            reschedule(&pool, created.id, bob.id, at(23, 0, 0))
                .await
                .expect("the stranger's reschedule runs")
                .is_none(),
            "another account moves nothing"
        );
        assert!(
            reschedule(&pool, Uuid::now_v7(), alice.id, at(23, 0, 0))
                .await
                .expect("the unknown reschedule runs")
                .is_none(),
            "and so is an id the table never held"
        );
        let untouched = get_by_id(&pool, created.id)
            .await
            .expect("the read runs")
            .expect("the row is stored");
        assert_eq!(
            untouched.scheduled_at,
            at(9, 0, 0),
            "the two refused writes left the moment alone"
        );
    }

    #[tokio::test]
    async fn delete_owned_cancels_only_its_owners_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let row = schedule(&pool, &alice, at(9, 0, 0), json!({ "status": "a" })).await;

        assert!(
            !delete_owned(&pool, row.id, bob.id)
                .await
                .expect("the stranger's cancel runs"),
            "another account's schedule does not go"
        );
        assert!(
            !delete_owned(&pool, Uuid::now_v7(), alice.id)
                .await
                .expect("the unknown cancel runs"),
            "and neither does an id the table never held"
        );
        assert!(
            delete_owned(&pool, row.id, alice.id)
                .await
                .expect("the owner's cancel runs"),
            "the owner's own row goes"
        );
        assert!(
            get_by_id(&pool, row.id)
                .await
                .expect("the read runs")
                .is_none(),
            "and is gone afterwards"
        );
    }

    #[tokio::test]
    async fn counts_are_per_user_and_per_utc_day() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        // Three on 2026-05-17 (one exactly at the opening midnight, one
        // just before the closing one) and one on the 18th exactly at its
        // midnight — on the far side of the boundary the day window draws.
        pending(&pool, &alice, at(0, 0, 0), json!({ "status": "a" })).await;
        pending(&pool, &alice, at(12, 0, 0), json!({ "status": "b" })).await;
        pending(&pool, &alice, at(23, 59, 59), json!({ "status": "c" })).await;
        pending(
            &pool,
            &alice,
            at(23, 59, 59) + ChronoDuration::days(1),
            json!({ "status": "d" }),
        )
        .await;
        pending(&pool, &bob, at(15, 0, 0), json!({ "status": "e" })).await;

        assert_eq!(
            count_pending_for_user(&pool, alice.id)
                .await
                .expect("alice counts"),
            4,
            "all of alice's rows are pending"
        );
        assert_eq!(
            count_pending_for_user(&pool, bob.id)
                .await
                .expect("bob counts"),
            1,
            "the count is each account's own"
        );
        assert_eq!(
            count_pending_for_user(&pool, Uuid::now_v7())
                .await
                .expect("a stranger counts"),
            0,
            "an account with no schedule counts zero"
        );

        let (from, to) = utc_day_bounds(at(12, 0, 0));
        assert_eq!(
            count_pending_for_user_between(&pool, alice.id, from, to)
                .await
                .expect("the day counts"),
            3,
            "midnight to midnight counts alice's whole 17th, opening bound included"
        );
        assert_eq!(
            count_pending_for_user_between(&pool, alice.id, to, to + ChronoDuration::days(1))
                .await
                .expect("the next day counts"),
            1,
            "and the row at the closing midnight falls on the 18th instead"
        );
        assert_eq!(
            count_pending_for_user_between(&pool, bob.id, from, to)
                .await
                .expect("bob's day counts"),
            1,
            "the window narrows bob's row too"
        );
        assert_eq!(
            count_pending_for_user_between(&pool, alice.id, to, to)
                .await
                .expect("an empty window counts"),
            0,
            "an empty range holds nothing"
        );
    }

    #[test]
    fn utc_day_bounds_is_the_half_open_utc_day() {
        // A normal instant: the day it falls in, both bounds whole.
        assert_eq!(
            utc_day_bounds(at(13, 45, 12)),
            (at(0, 0, 0), at(0, 0, 0) + ChronoDuration::days(1)),
            "an interior instant takes its whole day"
        );
        // Exactly midnight is the opening bound of its own day, included.
        assert_eq!(
            utc_day_bounds(at(0, 0, 0)),
            (at(0, 0, 0), at(0, 0, 0) + ChronoDuration::days(1)),
            "midnight starts the day it belongs to"
        );
        // The last second of the day still closes at the same midnight.
        assert_eq!(
            utc_day_bounds(at(23, 59, 59)),
            (at(0, 0, 0), at(0, 0, 0) + ChronoDuration::days(1)),
            "23:59:59 is still inside the day"
        );
        // The year rolls over the same way as any month end.
        let year_end = Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).unwrap();
        assert_eq!(
            utc_day_bounds(year_end),
            (
                Utc.with_ymd_and_hms(2026, 12, 31, 0, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap(),
            ),
            "the closing bound crosses the year"
        );
    }

    #[tokio::test]
    async fn fetch_due_returns_only_due_rows_oldest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let now = Utc::now().trunc_subsecs(6);
        // Inserted newest-first so the ordering assertion is the query's,
        // not the insertion's.
        let late = schedule(
            &pool,
            &alice,
            now + ChronoDuration::hours(1),
            json!({ "status": "late" }),
        )
        .await;
        let middle = schedule(
            &pool,
            &alice,
            now - ChronoDuration::hours(1),
            json!({ "status": "middle" }),
        )
        .await;
        let due_now = schedule(&pool, &alice, now, json!({ "status": "now" })).await;
        let oldest = schedule(
            &pool,
            &alice,
            now - ChronoDuration::hours(2),
            json!({ "status": "old" }),
        )
        .await;

        let due = fetch_due(&pool, now, 10).await.expect("the sweep runs");
        let ids: Vec<Uuid> = due.iter().map(|row| row.id).collect();
        assert_eq!(
            ids,
            vec![oldest.id, middle.id, due_now.id],
            "due rows only, oldest first, the exactly-due row included"
        );

        let page = fetch_due(&pool, now, 2)
            .await
            .expect("the limited sweep runs");
        assert_eq!(page.len(), 2, "the sweep honours the limit");
        assert_eq!(
            page.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![oldest.id, middle.id],
            "and takes the oldest page"
        );

        assert!(
            delete(&pool, oldest.id).await.expect("cleanup 1"),
            "row went"
        );
        assert!(
            delete(&pool, middle.id).await.expect("cleanup 2"),
            "row went"
        );
        assert!(
            delete(&pool, due_now.id).await.expect("cleanup 3"),
            "row went"
        );
        assert!(delete(&pool, late.id).await.expect("cleanup 4"), "row went");

        let empty = fetch_due(&pool, now, 10)
            .await
            .expect("the drained sweep runs");
        assert!(empty.is_empty(), "fired rows leave the sweep");

        let before_time = fetch_due(&pool, now - ChronoDuration::hours(3), 10)
            .await
            .expect("the early sweep runs");
        assert!(
            before_time.is_empty(),
            "an instant before everything is due sweeps nothing"
        );
    }

    #[tokio::test]
    async fn reserved_media_ids_follows_media_id_arrays_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let mine = Uuid::now_v7();
        let other = Uuid::now_v7();
        let not_media = Uuid::now_v7();
        let nowhere = Uuid::now_v7();

        // A pending row of alice's reserving `mine` and `other` through the
        // media_ids array — the id strings as the create params store them.
        pending(
            &pool,
            &alice,
            at(9, 0, 0) + ChronoDuration::days(1),
            json!({ "status": "pics", "media_ids": [mine.to_string(), other.to_string()] }),
        )
        .await;
        // Bob's pending row carries `not_media` under keys that are not the
        // media_ids array: a scalar `in_reply_to_id` and a differently named
        // array. Neither reserves the attachment for anybody.
        pending(
            &pool,
            &bob,
            at(9, 0, 0) + ChronoDuration::days(1),
            json!({
                "status": "reply",
                "in_reply_to_id": not_media.to_string(),
                "preview_media_ids": [not_media.to_string()],
            }),
        )
        .await;

        let reserved = reserved_media_ids(&pool, &[other, not_media, mine, nowhere])
            .await
            .expect("the gate runs");
        assert_eq!(
            reserved,
            vec![other, mine],
            "array ids hit in submission order; a non-array key in another user's row does not, and an unmentioned id does not"
        );

        assert!(
            reserved_media_ids(&pool, &[nowhere, not_media])
                .await
                .expect("the miss case runs")
                .is_empty(),
            "no matches answer empty"
        );
        assert!(
            reserved_media_ids(&pool, &[])
                .await
                .expect("the empty case runs")
                .is_empty(),
            "asking about nothing reserves nothing"
        );

        // Cancelling the schedule releases the reservation.
        let rows = fetch_due(&pool, at(23, 59, 59) + ChronoDuration::days(1), 10)
            .await
            .expect("the sweep runs");
        for row in &rows {
            assert!(delete(&pool, row.id).await.expect("cleanup"), "row went");
        }
        assert!(
            reserved_media_ids(&pool, &[mine, other])
                .await
                .expect("the post-cancel gate runs")
                .is_empty(),
            "a deleted row reserves no more"
        );
    }

    #[tokio::test]
    async fn deleting_a_user_cascades_their_schedules() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let doomed = schedule(
            &pool,
            &alice,
            at(9, 0, 0) + ChronoDuration::days(1),
            json!({ "status": "a" }),
        )
        .await;
        let kept = schedule(
            &pool,
            &bob,
            at(9, 0, 0) + ChronoDuration::days(1),
            json!({ "status": "b" }),
        )
        .await;

        let erased = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the fixture user deletes")
            .rows_affected();
        assert_eq!(erased, 1, "the scheduling account was there to delete");

        assert!(
            get_by_id(&pool, doomed.id)
                .await
                .expect("the read runs")
                .is_none(),
            "the cascade took the dead account's schedule"
        );
        assert!(
            get_by_id(&pool, kept.id)
                .await
                .expect("the read runs")
                .is_some(),
            "and left the other account's schedule pending"
        );
        assert_eq!(
            count_pending_for_user(&pool, alice.id)
                .await
                .expect("the dead counts"),
            0,
            "nothing of alice's is left"
        );
    }
}
