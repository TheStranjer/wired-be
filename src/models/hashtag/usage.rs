//! One tag's usage: the per-UTC-day buckets that make up the tag
//! endpoints' `history` ([`day_counts`]) and the page of live `public` root
//! posts the tag's `Atom`/`RSS` feeds enumerate ([`public_posts`]).
//!
//! Both reads are of the tag's *live* statuses: a soft-deleted row's content
//! is gone, so its use of the tag counts nowhere here. [`day_counts`] is
//! grouped by calendar day with the boundary pinned to UTC, and
//! [`public_posts`] walks the `(hashtag_id, status_id DESC)` index that
//! exists to serve exactly that page.

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, models::status::Status};

/// The statement behind [`day_counts`]: the tag's live statuses bucketed by
/// UTC calendar day. `AT TIME ZONE 'UTC'` makes the day boundary explicit
/// (the session zone must not move a status between two days), soft-deleted
/// rows are out (their content is gone, so their use of the tag is), and
/// `COUNT(DISTINCT user_id)` is the day's account count — Mastodon's
/// `uses`/`accounts` pair.
const DAY_COUNTS_SQL: &str = "SELECT (s.inserted_at AT TIME ZONE 'UTC')::date,
       COUNT(*)::bigint, COUNT(DISTINCT s.user_id)::bigint
  FROM status_hashtags sh
  JOIN statuses s ON s.id = sh.status_id
       WHERE sh.hashtag_id = $1 AND s.deleted_at IS NULL AND s.inserted_at >= $2
       GROUP BY 1
       ORDER BY 1";

/// [`public_posts`]: the live `public` root posts carrying one tag, newest
/// id first over the `(hashtag_id, status_id DESC)` index, optionally a
/// keyset page older than the cursor. Replies and reblogs are out: the tag
/// feed advertises what people said, not their boosts or thread replies.
const PUBLIC_POSTS_SQL: &str = "SELECT s.* FROM statuses s
   JOIN status_hashtags sh ON sh.status_id = s.id
  WHERE sh.hashtag_id = $1
    AND s.visibility = 'public'
    AND s.deleted_at IS NULL
    AND s.in_reply_to_id IS NULL
    AND s.reblog_of_id IS NULL
    AND ($2::uuid IS NULL OR s.id < $2)
ORDER BY s.id DESC
    LIMIT $3";

/// The per-UTC-day usage of tag `hashtag_id`, oldest bucket first, for every
/// live status created at or after `since`.
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

/// The newest `limit` live `public` root posts carrying tag `hashtag_id`,
/// newest first (decision 4): the page the tag's `Atom`/`RSS` feeds
/// ([`crate::api::feeds`]) enumerate.
///
/// This is the reverse lookup the `status_hashtags_hashtag_id_idx` index
/// exists to serve. As in the account feed, replies and reblogs are out and
/// only `public` visibility counts; `max_id`, when given, keys the page on
/// the status id.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn public_posts(
    pool: &PgPool,
    hashtag_id: Uuid,
    max_id: Option<Uuid>,
    limit: i64,
) -> AppResult<Vec<Status>> {
    let statuses = sqlx::query_as::<_, Status>(PUBLIC_POSTS_SQL)
        .bind(hashtag_id)
        .bind(max_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(statuses)
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::models::{
        hashtag::{find_or_create, get_by_name, test_support::*},
        status::{self, NewStatus, Visibility},
        user::{NewUser, insert as insert_user, local_ap_id},
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
    /// `public_posts` reads the tag's feed page: the live, `public`, root
    /// posts carrying the tag, newest id first, keyset-paged by `max_id`
    /// and cut by `limit` — the other visibilities, replies, reblogs,
    /// tombstones and untagged posts stay out, and a tag no status has
    /// used answers empty rather than failing.
    #[tokio::test]
    async fn public_posts_pages_the_tagged_public_roots() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let author = insert_user(&pool, &NewUser::local("alice", &local_ap_id(BASE, "alice")))
            .await
            .expect("fixture user inserts");
        let one = status::insert(&pool, BASE, &NewStatus::local(&author, "one #rust"))
            .await
            .expect("fixture status inserts");
        let _unlisted = {
            let mut new = NewStatus::local(&author, "unlisted #rust");
            new.visibility = Visibility::Unlisted;
            status::insert(&pool, BASE, &new)
                .await
                .expect("fixture status inserts")
        };
        let two = status::insert(&pool, BASE, &NewStatus::local(&author, "two #rust"))
            .await
            .expect("fixture status inserts");
        let _reply = {
            let mut new = NewStatus::local(&author, "in answer #rust");
            new.in_reply_to_id = Some(one.id);
            status::insert(&pool, BASE, &new)
                .await
                .expect("fixture status inserts")
        };
        let three = status::insert(&pool, BASE, &NewStatus::local(&author, "three #rust"))
            .await
            .expect("fixture status inserts");
        let _untagged = status::insert(&pool, BASE, &NewStatus::local(&author, "plain talk"))
            .await
            .expect("fixture status inserts");
        assert!(
            status::soft_delete(&pool, two.id).await.unwrap().is_some(),
            "one tagged public root is deleted"
        );

        let tag = get_by_name(&pool, "rust")
            .await
            .unwrap()
            .expect("the tag row exists");
        let page_ids = |statuses: Vec<Status>| {
            statuses
                .into_iter()
                .map(|found| found.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            page_ids(public_posts(&pool, tag.id, None, 20).await.unwrap()),
            vec![three.id, one.id],
            "newest first, the tombstone gone, the unlisted post and the \
             reply out, and the untagged post never in"
        );
        assert_eq!(
            page_ids(
                public_posts(&pool, tag.id, Some(three.id), 20)
                    .await
                    .unwrap()
            ),
            vec![one.id],
            "the cursor pages strictly older posts"
        );
        assert_eq!(
            page_ids(public_posts(&pool, tag.id, None, 1).await.unwrap()),
            vec![three.id],
            "the limit cuts the page"
        );
    }

    /// A tag with no statuses linked answers the empty page rather than
    /// failing: what makes the feed route's "unknown tag is a valid empty
    /// feed" the ordinary answer.
    #[tokio::test]
    async fn public_posts_of_an_unused_tag_is_empty() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let tag = find_or_create(&pool, "neverused")
            .await
            .unwrap()
            .expect("the tag row is created");
        assert!(
            public_posts(&pool, tag.id, None, 20)
                .await
                .unwrap()
                .is_empty(),
            "a tag no status carries has nothing to show"
        );
    }
}
