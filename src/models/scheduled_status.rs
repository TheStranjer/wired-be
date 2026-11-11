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
//! [`scheduled_at`]: ScheduledStatus::scheduled_at

use chrono::{DateTime, Duration as ChronoDuration, NaiveTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

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
    /// When the row last changed; a schedule is write-once, so in practice
    /// it keeps equal to [`inserted_at`](Self::inserted_at).
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
