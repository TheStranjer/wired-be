//! Replacing a mirrored poll wholesale: [`replace_remote_poll`], the inbound
//! `Question` update whose options (or choice mode) changed, and
//! [`delete_for_status`], the update that dropped the poll.

use sqlx::PgPool;
use uuid::Uuid;

use super::{RemoteTotals, create::INSERT_OPTIONS_SQL};
use crate::{error::AppResult, util::id::new_id};

/// [`replace_remote_poll`]'s poll write: the choice mode, deadline and
/// counters the remote states; the deadline is taken as given, so a
/// restated poll without one stops expiring. The poll starts over, so its
/// end notification is owed again.
const REPLACE_POLL_SQL: &str = "UPDATE polls
     SET multiple = $2,
         expires_at = $3,
         voters_count = $4,
         votes_count = $5,
         ended_notified = false
   WHERE id = $1";

/// Rewrites poll `poll_id` as the remote now states it: every stored vote
/// and option is deleted and the options in `totals` are inserted fresh,
/// each with the count the remote gives it.
///
/// A remote author who edits a poll's options resets its votes, so the
/// marks cast here point at options that no longer exist there; they go,
/// and local voters may vote again. `multiple`, the deadline (`None` means
/// no deadline) and both counters are overwritten. It all runs in one
/// transaction; a poll id naming no stored poll changes nothing.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError);
/// the rollback leaves the poll exactly as it was.
pub async fn replace_remote_poll(
    pool: &PgPool,
    poll_id: Uuid,
    multiple: bool,
    totals: &RemoteTotals,
) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(REPLACE_POLL_SQL)
        .bind(poll_id)
        .bind(multiple)
        .bind(totals.expires_at)
        .bind(totals.voters_count)
        .bind(totals.votes_count)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if updated == 0 {
        tx.rollback().await?;
        return Ok(());
    }
    sqlx::query("DELETE FROM poll_votes WHERE poll_id = $1")
        .bind(poll_id)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    sqlx::query("DELETE FROM poll_options WHERE poll_id = $1")
        .bind(poll_id)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    let ids: Vec<Uuid> = totals.options.iter().map(|_| new_id()).collect();
    let titles: Vec<String> = totals
        .options
        .iter()
        .map(|(title, _votes)| title.clone())
        .collect();
    let votes: Vec<i32> = totals
        .options
        .iter()
        .map(|(_title, votes)| *votes)
        .collect();
    sqlx::query(INSERT_OPTIONS_SQL)
        .bind(poll_id)
        .bind(&ids)
        .bind(&titles)
        .bind(&votes)
        .execute(&mut *tx)
        .await
        .map(drop)?;
    tx.commit().await?;
    Ok(())
}

/// Deletes the poll of status `status_id` with its options and votes, and
/// answers whether there was one.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn delete_for_status(pool: &PgPool, status_id: Uuid) -> AppResult<bool> {
    let deleted = sqlx::query("DELETE FROM polls WHERE status_id = $1")
        .bind(status_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::poll::{
        cast_vote, get_for_status, has_voted, options,
        test_support::{count_for, create_for, create_status, create_user, params},
    };

    /// The totals of a restated poll titled `titles`, one vote each.
    fn restated(titles: &[&str]) -> RemoteTotals {
        RemoteTotals {
            options: titles.iter().map(|t| ((*t).to_owned(), 1)).collect(),
            voters_count: 1,
            votes_count: i32::try_from(titles.len()).expect("a few options"),
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn replacing_swaps_options_clears_votes_and_lets_voters_vote_again() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let author = create_user(&pool, "replace_author").await;
        let voter = create_user(&pool, "replace_voter").await;
        let posted = create_status(&pool, &author).await;
        let stand = create_for(&pool, posted.id, &params(&["old1", "old2"])).await;
        let old = options(&pool, stand.id).await.expect("the options read");
        let first_cast = cast_vote(&pool, stand.id, voter.id, &[old[0].id])
            .await
            .expect("the vote casts");
        assert!(first_cast, "the fixture vote lands");

        replace_remote_poll(&pool, stand.id, true, &restated(&["new1", "new2", "new3"]))
            .await
            .expect("the poll is replaced");

        let reloaded = get_for_status(&pool, posted.id)
            .await
            .expect("the poll read")
            .expect("the poll is still stored");
        assert_eq!(reloaded.id, stand.id, "the poll row itself stays");
        assert!(reloaded.multiple, "the choice mode follows the remote");
        assert_eq!(reloaded.votes_count, 3, "the remote's total");
        let fresh = options(&pool, stand.id).await.expect("the options read");
        let titles: Vec<&str> = fresh.iter().map(|o| o.title.as_str()).collect();
        assert_eq!(titles, vec!["new1", "new2", "new3"], "in order");
        let positions: Vec<i32> = fresh.iter().map(|o| o.position).collect();
        assert_eq!(positions, vec![0, 1, 2], "from zero");
        assert!(
            !has_voted(&pool, stand.id, voter.id).await.expect("reads"),
            "the old ballot is gone"
        );
        assert_eq!(
            count_for(
                &pool,
                "SELECT count(*) FROM poll_votes WHERE poll_id = $1",
                stand.id
            )
            .await,
            0,
            "no marks remain"
        );
        let again = cast_vote(&pool, stand.id, voter.id, &[fresh[1].id])
            .await
            .expect("the new vote casts");
        assert!(again, "the voter may vote again");
    }

    #[tokio::test]
    async fn replacing_an_unknown_poll_changes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        replace_remote_poll(&pool, new_id(), false, &restated(&["a"]))
            .await
            .expect("the write runs against no row");
        let orphans = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM poll_options")
            .fetch_one(&pool)
            .await
            .expect("the count reads");
        assert_eq!(orphans, 0, "no options were inserted");
    }

    #[tokio::test]
    async fn deleting_for_a_status_takes_the_poll_and_reports_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let author = create_user(&pool, "delete_author").await;
        let posted = create_status(&pool, &author).await;
        let stand = create_for(&pool, posted.id, &params(&["a", "b"])).await;

        assert!(
            delete_for_status(&pool, posted.id).await.expect("deletes"),
            "a poll was there"
        );
        assert!(
            get_for_status(&pool, posted.id)
                .await
                .expect("reads")
                .is_none(),
            "the poll is gone"
        );
        assert!(
            options(&pool, stand.id).await.expect("reads").is_empty(),
            "and its options"
        );
        assert!(
            !delete_for_status(&pool, posted.id).await.expect("deletes"),
            "a second delete finds nothing"
        );
    }
}
