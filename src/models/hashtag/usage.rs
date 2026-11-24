//! One tag's usage: the per-UTC-day buckets that make up the tag
//! endpoints' `history` ([`day_counts`]).
//!
//! The read is of the tag's *live, public, original* statuses: a
//! soft-deleted row's content is gone, so its use of the tag counts nowhere
//! here, and an unlisted, followers-only, direct or local status or a reblog
//! is no use either (the history is public, so counting them would leak when
//! private posts were written; Mastodon's `Trends::Tags` records only public
//! non-reblog statuses). [`day_counts`] is grouped by calendar day with the
//! boundary pinned to UTC.

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

/// The statement behind [`day_counts`]: the tag's live, public, non-reblog
/// statuses bucketed by UTC calendar day. `AT TIME ZONE 'UTC'` makes the day
/// boundary explicit (the session zone must not move a status between two
/// days), soft-deleted rows are out (their content is gone, so their use of
/// the tag is), only `visibility = 'public'` originals count (the history is
/// anonymous-readable, so a non-public use must not show), and
/// `COUNT(DISTINCT user_id)` is the day's account count — Mastodon's
/// `uses`/`accounts` pair.
const DAY_COUNTS_SQL: &str = "SELECT (s.inserted_at AT TIME ZONE 'UTC')::date,
       COUNT(*)::bigint, COUNT(DISTINCT s.user_id)::bigint
  FROM status_hashtags sh
  JOIN statuses s ON s.id = sh.status_id
       WHERE sh.hashtag_id = $1 AND s.deleted_at IS NULL AND s.inserted_at >= $2
         AND s.visibility = 'public' AND s.reblog_of_id IS NULL
       GROUP BY 1
       ORDER BY 1";

/// The per-UTC-day usage of tag `hashtag_id`, oldest bucket first.
///
/// Every live public non-reblog status created at or after `since` counts;
/// unlisted, followers-only, direct and local statuses and reblogs do not.
///
/// The `(day, uses, accounts)` triples are what the tag endpoints'
/// `history` is built from; a day no status used the tag on is simply
/// absent — the caller owns the calendar and renders the gap as zeros.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn day_counts(
    pool: &PgPool,
    hashtag_id: Uuid,
    since: DateTime<Utc>,
) -> AppResult<Vec<(NaiveDate, i64, i64)>> {
    let rows = sqlx::query_as::<_, (NaiveDate, i64, i64)>(DAY_COUNTS_SQL)
        .bind(hashtag_id)
        .bind(since)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::models::{
        hashtag::{find_or_create, get_by_name, test_support::*},
        status,
    };

    /// The `(day, uses, accounts)` buckets of tag `name` over the last week,
    /// read straight from [`day_counts`] with a window anchored on `anchor`.
    async fn week_of(
        pool: &PgPool,
        name: &str,
        anchor: DateTime<Utc>,
    ) -> Vec<(NaiveDate, i64, i64)> {
        let hashtag = get_by_name(pool, name)
            .await
            .expect("the tag reads")
            .expect("the fixture tag exists");
        let since = noon(anchor, 6);
        day_counts(pool, hashtag.id, since)
            .await
            .expect("the buckets read")
    }

    /// Noon UTC of the day `days_ago` before `anchor`'s date: the fixture
    /// timestamp one status's bucket day names. Noon (never a boundary) and
    /// one anchor for every fixture mean a UTC midnight crossing during the
    /// run cannot move a bucket or shift an expectation — each fixture's
    /// bucket day is pinned to the date it was computed from.
    fn noon(anchor: DateTime<Utc>, days_ago: i64) -> DateTime<Utc> {
        anchor
            .date_naive()
            .checked_sub_days(chrono::Days::new(
                u64::try_from(days_ago).expect("in range"),
            ))
            .expect("the fixture day exists")
            .and_hms_opt(12, 0, 0)
            .expect("noon")
            .and_utc()
    }

    /// Pin status `id`'s `inserted_at` to [`noon(anchor, days_ago)`].
    async fn pin_day(pool: &PgPool, id: Uuid, anchor: DateTime<Utc>, days_ago: i64) {
        let pinned = sqlx::query("UPDATE statuses SET inserted_at = $2 WHERE id = $1")
            .bind(id)
            .bind(noon(anchor, days_ago))
            .execute(pool)
            .await
            .expect("the pin runs");
        assert_eq!(pinned.rows_affected(), 1, "the fixture status moved");
    }

    #[tokio::test]
    async fn day_counts_ignores_non_public_statuses_and_reblogs() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let anchor = Utc::now();
        let today = anchor.date_naive();
        let public = post(&pool, "alice", "#rust").await;
        let unlisted = post(&pool, "bob", "#rust").await;
        let private = post(&pool, "carol", "#rust").await;
        let direct = post(&pool, "dave", "#rust").await;
        let local = post(&pool, "erin", "#rust").await;
        let reblog = post(&pool, "frank", "#rust").await;
        for (status, visibility) in [
            (&unlisted, "unlisted"),
            (&private, "private"),
            (&direct, "direct"),
            (&local, "local"),
        ] {
            let changed = sqlx::query("UPDATE statuses SET visibility = $2 WHERE id = $1")
                .bind(status.id)
                .bind(visibility)
                .execute(&pool)
                .await
                .expect("the visibility changes");
            assert_eq!(changed.rows_affected(), 1, "the fixture status moved");
        }
        let reblogged = sqlx::query("UPDATE statuses SET reblog_of_id = $2 WHERE id = $1")
            .bind(reblog.id)
            .bind(public.id)
            .execute(&pool)
            .await
            .expect("the reblog link is set");
        assert_eq!(reblogged.rows_affected(), 1, "the fixture status moved");
        for status in [&public, &unlisted, &private, &direct, &local, &reblog] {
            pin_day(&pool, status.id, anchor, 0).await;
        }

        assert_eq!(
            week_of(&pool, "rust", anchor).await,
            vec![(today, 1, 1)],
            "only the public original is a use: unlisted, followers-only, \
             direct, local and reblog rows leave no trace in the history"
        );
    }

    #[tokio::test]
    async fn day_counts_buckets_per_utc_day_counting_uses_and_authors() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let anchor = Utc::now();
        let today = anchor.date_naive();
        let alice = post(&pool, "alice", "#rust twice #rust").await;
        let bob = post(&pool, "bob", "#rust").await;
        let python = post(&pool, "carol", "#python").await;
        let aged = post(&pool, "dave", "#rust").await;
        let gone = post(&pool, "erin", "#rust").await;
        for (status, days_ago) in [(&alice, 0), (&bob, 0), (&python, 0), (&aged, 2), (&gone, 0)] {
            pin_day(&pool, status.id, anchor, days_ago).await;
        }
        let tombstone = status::soft_delete(&pool, gone.id)
            .await
            .expect("the fixture status soft-deletes");
        assert!(
            tombstone.expect("the status was live").deleted_at.is_some(),
            "and it is a tombstone now"
        );

        let buckets = week_of(&pool, "rust", anchor).await;
        assert_eq!(
            buckets,
            vec![(today - Duration::days(2), 1, 1), (today, 2, 2)],
            "one bucket per used day, oldest first: `twice #rust` is one use, \
             the deleted status is no use at all"
        );
        assert_eq!(
            week_of(&pool, "python", anchor).await,
            vec![(today, 1, 1)],
            "the other tag of the same day keeps its own buckets — the read is \
             per tag id"
        );
        let untouched = find_or_create(&pool, "untouched")
            .await
            .unwrap()
            .expect("the name is usable");
        assert!(
            day_counts(&pool, untouched.id, noon(anchor, 6))
                .await
                .unwrap()
                .is_empty(),
            "a tag with no statuses has no buckets (the endpoint renders the \
             zeros)"
        );

        // A use older than the seven-day window is out of the read entirely.
        let ancient = post(&pool, "frank", "#rust").await;
        pin_day(&pool, ancient.id, anchor, 30).await;
        assert_eq!(
            week_of(&pool, "rust", anchor).await.len(),
            2,
            "the thirty-day-old use stays out of the week"
        );
        let wide = day_counts(
            &pool,
            get_by_name(&pool, "rust")
                .await
                .unwrap()
                .expect("rust exists")
                .id,
            noon(anchor, 45),
        )
        .await
        .expect("the wide window reads");
        assert_eq!(
            wide.first().expect("buckets came back").0,
            today - Duration::days(30),
            "the window is the caller's: ask wider, get the older day too"
        );
        assert_eq!(
            wide.len(),
            3,
            "the thirty-day day plus the two inside the week"
        );
    }
}
