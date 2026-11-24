//! The statements and the writes and unscoped and owner-scoped reads of the
//! whole `scheduled_statuses` rows: [`insert`] stores a parked request,
//! [`get_by_id`] and [`delete`] are the worker's unscoped pair, and
//! [`get_owned`], [`reschedule`] and [`delete_owned`] are the
//! account-scoped ones the management endpoints run — scoping is what makes
//! a foreign schedule indistinguishable from a missing one. The keyset page
//! of one account's rows is [`pages`](super::pages)'s; the counters, the due
//! sweep and the media gate are [`limits_due`](super::limits_due)'s.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

use super::{NewScheduledStatus, ScheduledStatus};

/// The `INSERT` behind [`insert`]: the minted id plus the three payload
/// fields, with `RETURNING *` so the row comes back with its database-side
/// timestamps.
const INSERT_SQL: &str = "INSERT INTO scheduled_statuses (id, user_id, scheduled_at, params)
     VALUES ($1, $2, $3, $4)
  RETURNING *";

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

/// [`lock_due`]: the publisher's claim — the row only while its moment has
/// passed, locked for the caller's transaction; a row another transaction
/// holds is skipped rather than waited for.
const LOCK_DUE_SQL: &str = "SELECT * FROM scheduled_statuses
    WHERE id = $1 AND scheduled_at <= now()
      FOR UPDATE SKIP LOCKED";

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
    let mut conn = pool.acquire().await?;
    insert_conn(&mut conn, new_status).await
}

/// [`insert`] on a connection the caller holds, so the write can share a
/// transaction with the limit counts that gate it (see
/// [`lock_user`](super::lock_user)).
///
/// # Errors
///
/// The same as [`insert`].
pub async fn insert_conn(
    conn: &mut PgConnection,
    new_status: &NewScheduledStatus,
) -> AppResult<ScheduledStatus> {
    let stored = sqlx::query_as::<_, ScheduledStatus>(INSERT_SQL)
        .bind(new_id())
        .bind(new_status.user_id)
        .bind(new_status.scheduled_at)
        .bind(&new_status.params)
        .fetch_one(conn)
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

/// Claims schedule `id` for the publisher: locks the row `FOR UPDATE SKIP
/// LOCKED` on `conn`, only if its `scheduled_at` has passed.
///
/// `None` means there is nothing for this caller to publish: the row is gone,
/// is not due yet (a reschedule moved it past the moment a stale fire was
/// queued for), or another transaction holds its lock — which is how the
/// per-row fire and the sweep keep from both posting one schedule. The lock
/// lasts until the transaction `conn` belongs to ends; call this on the
/// connection of an open transaction, since outside one the lock is released
/// as soon as the statement finishes and protects nothing.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn lock_due(conn: &mut PgConnection, id: Uuid) -> AppResult<Option<ScheduledStatus>> {
    let claimed = sqlx::query_as::<_, ScheduledStatus>(LOCK_DUE_SQL)
        .bind(id)
        .fetch_optional(conn)
        .await?;
    Ok(claimed)
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
    let mut conn = pool.acquire().await?;
    delete_on(&mut conn, id).await
}

/// [`delete`] on a connection the caller holds, so the removal can share the
/// transaction that holds the row's [`lock_due`] lock.
///
/// # Errors
///
/// The same as [`delete`].
pub async fn delete_on(conn: &mut PgConnection, id: Uuid) -> AppResult<bool> {
    let deleted = sqlx::query("DELETE FROM scheduled_statuses WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Deletes every schedule of `user_id` and returns the ids that went, so
/// the caller can cancel each one's pending publish job.
///
/// The account-deletion sweep's writer: the user row is kept, so no
/// cascade takes these rows. Nothing to delete answers the empty vector,
/// which makes a rerun a no-op.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_all_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<Uuid>> {
    let ids = sqlx::query_scalar::<_, Uuid>(
        "DELETE FROM scheduled_statuses WHERE user_id = $1 RETURNING id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(ids)
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

#[cfg(test)]
mod tests {
    use chrono::Duration as ChronoDuration;
    use serde_json::json;

    use super::super::count_pending_for_user;
    use super::super::test_support::{at, schedule, user};
    use super::*;

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
    async fn lock_due_claims_only_a_due_row_and_only_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let due = schedule(
            &pool,
            &alice,
            Utc::now() - ChronoDuration::minutes(1),
            json!({ "status": "due" }),
        )
        .await;
        let future = schedule(
            &pool,
            &alice,
            Utc::now() + ChronoDuration::hours(1),
            json!({ "status": "later" }),
        )
        .await;

        let mut first = pool.begin().await.expect("the first transaction opens");
        let mut second = pool.begin().await.expect("the second transaction opens");
        assert!(
            lock_due(&mut first, future.id)
                .await
                .expect("the future claim runs")
                .is_none(),
            "a row whose moment has not come is not claimable"
        );
        assert!(
            lock_due(&mut first, Uuid::now_v7())
                .await
                .expect("the unknown claim runs")
                .is_none(),
            "an id the table never held is not claimable"
        );
        let held = lock_due(&mut first, due.id)
            .await
            .expect("the first claim runs")
            .expect("the first transaction gets the due row");
        assert_eq!(held.id, due.id, "the claim returns the row itself");
        assert!(
            lock_due(&mut second, due.id)
                .await
                .expect("the second claim runs")
                .is_none(),
            "a row another transaction holds is skipped, not waited for"
        );

        assert!(
            delete_on(&mut first, due.id)
                .await
                .expect("the delete runs"),
            "the holder removes its row"
        );
        first.commit().await.expect("the holder commits");
        assert!(
            lock_due(&mut second, due.id)
                .await
                .expect("the claim after commit runs")
                .is_none(),
            "and once the holder committed the row is gone for good"
        );
        second.rollback().await.expect("the second rolls back");
    }

    #[tokio::test]
    async fn lock_due_is_free_again_after_a_rollback() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let due = schedule(
            &pool,
            &alice,
            Utc::now() - ChronoDuration::minutes(1),
            json!({ "status": "due" }),
        )
        .await;

        let mut first = pool.begin().await.expect("the first transaction opens");
        assert!(
            lock_due(&mut first, due.id)
                .await
                .expect("the first claim runs")
                .is_some(),
            "the first claim holds the row"
        );
        first.rollback().await.expect("the holder rolls back");

        let mut second = pool.begin().await.expect("the second transaction opens");
        assert!(
            lock_due(&mut second, due.id)
                .await
                .expect("the second claim runs")
                .is_some(),
            "a rolled-back claim leaves the row claimable"
        );
        second.rollback().await.expect("the second rolls back");
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
