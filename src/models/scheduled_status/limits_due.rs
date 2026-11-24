//! The create-time limit counters, the UTC-day window arithmetic, the
//! worker's due sweep ([`fetch_due`]) and the media reservation gate
//! ([`reserved_media_ids`]).
//!
//! Every row of this table is pending by construction — the worker removes
//! one as it fires it (see [`delete`](super::delete)) — so the counters are
//! plain counts over the account's slice of the `(user_id, scheduled_at)`
//! index, and the sweep is that index read in due order.

use chrono::{DateTime, Duration as ChronoDuration, NaiveTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::error::AppResult;

use super::ScheduledStatus;

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

/// [`lock_user`]: the transaction-scoped advisory lock keyed by the account,
/// hashed to the `bigint` the lock functions take.
const LOCK_USER_SQL: &str =
    "SELECT pg_advisory_xact_lock(hashtextextended('scheduled_statuses:' || $1::text, 0))";

/// [`fetch_due`]: the worker's sweep — everything due by `now`, oldest first,
/// a bounded page. `scheduled_at` orders the fire sequence; `id` (a `UUIDv7`,
/// so creation order) breaks a same-instant tie stably.
const FETCH_DUE_SQL: &str = "SELECT * FROM scheduled_statuses
      WHERE scheduled_at <= $1
   ORDER BY scheduled_at, id
      LIMIT $2";

/// [`fetch_due_ids`]: [`FETCH_DUE_SQL`]'s ordering and bound, ids only — the
/// sweep claims each row itself, so it needs no payload from this read.
const FETCH_DUE_IDS_SQL: &str = "SELECT id FROM scheduled_statuses
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
    let mut conn = pool.acquire().await?;
    count_pending_for_user_conn(&mut conn, user_id).await
}

/// [`count_pending_for_user`] on a connection the caller holds, so the count
/// can run inside the transaction that [`lock_user`] serialises.
///
/// # Errors
///
/// The same as [`count_pending_for_user`].
pub async fn count_pending_for_user_conn(conn: &mut PgConnection, user_id: Uuid) -> AppResult<i64> {
    let counted = sqlx::query_scalar::<_, i64>(COUNT_FOR_USER_SQL)
        .bind(user_id)
        .fetch_one(conn)
        .await?;
    Ok(counted)
}

/// Takes the transaction-scoped advisory lock that serialises the schedule
/// writers of one account.
///
/// The create arm counts an account's pending rows and then inserts one; run
/// as separate statements, concurrent requests all read the same count under
/// the cap and all insert. Holding this lock from before the counts until the
/// transaction ends makes the count-then-insert pair atomic per account: a
/// second request waits here, and on release its counts (a fresh snapshot per
/// statement under `READ COMMITTED`) see the first one's committed row. The
/// lock key is the account id, so other accounts never wait on each other,
/// and it is released by the commit or rollback, never by the caller.
///
/// Call it on the connection of an open transaction: outside one, the lock is
/// released as soon as this statement finishes and protects nothing.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn lock_user(conn: &mut PgConnection, user_id: Uuid) -> AppResult<()> {
    let _locked = sqlx::query(LOCK_USER_SQL)
        .bind(user_id)
        .execute(conn)
        .await?;
    Ok(())
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
    let mut conn = pool.acquire().await?;
    count_pending_for_user_between_conn(&mut conn, user_id, from, to).await
}

/// [`count_pending_for_user_between`] on a connection the caller holds, so
/// the count can run inside the transaction that [`lock_user`] serialises.
///
/// # Errors
///
/// The same as [`count_pending_for_user_between`].
pub async fn count_pending_for_user_between_conn(
    conn: &mut PgConnection,
    user_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> AppResult<i64> {
    let counted = sqlx::query_scalar::<_, i64>(COUNT_FOR_USER_BETWEEN_SQL)
        .bind(user_id)
        .bind(from)
        .bind(to)
        .fetch_one(conn)
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
/// [`delete`]: super::delete
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

/// The ids of the schedules due at or before `now`, oldest first, at most
/// `limit` of them — [`fetch_due`] without the rows.
///
/// Nothing is locked: the sweep takes each id through
/// [`lock_due`](super::lock_due) and publishes it like the per-row job does,
/// so a row another runner holds simply comes back from the claim as `None`.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn fetch_due_ids(pool: &PgPool, now: DateTime<Utc>, limit: i64) -> AppResult<Vec<Uuid>> {
    let ids = sqlx::query_scalar::<_, Uuid>(FETCH_DUE_IDS_SQL)
        .bind(now)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(ids)
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

    use super::super::delete;
    use super::super::test_support::{at, pending, schedule, user};
    use super::*;

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
    async fn fetch_due_ids_returns_the_due_ids_oldest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let now = Utc::now().trunc_subsecs(6);
        let hour = ChronoDuration::hours(1);
        let late = schedule(&pool, &alice, now + hour, json!({ "status": "late" })).await;
        let middle = schedule(&pool, &alice, now - hour, json!({ "status": "middle" })).await;
        let due_now = schedule(&pool, &alice, now, json!({ "status": "now" })).await;
        let oldest = schedule(&pool, &alice, now - hour * 2, json!({ "status": "old" })).await;

        let ids = fetch_due_ids(&pool, now, 10)
            .await
            .expect("the id sweep runs");
        assert_eq!(
            ids,
            vec![oldest.id, middle.id, due_now.id],
            "due ids only, oldest first, the exactly-due row included"
        );
        let page = fetch_due_ids(&pool, now, 2)
            .await
            .expect("the limited id sweep runs");
        assert_eq!(
            page,
            vec![oldest.id, middle.id],
            "the limit takes the oldest"
        );
        assert!(
            fetch_due_ids(&pool, now - hour * 3, 10)
                .await
                .expect("the early id sweep runs")
                .is_empty(),
            "nothing is due before everything"
        );
        assert!(
            !ids.contains(&late.id),
            "a future row is not in the due ids"
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
}
