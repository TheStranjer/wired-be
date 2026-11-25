//! A local author's edit of their own poll: [`PollEdit`], the question as
//! the edit states it, and [`apply_edit`], the write that lands it.
//!
//! Mastodon's `UpdateStatusService` resets a poll when its options (or its
//! choice mode) change: the votes go, the counters start over and voters may
//! answer the new question. An edit that restates the same options keeps
//! every vote and only moves the deadline or the totals flag. Both writes
//! run on the connection of the edit's transaction (`status_edit`'s
//! `update_status`), so the poll and the version that supersedes it commit
//! together.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{PollParams, create, create::INSERT_OPTIONS_SQL};
use crate::{error::AppResult, util::id::new_id};

/// The `polls` row of a status, as the edit reads it: the id and the choice
/// mode the comparison needs.
const STANDING_SQL: &str = "SELECT id, multiple FROM polls WHERE status_id = $1";

/// The titles of one poll's options, in order.
const TITLES_SQL: &str = "SELECT title FROM poll_options WHERE poll_id = $1 ORDER BY position";

/// The voters of a poll about to be reset, remembered (`$1` the status, `$2`
/// the poll) so the edit's `Update` can still reach their servers once the
/// ballots are gone.
const KEEP_VOTERS_SQL: &str = "INSERT INTO poll_departed_voters (status_id, user_id)
     SELECT DISTINCT $1::uuid, user_id FROM poll_votes WHERE poll_id = $2
     ON CONFLICT DO NOTHING";

/// A restated question's write: new choice mode and totals flag, counters
/// back to zero, the deadline replaced only when the edit gave one (`$4`),
/// and the end notification owed again.
const RESET_POLL_SQL: &str = "UPDATE polls
     SET multiple = $2,
         hide_totals = $3,
         expires_at = COALESCE($4, expires_at),
         voters_count = 0,
         votes_count = 0,
         ended_notified = false
   WHERE id = $1";

/// The same question's write: the totals flag, and the deadline only when
/// the edit gave one (`$3`), which also owes the end notification again.
/// Votes and counters stay.
const KEEP_POLL_SQL: &str = "UPDATE polls
     SET hide_totals = $2,
         expires_at = COALESCE($3, expires_at),
         ended_notified = CASE WHEN $3::timestamptz IS NULL THEN ended_notified ELSE false END
   WHERE id = $1";

/// The poll an edit leaves the status with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollEdit {
    /// The options, in the order voters see them.
    pub options: Vec<String>,
    /// Whether a voter may mark more than one option.
    pub multiple: bool,
    /// Whether per-option counts stay hidden while the poll runs.
    pub hide_totals: bool,
    /// The new deadline; `None` keeps the stored one of a poll the status
    /// already has, and is a poll that never closes for a fresh one.
    pub expires_at: Option<DateTime<Utc>>,
}

/// The poll a status carries before the edit, as [`standing`] read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Standing {
    /// The `polls` row id.
    pub poll_id: Uuid,
    /// Whether voters may mark several options now.
    pub multiple: bool,
    /// The stored option titles, in order.
    pub titles: Vec<String>,
}

impl PollEdit {
    /// Whether this edit starts the poll over against `stored`: a
    /// different option list (text or order) or a different choice mode.
    #[must_use]
    pub fn resets_votes(&self, stored: &Standing) -> bool {
        let same_mode = self.multiple == stored.multiple;
        let same_options = self.options.as_slice() == stored.titles.as_slice();
        !(same_mode && same_options)
    }
}

/// Reads the poll of `status_id` on `conn`, or `None` for a status without
/// one.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn standing(conn: &mut PgConnection, status_id: Uuid) -> AppResult<Option<Standing>> {
    let Some((poll_id, multiple)) = sqlx::query_as::<_, (Uuid, bool)>(STANDING_SQL)
        .bind(status_id)
        .fetch_optional(&mut *conn)
        .await?
    else {
        return Ok(None);
    };
    let titles = sqlx::query_scalar::<_, String>(TITLES_SQL)
        .bind(poll_id)
        .fetch_all(&mut *conn)
        .await?;
    Ok(Some(Standing {
        poll_id,
        multiple,
        titles,
    }))
}

/// Lands `edit` on status `status_id`, whose poll before the edit is
/// `standing`.
///
/// No poll yet: one is created from `edit`. A changed question
/// ([`PollEdit::resets_votes`]): every vote and option is deleted, the new
/// options are inserted at zero and both counters restart. The same
/// question: only the totals flag and, when given, the deadline move, and
/// the votes stay.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError);
/// the caller's transaction rolls the whole edit back.
pub async fn apply_edit(
    conn: &mut PgConnection,
    status_id: Uuid,
    edit: &PollEdit,
    standing: Option<&Standing>,
) -> AppResult<()> {
    let Some(stand) = standing else {
        let fresh = PollParams {
            options: edit.options.clone(),
            expires_at: edit.expires_at,
            multiple: edit.multiple,
            hide_totals: edit.hide_totals,
            ..PollParams::default()
        };
        return create(conn, status_id, &fresh).await.map(drop);
    };
    if !edit.resets_votes(stand) {
        sqlx::query(KEEP_POLL_SQL)
            .bind(stand.poll_id)
            .bind(edit.hide_totals)
            .bind(edit.expires_at)
            .execute(&mut *conn)
            .await
            .map(drop)?;
        return Ok(());
    }
    sqlx::query(KEEP_VOTERS_SQL)
        .bind(status_id)
        .bind(stand.poll_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    sqlx::query("DELETE FROM poll_votes WHERE poll_id = $1")
        .bind(stand.poll_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    sqlx::query("DELETE FROM poll_options WHERE poll_id = $1")
        .bind(stand.poll_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    let ids: Vec<Uuid> = edit.options.iter().map(|_| new_id()).collect();
    let zeroes: Vec<i32> = edit.options.iter().map(|_| 0).collect();
    sqlx::query(INSERT_OPTIONS_SQL)
        .bind(stand.poll_id)
        .bind(&ids)
        .bind(&edit.options)
        .bind(&zeroes)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    sqlx::query(RESET_POLL_SQL)
        .bind(stand.poll_id)
        .bind(edit.multiple)
        .bind(edit.hide_totals)
        .bind(edit.expires_at)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;
    use crate::models::poll::{
        cast_vote, get_for_status, has_voted, options,
        test_support::{count_for, create_for, create_status, create_user, params},
    };

    /// An edit stating `titles`, single choice, totals shown, no new deadline.
    fn edit(titles: &[&str]) -> PollEdit {
        PollEdit {
            options: titles.iter().map(|title| (*title).to_owned()).collect(),
            multiple: false,
            hide_totals: false,
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn changed_options_reset_votes_counters_and_options() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let author = create_user(&pool, "pe_author").await;
        let voter = create_user(&pool, "pe_voter").await;
        let posted = create_status(&pool, &author).await;
        let stand = create_for(&pool, posted.id, &params(&["a", "b", "c"])).await;
        let old = options(&pool, stand.id).await.expect("the options read");
        let cast = cast_vote(&pool, stand.id, voter.id, &[old[0].id])
            .await
            .expect("the vote casts");
        assert!(cast, "the fixture vote lands");

        let deadline = Utc::now() + TimeDelta::hours(1);
        let restated = PollEdit {
            expires_at: Some(deadline),
            hide_totals: true,
            ..edit(&["n1", "n2"])
        };
        let mut conn = pool.acquire().await.expect("slot connection");
        let before = standing(&mut conn, posted.id).await.expect("reads");
        assert_eq!(
            before.as_ref().map(|found| found.titles.clone()),
            Some(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]),
            "the standing poll is read in order"
        );
        assert!(restated.resets_votes(before.as_ref().expect("a poll stands")));
        apply_edit(&mut conn, posted.id, &restated, before.as_ref())
            .await
            .expect("the edit applies");

        let after = get_for_status(&pool, posted.id)
            .await
            .expect("reads")
            .expect("the poll row stays");
        assert_eq!(after.id, stand.id, "the poll row itself stays");
        assert_eq!(after.votes_count, 0, "the total restarts");
        assert_eq!(after.voters_count, 0, "so does the voter count");
        assert!(after.hide_totals, "the flag follows the edit");
        assert!(!after.multiple, "single choice");
        assert!(after.expires_at.is_some(), "the deadline moved");
        let fresh = options(&pool, stand.id).await.expect("the options read");
        let titles: Vec<&str> = fresh.iter().map(|o| o.title.as_str()).collect();
        assert_eq!(titles, vec!["n1", "n2"], "the new options, in order");
        assert!(fresh.iter().all(|o| o.votes_count == 0), "all at zero");
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
    }

    #[tokio::test]
    async fn a_choice_mode_change_alone_resets_the_votes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let author = create_user(&pool, "pm_author").await;
        let voter = create_user(&pool, "pm_voter").await;
        let posted = create_status(&pool, &author).await;
        let stand = create_for(&pool, posted.id, &params(&["a", "b"])).await;
        let old = options(&pool, stand.id).await.expect("the options read");
        let cast = cast_vote(&pool, stand.id, voter.id, &[old[1].id])
            .await
            .expect("the vote casts");
        assert!(cast, "the fixture vote lands");

        let multiple = PollEdit {
            multiple: true,
            ..edit(&["a", "b"])
        };
        let mut conn = pool.acquire().await.expect("slot connection");
        let before = standing(&mut conn, posted.id).await.expect("reads");
        apply_edit(&mut conn, posted.id, &multiple, before.as_ref())
            .await
            .expect("the edit applies");

        let after = get_for_status(&pool, posted.id)
            .await
            .expect("reads")
            .expect("the poll stays");
        assert!(after.multiple, "now any-choice");
        assert_eq!(after.votes_count, 0, "the votes went");
        assert!(!has_voted(&pool, stand.id, voter.id).await.expect("reads"));
    }

    #[tokio::test]
    async fn the_same_question_keeps_votes_and_moves_only_the_deadline() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let author = create_user(&pool, "ps_author").await;
        let voter = create_user(&pool, "ps_voter").await;
        let posted = create_status(&pool, &author).await;
        let stand = create_for(&pool, posted.id, &params(&["a", "b"])).await;
        let old = options(&pool, stand.id).await.expect("the options read");
        let cast = cast_vote(&pool, stand.id, voter.id, &[old[0].id])
            .await
            .expect("the vote casts");
        assert!(cast, "the fixture vote lands");

        let mut conn = pool.acquire().await.expect("slot connection");
        let before = standing(&mut conn, posted.id).await.expect("reads");
        let same = edit(&["a", "b"]);
        assert!(!same.resets_votes(before.as_ref().expect("a poll stands")));
        apply_edit(&mut conn, posted.id, &same, before.as_ref())
            .await
            .expect("an unchanged question applies");
        let untouched = get_for_status(&pool, posted.id)
            .await
            .expect("reads")
            .expect("the poll stays");
        assert_eq!(untouched.votes_count, 1, "no deadline given, nothing moved");
        assert!(untouched.expires_at.is_none(), "and no deadline appears");

        let later = PollEdit {
            expires_at: Some(Utc::now() + TimeDelta::hours(2)),
            ..same
        };
        apply_edit(&mut conn, posted.id, &later, before.as_ref())
            .await
            .expect("a deadline edit applies");
        let moved = get_for_status(&pool, posted.id)
            .await
            .expect("reads")
            .expect("the poll stays");
        assert!(moved.expires_at.is_some(), "the deadline moved");
        assert_eq!(moved.votes_count, 1, "the votes stayed");
        assert_eq!(moved.voters_count, 1, "and so did the voter");
        assert!(
            has_voted(&pool, stand.id, voter.id).await.expect("reads"),
            "the ballot survived"
        );
        let kept = options(&pool, stand.id).await.expect("the options read");
        assert_eq!(kept[0].votes_count, 1, "the option kept its count");
        assert_eq!(kept[0].id, old[0].id, "and its id");
    }

    #[tokio::test]
    async fn a_status_without_a_poll_gets_one() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let author = create_user(&pool, "pn_author").await;
        let posted = create_status(&pool, &author).await;

        let mut conn = pool.acquire().await.expect("slot connection");
        assert!(
            standing(&mut conn, posted.id)
                .await
                .expect("reads")
                .is_none(),
            "no poll stands"
        );
        let added = PollEdit {
            multiple: true,
            ..edit(&["x", "y"])
        };
        apply_edit(&mut conn, posted.id, &added, None)
            .await
            .expect("the poll is created");
        let born = get_for_status(&pool, posted.id)
            .await
            .expect("reads")
            .expect("the poll exists now");
        assert!(born.multiple, "the posted mode is stored");
        let created = options(&pool, born.id).await.expect("the options read");
        assert_eq!(created.len(), 2, "both options stored");
    }
}
