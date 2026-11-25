//! The remote accounts that hold a copy of a local status without following
//! its author: the voters on its poll and the accounts that boosted it.
//!
//! Mastodon's `DistributePollUpdateWorker` and `StatusUpdateDistributionWorker`
//! add both to the recipients of a poll or status `Update`; the federation
//! hooks read them from here.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, models::user::User};

/// The remote, active accounts with a mark on the poll of status `$1`, now
/// or before an edit wiped the ballots (`poll_departed_voters`).
const REMOTE_VOTERS_SQL: &str = "SELECT u.* FROM users u
      WHERE u.local = false AND u.is_active = true
        AND (u.id IN (SELECT pv.user_id FROM poll_votes pv
                        JOIN polls p ON p.id = pv.poll_id
                       WHERE p.status_id = $1)
             OR u.id IN (SELECT dv.user_id FROM poll_departed_voters dv
                          WHERE dv.status_id = $1))
   ORDER BY u.id";

/// The remote, active accounts holding a reblog of status `$1`: live reblogs
/// only, or tombstoned ones too when `$2` is set.
const REMOTE_BOOSTERS_SQL: &str = "SELECT u.* FROM users u
      WHERE u.local = false AND u.is_active = true
        AND u.id IN (SELECT r.user_id FROM statuses r
                      WHERE r.reblog_of_id = $1 AND ($2 OR r.deleted_at IS NULL))
   ORDER BY u.id";

/// The accounts that voted on the poll of `status_id`, remote and active.
///
/// Includes the voters an edit of the poll reset, until
/// [`clear_departed_voters`] forgets them.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn remote_voters(pool: &PgPool, status_id: Uuid) -> AppResult<Vec<User>> {
    let rows = sqlx::query_as::<_, User>(REMOTE_VOTERS_SQL)
        .bind(status_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The accounts that boosted `status_id`, remote and active.
///
/// `withdrawn` also counts boosts already tombstoned: the delete of a post
/// tombstones its reblogs before the `Delete` is fanned out, and the servers
/// of those boosters still hold the copy.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn remote_boosters(
    pool: &PgPool,
    status_id: Uuid,
    withdrawn: bool,
) -> AppResult<Vec<User>> {
    let rows = sqlx::query_as::<_, User>(REMOTE_BOOSTERS_SQL)
        .bind(status_id)
        .bind(withdrawn)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Forgets the voters a poll edit reset for `status_id`, answering how many
/// were remembered.
///
/// Called once the edit's `Update` has been queued to them.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn clear_departed_voters(pool: &PgPool, status_id: Uuid) -> AppResult<u64> {
    let cleared = sqlx::query("DELETE FROM poll_departed_voters WHERE status_id = $1")
        .bind(status_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(cleared)
}
