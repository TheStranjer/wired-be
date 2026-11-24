//! The [`polls.end_notify`](crate::jobs::polls::KIND_END_NOTIFY) sweep's
//! side of the table: [`fetch_ended_unnotified`] the work list,
//! [`voter_ids`] whose ballots the closing tells, and
//! [`mark_ended_notified`] the flag write that retires a row.

use std::sync::LazyLock;

use sqlx::{AssertSqlSafe, PgPool};
use uuid::Uuid;

use crate::error::AppResult;

use super::{POLL_COLUMNS, Poll};

/// [`fetch_ended_unnotified`]: the polls the end-notify sweep has not told
/// about yet whose deadline fell inside the look-back window — the strict
/// `expires_at < now()` of the entity's `expired` flag, floored by
/// `now() - $1 minutes` so a poll nobody ever voted on (or a sweep that ran
/// while this instance was down) is not shouted about years late.
/// [`make_interval`] takes the window as a number so the caller's constant
/// and the statement cannot drift apart.
///
/// [`make_interval`]: https://www.postgresql.org/docs/current/functions-datetime.html
static FETCH_ENDED_UNNOTIFIED_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {POLL_COLUMNS}
  FROM polls
 WHERE NOT ended_notified
   AND expires_at IS NOT NULL
   AND expires_at < now()
   AND expires_at >= now() - make_interval(mins => $1)
 ORDER BY id"
    )
});

/// The polls whose [`expires_at`](Poll::expires_at) ran out within the last
/// `window_minutes` minutes and that the [`polls.end_notify`] sweep has not
/// yet told about, oldest id first.
///
/// This is the [`polls.end_notify`](crate::jobs::polls::KIND_END_NOTIFY)
/// job's work list; a poll that closed before the window is left where it
/// is (its telling moment has passed), and one already flagged is done.
///
/// [`polls.end_notify`]: crate::jobs::polls::KIND_END_NOTIFY
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn fetch_ended_unnotified(pool: &PgPool, window_minutes: i32) -> AppResult<Vec<Poll>> {
    let rows = sqlx::query_as::<_, Poll>(AssertSqlSafe(FETCH_ENDED_UNNOTIFIED_SQL.as_str()))
        .bind(window_minutes)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// [`voter_ids`]: the distinct accounts behind one poll's marks, walked
/// through the primary key's `poll_id` lead. `DISTINCT` because a ballot
/// marks one option per row and the sweep tells each voter once, whatever
/// their ballot looked like.
const VOTER_IDS_SQL: &str = "SELECT DISTINCT user_id FROM poll_votes WHERE poll_id = $1 \
      ORDER BY user_id";

/// Every account that marked anything on poll `poll_id`, deduplicated.
///
/// The [`polls.end_notify`](crate::jobs::polls::KIND_END_NOTIFY) telling
/// hands this list to [`crate::services::notify`], which tells the local
/// ones (Pleroma's `create_poll_notifications` walks the Question object's
/// voter list exactly this way).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn voter_ids(pool: &PgPool, poll_id: Uuid) -> AppResult<Vec<Uuid>> {
    let ids = sqlx::query_scalar::<_, Uuid>(VOTER_IDS_SQL)
        .bind(poll_id)
        .fetch_all(pool)
        .await?;
    Ok(ids)
}

/// [`mark_ended_notified`]: the flag write, guarded so a rerun of the same
/// poll reports `0` — the telling it guards has already happened.
const MARK_ENDED_NOTIFIED_SQL: &str = "UPDATE polls SET ended_notified = true \
      WHERE id = $1 AND NOT ended_notified";

/// Sets [`ended_notified`](Poll::ended_notified) on poll `poll_id`, telling
/// the [`fetch_ended_unnotified`] sweep it is done with this row.
///
/// Returns how many rows changed: `1` for the sweep that retired the poll,
/// `0` for a rerun on an already-flagged row.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn mark_ended_notified(pool: &PgPool, poll_id: Uuid) -> AppResult<u64> {
    let outcome = sqlx::query(MARK_ENDED_NOTIFIED_SQL)
        .bind(poll_id)
        .execute(pool)
        .await?;
    Ok(outcome.rows_affected())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::{
        models::{
            poll::{PollParams, cast_vote, get_by_id, options, test_support::*},
            user::User,
        },
        util::id::new_id,
    };

    /// A two-option [`PollParams`] closing `mins` minutes from now
    /// (negative: one whose deadline already ran out).
    fn ending_in(mins: i64) -> PollParams {
        PollParams {
            options: vec!["tea".to_owned(), "coffee".to_owned()],
            expires_at: Some(Utc::now() + chrono::TimeDelta::minutes(mins)),
            multiple: true,
            ..PollParams::default()
        }
    }

    /// A poll by `author` closing `mins` minutes from now, on its own
    /// status.
    async fn ending_poll(pool: &PgPool, author: &User, mins: i64) -> Poll {
        let posted = create_status(pool, author).await;
        create_for(pool, posted.id, &ending_in(mins)).await
    }

    #[tokio::test]
    async fn fetch_ended_unnotified_walks_only_the_window() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let just_ended = ending_poll(&pool, &alice, -1).await;
        let long_ended = ending_poll(&pool, &alice, -10).await;
        let still_open = ending_poll(&pool, &alice, 10).await;
        let immortal = {
            let posted = create_status(&pool, &alice).await;
            create_for(&pool, posted.id, &params(&["tea", "coffee"])).await
        };

        let swept = fetch_ended_unnotified(&pool, 5)
            .await
            .expect("the sweep reads");
        let swept_ids: Vec<Uuid> = swept.iter().map(|found| found.id).collect();
        assert!(
            swept_ids.contains(&just_ended.id),
            "the poll that closed a minute ago is due its telling"
        );
        for absent in [long_ended.id, still_open.id, immortal.id] {
            assert!(
                !swept_ids.contains(&absent),
                "past the window, still open, or without a deadline is nobody's news"
            );
        }

        // A wider window reaches the older close: the bound is the
        // caller's constant, not a second rule.
        let wider = fetch_ended_unnotified(&pool, 15)
            .await
            .expect("the wider sweep reads");
        let wider_ids: Vec<Uuid> = wider.iter().map(|found| found.id).collect();
        assert!(
            wider_ids.contains(&long_ended.id) && wider_ids.contains(&just_ended.id),
            "both closed poll are inside fifteen minutes"
        );
    }

    #[tokio::test]
    async fn mark_ended_notified_retires_the_row_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let closed = ending_poll(&pool, &alice, -1).await;

        assert_eq!(
            mark_ended_notified(&pool, closed.id)
                .await
                .expect("the first marking runs"),
            1,
            "the sweep retires the poll"
        );
        assert_eq!(
            mark_ended_notified(&pool, closed.id)
                .await
                .expect("the rerun runs"),
            0,
            "and a rerun changes nothing"
        );
        assert!(
            fetch_ended_unnotified(&pool, 5)
                .await
                .expect("the sweep reads")
                .iter()
                .all(|found| found.id != closed.id),
            "the flagged poll is off the next work list"
        );
        let stored = get_by_id(&pool, closed.id)
            .await
            .expect("the poll reads")
            .expect("the poll stands");
        assert!(stored.ended_notified.get(), "and the flag round trips");
        assert_eq!(
            mark_ended_notified(&pool, new_id())
                .await
                .expect("an unknown id changes nothing"),
            0,
            "an id that names no poll is not an error"
        );
    }

    #[tokio::test]
    async fn voter_ids_answer_the_distinct_ballots_once_each() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let bob = create_user(&pool, "bob").await;
        let carol = create_user(&pool, "carol").await;
        let closed = ending_poll(&pool, &alice, -1).await;
        let choices = options(&pool, closed.id).await.expect("options read");

        // Bob marks both options of a multiple poll: one ballot, one id.
        assert!(
            cast_vote(&pool, closed.id, bob.id, &[choices[0].id, choices[1].id])
                .await
                .expect("the first ballot writes"),
            "the pair was free"
        );
        assert!(
            cast_vote(&pool, closed.id, carol.id, &[choices[1].id])
                .await
                .expect("the second ballot writes"),
            "and so was hers"
        );

        let mut voters = voter_ids(&pool, closed.id).await.expect("the voters read");
        voters.sort();
        let mut want = vec![bob.id, carol.id];
        want.sort();
        assert_eq!(
            voters, want,
            "each voter once, whichever option they marked"
        );
        assert!(
            voter_ids(&pool, new_id())
                .await
                .expect("the unknown poll reads")
                .is_empty(),
            "a poll nobody voted on has no voters"
        );
    }
}
