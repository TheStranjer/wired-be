//! Moving a report through moderation: [`set_state`] writes the state and
//! the `action_taken_at` / `action_taken_by` that belong with it, and
//! [`assign`] names (or un-names) the moderator who owns the report.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, models::report::ReportState};

use super::Report;

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

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use crate::{
        config::TEST_BASE_URL,
        error::AppError,
        models::{
            report::{NewReport, create, get, list, test_support::*},
            user::{Flag, NewUser, insert, local_ap_id},
        },
    };

    use super::*;

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
}
