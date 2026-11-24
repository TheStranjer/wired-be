//! The `jobs` table: the Postgres-backed background job queue.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in plain table
//! rows claimed with `FOR UPDATE SKIP LOCKED` — no Redis, no external queue.
//! A job moves through five states:
//!
//! | State       | Meaning                                              |
//! |-------------|------------------------------------------------------|
//! | `available` | enqueued, not yet claimed                             |
//! | `executing` | claimed by a worker (`locked_at` set, `attempts` up)  |
//! | `retryable` | failed or lost with attempts left; due at `run_at`    |
//! | `completed` | finished successfully (terminal)                      |
//! | `discarded` | failed for good, or lost at its attempt budget (terminal) |
//!
//! [`enqueue`] inserts, [`enqueue_batch`] many in one transaction,
//! [`claim_next`] hands one due job to one worker,
//! [`complete`] and [`fail`] end an attempt, [`rescue_stuck`] recovers
//! jobs whose worker died mid-flight, and [`cancel_pending`] takes back a
//! keyed job no worker has started yet — the cancel half of re-queueing
//! scheduled work. A live job (`available`, `executing`
//! or `retryable`) carrying a `unique_key` blocks any further job with the
//! same key: the deduplication contract of the partial unique index.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

/// One row of `jobs`: a unit of background work.
#[derive(Debug, sqlx::FromRow)]
pub struct Job {
    /// `UUIDv7` primary key, minted by [`enqueue`] (decision 4).
    pub id: Uuid,
    /// Queue workers subscribe to; [`claim_next`] filters on it.
    pub queue: String,
    /// Key naming the handler that runs this job.
    pub kind: String,
    /// JSON arguments handed to the handler.
    pub args: Value,
    /// Lifecycle state: `available`, `executing`, `completed`, `retryable`
    /// or `discarded` (checked by the database).
    pub state: String,
    /// Claims made so far; incremented by [`claim_next`].
    pub attempts: i32,
    /// Attempts at or beyond which [`fail`] discards instead of retrying.
    pub max_attempts: i32,
    /// Earliest time this job may be claimed; `retryable` jobs are due when
    /// it lies in the past.
    pub run_at: DateTime<Utc>,
    /// When the current claim locked the job; `None` in every other state.
    pub locked_at: Option<DateTime<Utc>>,
    /// Message recorded by the most recent [`fail`].
    pub last_error: Option<String>,
    /// Deduplication key; see [`enqueue`] and [`Job::state`].
    pub unique_key: Option<String>,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every update here.
    pub updated_at: DateTime<Utc>,
}

/// What to insert: everything [`enqueue`] needs beyond defaults.
///
/// The three `Option`al fields let the database column defaults apply when
/// left unset.
#[derive(Debug)]
pub struct NewJob<'a> {
    /// Queue to place the job on.
    pub queue: &'a str,
    /// Handler key for the job.
    pub kind: &'a str,
    /// JSON arguments for the handler.
    pub args: Value,
    /// Earliest claim time; `None` schedules the job immediately.
    pub run_at: Option<DateTime<Utc>>,
    /// Failure budget; `None` uses the column default (5).
    pub max_attempts: Option<i32>,
    /// Deduplication key; `None` (the common case) disables dedup.
    pub unique_key: Option<&'a str>,
}

/// [`enqueue`] and [`enqueue_batch`] share this statement: one row per
/// execution, the `Option`al fields falling to the column defaults
/// through `COALESCE`, and the deduplication contract living in the
/// partial unique index the `ON CONFLICT DO NOTHING` rides on.
const INSERT_JOB_SQL: &str =
    "INSERT INTO jobs (id, queue, kind, args, run_at, max_attempts, unique_key)
         VALUES ($1, $2, $3, $4, COALESCE($5, now()), COALESCE($6, 5), $7)
         ON CONFLICT DO NOTHING
         RETURNING *";

/// Inserts a new job and returns its row, or `None` when the row was
/// dropped by deduplication.
///
/// A job whose `unique_key` already belongs to a live job
/// (`available`, `executing` or `retryable`) is not inserted: the partial
/// unique index rejects it and `ON CONFLICT DO NOTHING` turns that into a
/// silent `None`. `NULL` keys never collide. The id is a fresh `UUIDv7`;
/// `run_at` and `max_attempts` fall back to their column defaults when
/// the fields are `None`.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn enqueue(pool: &PgPool, new_job: NewJob<'_>) -> AppResult<Option<Job>> {
    let inserted = sqlx::query_as::<_, Job>(INSERT_JOB_SQL)
        .bind(new_id())
        .bind(new_job.queue)
        .bind(new_job.kind)
        .bind(new_job.args)
        .bind(new_job.run_at)
        // 5 mirrors the `max_attempts` column default.
        .bind(new_job.max_attempts)
        .bind(new_job.unique_key)
        .fetch_optional(pool)
        .await?;
    Ok(inserted)
}

/// Inserts many jobs in one transaction on one connection and reports how
/// many rows actually went in.
///
/// The batch is the instance-deletion queueing of
/// `DELETE /api/v1/pleroma/admin/instances/{instance}`
/// ([`crate::api::admin::federation`]): one
/// [`users.delete_remote`](crate::jobs::deletion::KIND_DELETE_REMOTE)
/// sweep per doomed account, all of them committed together — a failure
/// partway through aborts the transaction, so the queue never learns half
/// a deletion. Deduplication applies per row exactly as in [`enqueue`]: a
/// keyed job a live row already carries is skipped and not counted.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn enqueue_batch(pool: &PgPool, jobs: &[NewJob<'_>]) -> AppResult<u64> {
    let mut tx = pool.begin().await?;
    let mut inserted = 0_u64;
    for new_job in jobs {
        let written = sqlx::query(INSERT_JOB_SQL)
            .bind(new_id())
            .bind(new_job.queue)
            .bind(new_job.kind)
            .bind(&new_job.args)
            .bind(new_job.run_at)
            .bind(new_job.max_attempts)
            .bind(new_job.unique_key)
            .execute(&mut *tx)
            .await?;
        inserted += written.rows_affected();
    }
    tx.commit().await?;
    Ok(inserted)
}

/// Claims the job in `queues` that is due soonest, or `None` when nothing
/// is due.
///
/// One statement flips the chosen row to `executing`, adds one to
/// `attempts` and stamps `locked_at`, using `FOR UPDATE SKIP LOCKED` in a
/// sub-select so concurrent workers scan the same queue without ever
/// blocking on — or handing out twice — the same job. Only jobs whose
/// state is `available` or `retryable`, whose `run_at` has passed and whose
/// `attempts` is still below `max_attempts` are eligible, so a claim never
/// pushes `attempts` past the budget.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn claim_next(pool: &PgPool, queues: &[&str]) -> AppResult<Option<Job>> {
    let names: Vec<String> = queues.iter().copied().map(str::to_owned).collect();
    let claimed = sqlx::query_as::<_, Job>(
        "UPDATE jobs
            SET state = 'executing',
                attempts = attempts + 1,
                locked_at = now(),
                updated_at = now()
          WHERE id = (
              SELECT id
                FROM jobs
               WHERE state IN ('available', 'retryable')
                 AND run_at <= now()
                 AND attempts < max_attempts
                 AND queue = ANY($1)
               ORDER BY run_at
                  FOR UPDATE SKIP LOCKED
               LIMIT 1
          )
        RETURNING *",
    )
    .bind(&names)
    .fetch_optional(pool)
    .await?;
    Ok(claimed)
}

/// Marks the claimed job `completed` and releases its lock, but only while
/// the claim that ran `attempt` still holds the row.
///
/// The write is fenced: it matches only an `executing` row whose `attempts`
/// equals `attempt` (the value [`claim_next`] returned). A job that
/// [`rescue_stuck`] returned to the queue and another worker claimed again
/// is on a later attempt, so a slow first attempt cannot overwrite it.
/// Answers `true` when the row was updated and `false` when nothing matched
/// (unknown `id`, stale `attempt`, or a row no longer `executing`).
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn complete(pool: &PgPool, id: Uuid, attempt: i32) -> AppResult<bool> {
    let done = sqlx::query(
        "UPDATE jobs
            SET state = 'completed',
                locked_at = NULL,
                updated_at = now()
          WHERE id = $1
            AND state = 'executing'
            AND attempts = $2",
    )
    .bind(id)
    .bind(attempt)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Records a failed attempt, retrying or discarding the job, but only while
/// the claim that ran `attempt` still holds the row.
///
/// `error` is stored as `last_error` either way. With `retry_in: Some` and
/// `attempts < max_attempts` the job becomes `retryable` and is due again
/// after `retry_in`; with `retry_in: Some` at the attempt budget — or with
/// `retry_in: None` at all — the job is `discarded`. The lock is released
/// in both cases. Like [`complete`], the write matches only an `executing`
/// row whose `attempts` equals `attempt`, and answers `true` when it
/// updated the row, `false` when nothing matched.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError). A `retry_in` finer than
/// a microsecond or above roughly 292 years cannot be encoded as a
/// Postgres `interval` and fails the same way.
pub async fn fail(
    pool: &PgPool,
    id: Uuid,
    attempt: i32,
    error: &str,
    retry_in: Option<Duration>,
) -> AppResult<bool> {
    match retry_in {
        Some(delay) => retry_or_discard(pool, id, attempt, error, delay).await,
        None => discard(pool, id, attempt, error).await,
    }
}

/// Fails a job that may be retried: `retryable` with a new `run_at` while
/// attempts remain, `discarded` at the budget; decided in SQL so the check
/// is atomic with the write, and fenced to the claim of `attempt`.
async fn retry_or_discard(
    pool: &PgPool,
    id: Uuid,
    attempt: i32,
    error: &str,
    delay: Duration,
) -> AppResult<bool> {
    let done = sqlx::query(
        "UPDATE jobs
            SET state = CASE
                    WHEN attempts < max_attempts THEN 'retryable'
                    ELSE 'discarded'
                END,
                run_at = CASE
                    WHEN attempts < max_attempts THEN now() + $3
                    ELSE run_at
                END,
                last_error = $2,
                locked_at = NULL,
                updated_at = now()
          WHERE id = $1
            AND state = 'executing'
            AND attempts = $4",
    )
    .bind(id)
    .bind(error)
    .bind(delay)
    .bind(attempt)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Fails a job for good, fenced to the claim of `attempt`.
async fn discard(pool: &PgPool, id: Uuid, attempt: i32, error: &str) -> AppResult<bool> {
    let done = sqlx::query(
        "UPDATE jobs
            SET state = 'discarded',
                last_error = $2,
                locked_at = NULL,
                updated_at = now()
          WHERE id = $1
            AND state = 'executing'
            AND attempts = $3",
    )
    .bind(id)
    .bind(error)
    .bind(attempt)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Settles `executing` jobs locked for longer than `older_than`, reporting
/// how many rows changed.
///
/// A worker that dies mid-job leaves its row stuck in `executing`; after
/// its lock has demonstrably expired the row goes back to `retryable`
/// (claimable again, `attempts` kept) with `locked_at` released — unless
/// `attempts` has reached `max_attempts`, in which case the row is
/// `discarded` with a `last_error` saying so. That bounds a poison job
/// (one that aborts the process or never returns) to its attempt budget;
/// the choice is made in the same statement as the write, so it is atomic.
/// `run_at` is untouched, so a rescued job is due at once. Jobs in any other
/// state are invisible to this sweep. The count covers both outcomes.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); an `older_than` that
/// cannot be encoded as a Postgres `interval` fails the same way as in
/// [`fail`].
pub async fn rescue_stuck(pool: &PgPool, older_than: Duration) -> AppResult<u64> {
    let rescued = sqlx::query(
        "UPDATE jobs
            SET state = CASE
                    WHEN attempts < max_attempts THEN 'retryable'
                    ELSE 'discarded'
                END,
                last_error = CASE
                    WHEN attempts < max_attempts THEN last_error
                    ELSE 'lock expired at the attempt budget'
                END,
                locked_at = NULL,
                updated_at = now()
          WHERE state = 'executing'
            AND locked_at < now() - $1",
    )
    .bind(older_than)
    .execute(pool)
    .await?;
    Ok(rescued.rows_affected())
}

/// Deletes the not-yet-started jobs of one `unique_key`, reporting how many
/// rows went.
///
/// The cancel half of re-queueing keyed work — moving a schedule to a new
/// moment runs this before the fresh [`enqueue`], because the partial unique
/// index holds the key through a live job and the old row's `run_at` is the
/// very moment the caller is leaving behind. Only `available` rows go: an
/// `executing` job is in a worker's hands and a `retryable` one is a failure
/// the queue owns, and neither is a not-yet-due fire the caller can replace.
/// Terminal rows are out of the index's reach already, and a `NULL` key
/// matches nothing here, so unkeyed jobs are never this statement's.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn cancel_pending(pool: &PgPool, unique_key: &str) -> AppResult<u64> {
    let cancelled = sqlx::query("DELETE FROM jobs WHERE unique_key = $1 AND state = 'available'")
        .bind(unique_key)
        .execute(pool)
        .await?;
    Ok(cancelled.rows_affected())
}

#[cfg(test)]
mod fencing_tests;
#[cfg(test)]
mod rescue_tests;

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;

    /// A minimal [`NewJob`] payload on `queue` with `kind`: empty args, no
    /// schedule, default attempts, no dedup key.
    pub(super) fn new_job(queue: &'static str, kind: &'static str) -> NewJob<'static> {
        NewJob {
            queue,
            kind,
            args: json!({}),
            run_at: None,
            max_attempts: None,
            unique_key: None,
        }
    }

    /// Reads the row behind `id` back out of the table.
    pub(super) async fn fetch(pool: &PgPool, id: Uuid) -> Job {
        sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn enqueue_claim_complete_round_trip() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let args = json!({ "target": "note/1" });
        let job = enqueue(
            &pool,
            NewJob {
                args: args.clone(),
                ..new_job("default", "publish")
            },
        )
        .await
        .unwrap()
        .expect("a fresh enqueue inserts its row");
        assert_eq!(job.state, "available", "new jobs start available");
        assert_eq!(job.attempts, 0, "nothing has been tried yet");
        assert_eq!(job.max_attempts, 5, "the column default applies");
        assert_eq!(job.args, args, "args round trip through jsonb");
        assert!(job.locked_at.is_none(), "an unclaimed job holds no lock");
        assert!(job.unique_key.is_none(), "no key was asked for");
        assert!(job.last_error.is_none(), "no failure yet");
        assert!(job.run_at <= Utc::now(), "unscheduled means due now");

        let wrong_queue = claim_next(&pool, &["other"]).await.unwrap();
        assert!(
            wrong_queue.is_none(),
            "a claim only sees the queues it names"
        );

        let claimed = claim_next(&pool, &["default"])
            .await
            .unwrap()
            .expect("the job is due and claimable");
        assert_eq!(claimed.id, job.id, "the enqueued row comes back");
        assert_eq!(claimed.state, "executing", "claiming marks it running");
        assert_eq!(claimed.attempts, 1, "a claim counts as an attempt");
        assert!(claimed.locked_at.is_some(), "a claim stamps the lock");

        let empty = claim_next(&pool, &["default"]).await.unwrap();
        assert!(empty.is_none(), "an executing job is not claimed again");

        assert!(
            complete(&pool, claimed.id, claimed.attempts).await.unwrap(),
            "the current claim's write lands"
        );
        let done = fetch(&pool, claimed.id).await;
        assert_eq!(done.state, "completed", "complete ends the job");
        assert!(done.locked_at.is_none(), "complete releases the lock");

        let drained = claim_next(&pool, &["default"]).await.unwrap();
        assert!(drained.is_none(), "a completed job leaves the queue");
    }

    #[tokio::test]
    async fn scheduled_job_is_not_claimed_early() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let when = Utc::now() + TimeDelta::seconds(3600);
        let job = enqueue(
            &pool,
            NewJob {
                run_at: Some(when),
                ..new_job("default", "later")
            },
        )
        .await
        .unwrap()
        .expect("scheduled jobs are inserted fine");

        let early = claim_next(&pool, &["default"]).await.unwrap();
        assert!(early.is_none(), "a job an hour out is not due yet");

        let waiting = fetch(&pool, job.id).await;
        assert_eq!(
            waiting.state, "available",
            "a claim miss leaves the row alone"
        );
        assert_eq!(waiting.attempts, 0, "and costs no attempt");
        assert!(waiting.locked_at.is_none(), "and locks nothing");
    }

    #[tokio::test]
    async fn concurrent_claims_never_share_a_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let first = enqueue(&pool, new_job("ingest", "one"))
            .await
            .unwrap()
            .expect("first job inserts");
        let second = enqueue(&pool, new_job("ingest", "two"))
            .await
            .unwrap()
            .expect("second job inserts");

        let (left, right) = tokio::join!(
            claim_next(&pool, &["ingest"]),
            claim_next(&pool, &["ingest"])
        );
        let left = left.unwrap().expect("worker one gets a job");
        let right = right.unwrap().expect("worker two gets a job");
        assert_ne!(left.id, right.id, "SKIP LOCKED hands one job to one worker");
        assert_eq!(left.attempts, 1, "each got a single attempt");
        assert_eq!(right.attempts, 1, "each got a single attempt");
        let claimed = [left.id, right.id];
        assert!(
            claimed.contains(&first.id) && claimed.contains(&second.id),
            "both due jobs were handed out, none lost"
        );

        let third = claim_next(&pool, &["ingest"]).await.unwrap();
        assert!(third.is_none(), "nothing is left to claim");
    }

    #[tokio::test]
    async fn unique_key_deduplicates_live_jobs() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let job = enqueue(
            &pool,
            NewJob {
                unique_key: Some("acct/1"),
                ..new_job("default", "update")
            },
        )
        .await
        .unwrap()
        .expect("the first job with a key inserts");

        let dup = enqueue(
            &pool,
            NewJob {
                unique_key: Some("acct/1"),
                ..new_job("default", "update")
            },
        )
        .await
        .unwrap();
        assert!(
            dup.is_none(),
            "a second live job with the same key is dropped"
        );
        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM jobs WHERE unique_key = 'acct/1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows.0, 1, "only the first row exists");

        let claimed = claim_next(&pool, &["default"])
            .await
            .unwrap()
            .expect("the keyed job is claimable");
        assert_eq!(claimed.id, job.id, "it is the row first enqueued");
        assert_eq!(
            claimed.unique_key.as_deref(),
            Some("acct/1"),
            "the key round trips"
        );

        let while_executing = enqueue(
            &pool,
            NewJob {
                unique_key: Some("acct/1"),
                ..new_job("default", "update")
            },
        )
        .await
        .unwrap();
        assert!(
            while_executing.is_none(),
            "executing counts as live for the dedup index"
        );

        assert!(
            complete(&pool, claimed.id, claimed.attempts).await.unwrap(),
            "the claim completes its own attempt"
        );
        let reopened = enqueue(
            &pool,
            NewJob {
                unique_key: Some("acct/1"),
                ..new_job("default", "update")
            },
        )
        .await
        .unwrap();
        assert!(
            reopened.is_some(),
            "completing frees the key for a fresh job"
        );
    }

    #[tokio::test]
    async fn fail_retries_then_discards_at_max_attempts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let job = enqueue(
            &pool,
            NewJob {
                max_attempts: Some(2),
                ..new_job("default", "flaky")
            },
        )
        .await
        .unwrap()
        .expect("the flaky job inserts");

        let first = claim_next(&pool, &["default"])
            .await
            .unwrap()
            .expect("it is due at once");
        assert_eq!(first.id, job.id, "the flaky job was claimed");

        assert!(
            fail(
                &pool,
                first.id,
                first.attempts,
                "boom",
                Some(Duration::from_secs(0)),
            )
            .await
            .unwrap(),
            "the first claim records its own failure"
        );
        let retry = fetch(&pool, job.id).await;
        assert_eq!(
            retry.state, "retryable",
            "an attempt below the budget retries"
        );
        assert_eq!(
            retry.last_error.as_deref(),
            Some("boom"),
            "the failure message is stored"
        );
        assert!(retry.locked_at.is_none(), "failing releases the lock");

        let second = claim_next(&pool, &["default"])
            .await
            .unwrap()
            .expect("a retryable job due now is claimed again");
        assert_eq!(second.id, job.id, "the same job comes back");
        assert_eq!(second.attempts, 2, "the retry is the second attempt");

        assert!(
            fail(
                &pool,
                second.id,
                second.attempts,
                "bang",
                Some(Duration::from_secs(60)),
            )
            .await
            .unwrap(),
            "the second claim records its own failure"
        );
        let dead = fetch(&pool, job.id).await;
        assert_eq!(
            dead.state, "discarded",
            "the budget was spent, so this is terminal"
        );
        assert_eq!(
            dead.last_error.as_deref(),
            Some("bang"),
            "the last failure wins the message"
        );

        let after = claim_next(&pool, &["default"]).await.unwrap();
        assert!(after.is_none(), "a discarded job never returns");
    }

    #[tokio::test]
    async fn fail_without_a_retry_delay_discards_at_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let doomed = enqueue(&pool, new_job("default", "doomed"))
            .await
            .unwrap()
            .expect("the doomed job inserts");
        let third = claim_next(&pool, &["default"])
            .await
            .unwrap()
            .expect("it is due at once");
        assert_eq!(third.id, doomed.id, "the doomed job was claimed");
        assert!(
            fail(&pool, third.id, third.attempts, "fatal", None)
                .await
                .unwrap(),
            "the doomed claim records its own failure"
        );
        let dropped = fetch(&pool, doomed.id).await;
        assert_eq!(
            dropped.state, "discarded",
            "no retry delay discards right away"
        );
        assert_eq!(
            dropped.last_error.as_deref(),
            Some("fatal"),
            "and still records the message"
        );
    }

    #[tokio::test]
    async fn rescue_stuck_returns_expired_locks() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let one = enqueue(&pool, new_job("default", "stuck_a"))
            .await
            .unwrap()
            .expect("first stuck job inserts");
        let two = enqueue(&pool, new_job("default", "stuck_b"))
            .await
            .unwrap()
            .expect("second stuck job inserts");
        let claimed_one = claim_next(&pool, &["default"])
            .await
            .unwrap()
            .expect("first claim succeeds");
        let claimed_two = claim_next(&pool, &["default"])
            .await
            .unwrap()
            .expect("second claim succeeds");
        let stuck = [claimed_one.id, claimed_two.id];
        assert!(
            stuck.contains(&one.id) && stuck.contains(&two.id),
            "both jobs are now executing with fresh locks"
        );

        let fresh = rescue_stuck(&pool, Duration::from_secs(3_600))
            .await
            .unwrap();
        assert_eq!(fresh, 0, "a lock younger than the limit is not stuck");

        assert!(
            complete(&pool, claimed_one.id, claimed_one.attempts)
                .await
                .unwrap(),
            "the first claim completes its own attempt"
        );
        let rescued = rescue_stuck(&pool, Duration::from_secs(0)).await.unwrap();
        assert_eq!(rescued, 1, "only the still-executing job is rescued");

        let back = fetch(&pool, claimed_two.id).await;
        assert_eq!(
            back.state, "retryable",
            "a stuck job goes back to the queue"
        );
        assert!(back.locked_at.is_none(), "rescuing releases the lock");
        let done = fetch(&pool, claimed_one.id).await;
        assert_eq!(
            done.state, "completed",
            "rescuing leaves terminal rows alone"
        );

        let reclaimed = claim_next(&pool, &["default"])
            .await
            .unwrap()
            .expect("the rescued job is claimable again");
        assert_eq!(reclaimed.id, claimed_two.id, "it is the rescued row");
        assert_eq!(reclaimed.attempts, 2, "the lost attempt stays counted");
    }

    #[tokio::test]
    async fn cancel_pending_takes_back_an_unstarted_keyed_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let later = Utc::now() + TimeDelta::seconds(3_600);
        let waiting = enqueue(
            &pool,
            NewJob {
                run_at: Some(later),
                unique_key: Some("sched/1"),
                ..new_job("scheduled", "fire")
            },
        )
        .await
        .unwrap()
        .expect("the first keyed job inserts");

        let replacement = enqueue(
            &pool,
            NewJob {
                unique_key: Some("sched/1"),
                ..new_job("scheduled", "fire")
            },
        )
        .await
        .unwrap();
        assert!(
            replacement.is_none(),
            "the live job holds the key against a second one"
        );

        assert_eq!(
            cancel_pending(&pool, "sched/1").await.unwrap(),
            1,
            "the unstarted job goes"
        );
        let still_there: Option<Job> = sqlx::query_as("SELECT * FROM jobs WHERE id = $1")
            .bind(waiting.id)
            .fetch_optional(&pool)
            .await
            .unwrap();
        assert!(
            still_there.is_none(),
            "the cancelled row is deleted, not reset"
        );

        let moved = enqueue(
            &pool,
            NewJob {
                run_at: Some(later),
                unique_key: Some("sched/1"),
                ..new_job("scheduled", "fire")
            },
        )
        .await
        .unwrap();
        assert!(
            moved.is_some(),
            "the freed key takes the replacement job the caller wanted"
        );
    }

    #[tokio::test]
    async fn cancel_pending_leaves_work_the_queue_owns() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let running = enqueue(
            &pool,
            NewJob {
                unique_key: Some("sched/2"),
                ..new_job("scheduled", "fire")
            },
        )
        .await
        .unwrap()
        .expect("the job inserts");
        let claimed = claim_next(&pool, &["scheduled"])
            .await
            .unwrap()
            .expect("the job is due and claimed");
        assert_eq!(claimed.id, running.id, "and it is the keyed row");

        assert_eq!(
            cancel_pending(&pool, "sched/2").await.unwrap(),
            0,
            "an executing job is in a worker's hands, not this cancel's"
        );
        assert!(
            fail(
                &pool,
                running.id,
                claimed.attempts,
                "boom",
                Some(Duration::from_secs(60)),
            )
            .await
            .unwrap(),
            "the running claim records its own failure"
        );
        assert_eq!(
            cancel_pending(&pool, "sched/2").await.unwrap(),
            0,
            "nor is a retryable one"
        );
        let waiting = fetch(&pool, running.id).await;
        assert_eq!(waiting.state, "retryable", "the row and its key stand");

        let rewound =
            sqlx::query("UPDATE jobs SET run_at = now() - interval '1 second' WHERE id = $1")
                .bind(running.id)
                .execute(&pool)
                .await
                .unwrap();
        assert_eq!(rewound.rows_affected(), 1, "the retry was made due");
        let again = claim_next(&pool, &["scheduled"])
            .await
            .unwrap()
            .expect("the retry is due and claimed again");
        assert!(
            complete(&pool, running.id, again.attempts).await.unwrap(),
            "the second claim completes its own attempt"
        );
        assert_eq!(
            cancel_pending(&pool, "sched/2").await.unwrap(),
            0,
            "and a terminal row is never deleted by it either"
        );
        assert_eq!(
            fetch(&pool, running.id).await.state,
            "completed",
            "the completed row stays"
        );

        let unkeyed = enqueue(&pool, new_job("scheduled", "fire"))
            .await
            .unwrap()
            .expect("the unkeyed job inserts");
        assert_eq!(
            cancel_pending(&pool, "sched/2").await.unwrap(),
            0,
            "a NULL key matches no key at all"
        );
        assert_eq!(
            fetch(&pool, unkeyed.id).await.state,
            "available",
            "and the unkeyed job is left alone"
        );
    }

    /// [`enqueue_batch`] commits the whole set in one call and counts what
    /// went in: every row is claimable afterwards, the per-row dedup of
    /// [`enqueue`] holds inside a batch, an empty batch is a clean no-op,
    /// and the `None` defaults land on the column defaults like always.
    #[tokio::test]
    async fn enqueue_batch_inserts_the_whole_set() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        let jobs = vec![
            NewJob {
                args: json!({ "user_id": "11111111-1111-1111-1111-111111111111" }),
                ..new_job("default", "users.delete_remote")
            },
            new_job("default", "users.delete_remote"),
            new_job("other", "some.other"),
        ];
        let inserted = enqueue_batch(&pool, &jobs).await.unwrap();
        assert_eq!(inserted, 3, "three asked, three in");

        let total = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, 3, "all three stand in the table");
        let claimed = claim_next(&pool, &["default"]).await.unwrap();
        assert!(
            matches!(&claimed, Some(job) if job.kind == "users.delete_remote"),
            "the batch is due and claimable at once"
        );
        let stored = fetch(&pool, claimed.expect("just claimed").id).await;
        assert_eq!(stored.max_attempts, 5, "defaults fall to the columns");

        // The keyed arm of the shared statement: a live row with the same
        // key swallows its batch twin, and the unkeyed rows still ride.
        let _first = enqueue(
            &pool,
            NewJob {
                unique_key: Some("dup"),
                ..new_job("default", "keyed")
            },
        )
        .await
        .unwrap()
        .expect("the live keyed job inserts");
        let mixed = vec![
            new_job("default", "plain-a"),
            NewJob {
                unique_key: Some("dup"),
                ..new_job("default", "keyed-dup")
            },
            new_job("default", "plain-b"),
        ];
        let dup_count = enqueue_batch(&pool, &mixed).await.unwrap();
        assert_eq!(dup_count, 2, "the duplicate key is skipped, the rest rides");

        assert_eq!(
            enqueue_batch(&pool, &[]).await.unwrap(),
            0,
            "nothing asked, nothing inserted"
        );
    }
}
