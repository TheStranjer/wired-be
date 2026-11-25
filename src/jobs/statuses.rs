//! The status expiration jobs: the per-status scheduled delete and the
//! periodic safety sweep behind it.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres rows;
//! this module is the consumer of the rows a scheduled status delete needs.
//! The producer side is the create: a `POST /api/v1/statuses` body with
//! `expires_in` stores the resulting [`expires_at`](status::Status::expires_at)
//! and [schedules](schedule_expiration) one [`KIND_EXPIRE`] job on
//! [`QUEUE_SCHEDULED`], due at exactly that moment,
//! under the dedupe key `expire:<status_id>` — Pleroma's per-activity
//! `PurgeExpiredActivityWorker` (`lib/pleroma/workers/purge_expired_activity.ex`),
//! which its `CommonAPI.post` schedules for a post carrying `expires_in`.
//!
//! Both handlers run the [deletion service](crate::services::delete_status) —
//! an expiring post leaves exactly what the author's delete button leaves:
//! the tombstone, the stepped counters and the `after_status_deleted` hook.
//! A status that is already gone when its job fires (hand-deleted, or swept)
//! completes the job silently: there is nothing left to do and a retry can
//! not change that.
//!
//! [`KIND_SWEEP`] is the safety net: Pleroma has no twin for it (its purge
//! jobs can simply be missed), but here a lost or failed per-status job must
//! not leave an expiring post alive forever, so every five minutes the sweep
//! tombstones every live **local** status whose `expires_at` has run out (a
//! remote status is only ever removed by its origin's `Delete`), in batches of
//! [`SWEEP_BATCH`] — the same loop
//! [`soft_delete_all_for_user`](crate::models::status::soft_delete_all_for_user)
//! runs for the account purge.

use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;
use uuid::Uuid;

use crate::{
    jobs::{
        AttemptInfo, HandlerFuture, JobError, QUEUE_SCHEDULED, enqueue_keyed, parse_args, retry,
        scheduler::PeriodicJob,
    },
    models::{
        job::NewJob,
        status::{self, Status},
    },
    services::delete_status,
    state::AppState,
};

/// The job kind of a single status's scheduled delete, handled by
/// [`handle_expire`] on [`QUEUE_SCHEDULED`].
pub const KIND_EXPIRE: &str = "statuses.expire";

/// The job kind of the periodic safety sweep, handled by [`handle_sweep`]
/// and scheduled by [`sweep_job`] — on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND), once every five minutes.
pub const KIND_SWEEP: &str = "statuses.expire_sweep";

/// How many expired statuses one sweep pass deletes: small enough to keep
/// every statement short, the ticket's batch size.
pub const SWEEP_BATCH: i64 = 100;

/// The `{status_id}` arguments of the expire job, as [`job_args`] writes and
/// [`handle_expire`] reads them.
#[derive(Debug, Deserialize)]
struct ExpireArgs {
    /// The status the job expires.
    status_id: Uuid,
}

/// The `{status_id}` job-arguments document for status `status_id`: the one
/// shape [`schedule_expiration`] enqueues and [`handle_expire`] deserialises.
#[must_use]
pub fn job_args(status_id: Uuid) -> Value {
    json!({ "status_id": status_id })
}

/// The dedupe key of status `status_id`'s expire job.
///
/// At most one live scheduled delete per status, however many times its
/// create is replayed (the `Idempotency-Key` arm restates the same row,
/// [`enqueue`](crate::models::job::enqueue)'s `None` swallows the second claim).
#[must_use]
pub fn unique_key(status_id: Uuid) -> String {
    format!("expire:{status_id}")
}

/// The sweep's [`PeriodicJob`] registry entry: [`KIND_SWEEP`] on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND) every five minutes.
/// [`periodic_jobs`](super::scheduler::periodic_jobs) appends it; a test
/// reads it so the period and queue are pinned in one place.
#[must_use]
pub const fn sweep_job() -> PeriodicJob {
    PeriodicJob {
        kind: KIND_SWEEP,
        queue: super::QUEUE_BACKGROUND,
        every: std::time::Duration::from_secs(300),
    }
}

/// Schedules `row`'s delete for its [`expires_at`](Status::expires_at).
///
/// The row is a [`KIND_EXPIRE`] job on
/// [`QUEUE_SCHEDULED`], due at exactly that
/// instant, keyed [`unique_key`] so a duplicate create cannot queue the
/// same purge twice. A row with no expiry schedules nothing — a plain post
/// never disappears on its own.
///
/// A failed enqueue is logged, not returned: the status is already stored
/// with its expiry, the [`KIND_SWEEP`](sweep_job) pass deletes it when its
/// time comes, and refusing the post the client already owns would be the
/// worse answer.
pub async fn schedule_expiration(state: &AppState, row: &Status) {
    let Some(expires_at) = row.expires_at else {
        return;
    };
    let key = unique_key(row.id);
    let request = NewJob {
        queue: QUEUE_SCHEDULED,
        kind: KIND_EXPIRE,
        args: job_args(row.id),
        run_at: Some(expires_at),
        max_attempts: None,
        unique_key: Some(&key),
    };
    drop(enqueue_keyed(&state.db, request, "status expiration").await);
}

/// The job name in the discard text of unusable [`ExpireArgs`].
const EXPIRE_JOB: &str = "status expire";

/// The job name in the retry text of a failed pass; both handlers' work is
/// idempotent (a second delete of a tombstone deletes nothing), so another
/// pass costs nothing but the wait.
const PASS_JOB: &str = "status expiration pass";

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_EXPIRE`]:
/// delete the status the arguments name, if it is still live.
///
/// A missing id or an already tombstoned row completes the job silently —
/// the post is exactly where an expiry should leave it. A database failure
/// is a [`JobError::Retry`], and the [`KIND_SWEEP`](sweep_job) pass stands
/// behind even a spent retry budget.
pub fn handle_expire(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move {
        let status_id = parse_args::<ExpireArgs>(&args, EXPIRE_JOB)?.status_id;
        expire_one(&state, status_id).await
    })
}

/// The expire job's body: load the live row, run the deletion service, and
/// treat "no live row" — unknown or already deleted — as success.
///
/// # Errors
///
/// [`JobError::Retry`] on a database failure; the vanished-row arms are not
/// failures.
async fn expire_one(state: &AppState, status_id: Uuid) -> Result<(), JobError> {
    match status::get_by_id(&state.db, status_id).await {
        Ok(Some(row)) if !row.local => {
            debug!(status = %status_id, "a remote status never expires");
            Ok(())
        }
        Ok(Some(row)) => delete(state, &row).await,
        Ok(None) => {
            debug!(status = %status_id, "expiring status is already gone");
            Ok(())
        }
        Err(err) => Err(retry(PASS_JOB, &err)),
    }
}

/// Runs the shared deletion service for one expired row. `false` — a
/// concurrent delete won the race — is the same success the service means
/// for the HTTP route's 404: the post is gone, which is what this job was
/// scheduled to achieve.
///
/// # Errors
///
/// [`JobError::Retry`] on a database failure.
async fn delete(state: &AppState, row: &Status) -> Result<(), JobError> {
    match delete_status::delete_status(state, row).await {
        Ok(deleted) => {
            debug!(status = %row.id, deleted, "status expired");
            Ok(())
        }
        Err(err) => Err(retry(PASS_JOB, &err)),
    }
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_SWEEP`]: walk
/// every live status whose expiry has run out — [`fetch_expired`](status::fetch_expired)
/// rows, [`SWEEP_BATCH`] at a time — and delete each through the same service
/// [`handle_expire`] runs.
///
/// The walk ends on the first short batch: rows tombstoned by this pass drop
/// out of the sweep query, so a full batch only ever means more work, and an
/// instance with nothing expired costs one read. A database failure is a
/// [`JobError::Retry`]; the scheduler enqueues the next period regardless,
/// and the per-status jobs still delete what the failed pass left behind.
pub fn handle_sweep(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        let mut total = 0_i64;
        loop {
            let batch = match status::fetch_expired(&state.db, SWEEP_BATCH).await {
                Ok(rows) => rows,
                Err(err) => return Err(retry(PASS_JOB, &err)),
            };
            let count = i64::try_from(batch.len()).unwrap_or(SWEEP_BATCH);
            delete_batch(&state, &batch).await?;
            total += count;
            if count < SWEEP_BATCH {
                break;
            }
        }
        if total > 0 {
            debug!(total, "expired statuses swept");
        }
        Ok(())
    })
}

/// Deletes one sweep batch through the shared service. Each row's delete is
/// its own transaction, so a retried pass re-runs the earlier ones as
/// no-ops (tombstoning a tombstone deletes nothing) and picks up where the
/// failure stopped.
///
/// # Errors
///
/// [`JobError::Retry`] on the first database failure.
async fn delete_batch(state: &AppState, batch: &[Status]) -> Result<(), JobError> {
    for row in batch {
        delete(state, row).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, Utc};
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        jobs::QUEUE_BACKGROUND,
        models::{
            notification::{self, NewNotification},
            status::NewStatus,
            user::User,
        },
        test_fixtures::{local_user as user, remote_user},
        test_support::app_state,
    };

    /// A public post by `author` that expires `secs` from now (negative:
    /// its moment has already run out), or never when `secs` is `None`.
    async fn post_expiring_in(
        pool: &PgPool,
        author: &User,
        text: &str,
        secs: Option<i64>,
    ) -> Status {
        let new = NewStatus {
            expires_at: secs.map(|secs| Utc::now() + TimeDelta::seconds(secs)),
            ..NewStatus::local(author, text)
        };
        status::insert(pool, TEST_BASE_URL, &new)
            .await
            .expect("the status fixture inserts")
    }

    /// A `favourite` notification of `recipient` by `actor` about `row`.
    async fn favourited(pool: &PgPool, recipient: &User, actor: &User, row: &Status) {
        let _fixture = notification::create(
            pool,
            &NewNotification {
                user_id: recipient.id,
                from_account_id: actor.id,
                notification_type: "favourite",
                status_id: Some(row.id),
                emoji: None,
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the notification fixture inserts");
    }

    /// How many notifications name `status_id`.
    async fn notification_count(pool: &PgPool, status_id: Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM notifications WHERE status_id = $1")
            .bind(status_id)
            .fetch_one(pool)
            .await
            .expect("the count reads")
    }

    /// The stored row behind `id`, tombstone included.
    async fn stored(pool: &PgPool, id: Uuid) -> Option<Status> {
        status::get_by_id_including_deleted(pool, id)
            .await
            .expect("the read runs")
    }

    /// How many `jobs` rows of `kind` the table holds.
    async fn job_count(pool: &PgPool, kind: &str) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE kind = $1")
            .bind(kind)
            .fetch_one(pool)
            .await
            .expect("the count reads")
    }

    /// The handler's attempt info: the first of however many the job gets.
    const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };

    #[tokio::test]
    async fn an_expiring_post_schedules_one_job_due_at_its_expiry() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let row = post_expiring_in(&pool, &alice, "gone in an hour", Some(3_600)).await;
        let state = app_state(&pool);

        schedule_expiration(&state, &row).await;
        // The dedupe key swallows the second claim, so a replayed create
        // cannot queue the same purge twice.
        schedule_expiration(&state, &row).await;

        let rows = sqlx::query_as::<_, crate::models::job::Job>(
            "SELECT * FROM jobs WHERE kind = $1 ORDER BY id",
        )
        .bind(KIND_EXPIRE)
        .fetch_all(&pool)
        .await
        .expect("the job rows read");
        assert_eq!(rows.len(), 1, "one job per expiring status");
        let job = &rows[0];
        assert_eq!(job.queue, QUEUE_SCHEDULED, "on the scheduled queue");
        assert_eq!(
            job.args,
            job_args(row.id),
            "the args name exactly the status id"
        );
        assert_eq!(
            job.args["status_id"],
            row.id.to_string(),
            "the key is status_id"
        );
        assert_eq!(
            job.run_at,
            row.expires_at.unwrap_or_else(Utc::now),
            "due at the stored expiry, to the microsecond"
        );
        assert_eq!(
            job.unique_key.as_deref(),
            Some(unique_key(row.id).as_str()),
            "the ticket's dedupe key"
        );
    }

    #[tokio::test]
    async fn a_post_with_no_expiry_schedules_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let row = post_expiring_in(&pool, &alice, "stays", None).await;

        schedule_expiration(&app_state(&pool), &row).await;

        let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE kind = $1")
            .bind(KIND_EXPIRE)
            .fetch_one(&pool)
            .await
            .expect("the count reads");
        assert_eq!(rows, 0, "a plain post never scheduled a purge");
    }

    #[tokio::test]
    async fn the_handler_deletes_the_expiring_status_and_fires_the_hook() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let row = post_expiring_in(&pool, &alice, "time is up", Some(60)).await;
        favourited(&pool, &alice, &bob, &row).await;

        let outcome = handle_expire(app_state(&pool), FIRST_ATTEMPT, job_args(row.id)).await;

        assert_eq!(outcome, Ok(()), "expiring a live status succeeds");
        let tombstone = stored(&pool, row.id)
            .await
            .expect("the row is still stored");
        assert!(
            tombstone.deleted_at.is_some(),
            "the job deleted it like the endpoint would"
        );
        assert_eq!(tombstone.content, "", "with the text cleared");
        assert_eq!(
            notification_count(&pool, row.id).await,
            0,
            "and the deletion hook took its notifications with it"
        );
    }

    #[tokio::test]
    async fn a_deleted_status_completes_the_job_silently() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let row = post_expiring_in(&pool, &alice, "hand-deleted first", Some(60)).await;
        favourited(&pool, &alice, &bob, &row).await;
        let _tombstone = status::soft_delete_with_reblogs(&pool, row.id)
            .await
            .expect("the fixture delete runs");

        let outcome = handle_expire(app_state(&pool), FIRST_ATTEMPT, job_args(row.id)).await;

        assert_eq!(outcome, Ok(()), "there is nothing left to expire");
        let tombstone = stored(&pool, row.id)
            .await
            .expect("the row is still stored");
        assert_eq!(
            notification_count(&pool, row.id).await,
            1,
            "the no-op re-ran nothing: the fixture's delete is what is stored"
        );
        assert!(
            tombstone.deleted_at.is_some(),
            "and the row is the same tombstone"
        );
    }

    #[tokio::test]
    async fn a_missing_status_completes_the_job_silently() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        let outcome =
            handle_expire(app_state(&pool), FIRST_ATTEMPT, job_args(Uuid::now_v7())).await;

        assert_eq!(outcome, Ok(()), "an id that names nothing is done work");
    }

    #[tokio::test]
    async fn unusable_arguments_discard_the_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        let outcome = handle_expire(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        match outcome {
            Err(JobError::Discard(detail)) => assert!(
                detail.starts_with("status expire job arguments are unusable"),
                "the discard states why: {detail}"
            ),
            Ok(()) => panic!("bad arguments must not complete the job"),
            Err(other) => panic!("bad arguments are permanent, not {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_sweep_deletes_expired_posts_and_leaves_the_rest() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let lapsed = post_expiring_in(&pool, &alice, "already due", Some(-60)).await;
        let boundary = post_expiring_in(&pool, &alice, "due right now", Some(0)).await;
        let future = post_expiring_in(&pool, &alice, "not yet", Some(3_600)).await;
        let immortal = post_expiring_in(&pool, &alice, "never", None).await;

        let outcome = handle_sweep(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "a sweep of lapsed rows succeeds");
        for gone in [&lapsed, &boundary] {
            let tombstone = stored(&pool, gone.id)
                .await
                .expect("the row is still stored");
            assert!(
                tombstone.deleted_at.is_some(),
                "the status whose expiry ran out is a tombstone"
            );
        }
        for live in [&future, &immortal] {
            assert!(
                status::get_by_id(&pool, live.id)
                    .await
                    .expect("the read runs")
                    .is_some(),
                "a future expiry or no expiry is left alone"
            );
        }
    }

    /// A remote public post whose `expires_at` ran out a minute ago, the row
    /// the old parser wrote for every closed remote poll.
    async fn lapsed_remote_post(pool: &PgPool) -> Status {
        let bob = remote_user(pool, "bob").await;
        let new = NewStatus {
            user_id: bob.id,
            uri: Some("https://remote.example/objects/lapsed".to_owned()),
            content: "<p>which?</p>".to_owned(),
            expires_at: Some(Utc::now() - TimeDelta::seconds(60)),
            ..NewStatus::default()
        };
        status::insert(pool, TEST_BASE_URL, &new)
            .await
            .expect("the remote fixture inserts")
    }

    #[tokio::test]
    async fn the_sweep_never_tombstones_a_remote_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let remote = lapsed_remote_post(&pool).await;
        assert!(!remote.local, "the fixture is a remote status");

        let outcome = handle_sweep(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "a sweep with nothing local succeeds");
        assert!(
            status::get_by_id(&pool, remote.id)
                .await
                .expect("the read runs")
                .is_some(),
            "a remote status with a past expiry stays live"
        );
    }

    #[tokio::test]
    async fn the_expire_job_never_tombstones_a_remote_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let remote = lapsed_remote_post(&pool).await;

        let outcome = handle_expire(app_state(&pool), FIRST_ATTEMPT, job_args(remote.id)).await;

        assert_eq!(outcome, Ok(()), "the job completes");
        assert!(
            status::get_by_id(&pool, remote.id)
                .await
                .expect("the read runs")
                .is_some(),
            "and leaves the remote status live"
        );
    }

    /// More expired rows than one batch holds: the sweep must walk every
    /// batch, not stop after the first [`SWEEP_BATCH`].
    #[tokio::test]
    async fn the_sweep_walks_past_a_full_batch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let mut ids = Vec::new();
        for n in 0..SWEEP_BATCH + 3 {
            let row = post_expiring_in(&pool, &alice, &format!("lapsed {n}"), Some(-1)).await;
            ids.push(row.id);
        }

        let outcome = handle_sweep(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "a backlog spanning batches succeeds");
        let live = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM statuses WHERE id = ANY($1) AND deleted_at IS NULL",
        )
        .bind(&ids)
        .fetch_one(&pool)
        .await
        .expect("the count reads");
        assert_eq!(live, 0, "every lapsed row is a tombstone");
    }

    #[tokio::test]
    async fn an_unreachable_database_retries_both_handlers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let broken = {
            let unusable = sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://localhost/unused")
                .expect("a lazy pool needs no database");
            unusable.close().await;
            app_state(&unusable)
        };

        let expire = handle_expire(broken.clone(), FIRST_ATTEMPT, job_args(Uuid::now_v7())).await;
        let sweep = handle_sweep(broken, FIRST_ATTEMPT, json!({})).await;

        for outcome in [expire, sweep] {
            match outcome {
                Err(JobError::Retry(detail)) => assert!(
                    detail.starts_with("status expiration pass failed"),
                    "the retry states what failed: {detail}"
                ),
                other => panic!("a closed pool is retryable, not {other:?}"),
            }
        }
        let healthy = db.pool().clone();
        assert!(
            job_count(&healthy, KIND_EXPIRE).await == 0,
            "the healthy pool sees no work the broken one claimed to do"
        );
    }

    /// Both kinds are wired into the registries the ticket names: the worker
    /// can run them, and the sweep sits on the periodic schedule every five
    /// minutes.
    #[test]
    fn the_kinds_are_registered_and_the_sweep_is_scheduled() {
        let registry = crate::job_registry(&crate::config::Config::for_tests());
        assert!(
            registry.get(KIND_EXPIRE).is_some(),
            "the worker knows `{KIND_EXPIRE}`"
        );
        assert!(
            registry.get(KIND_SWEEP).is_some(),
            "the worker knows `{KIND_SWEEP}`"
        );

        let scheduled = crate::jobs::scheduler::periodic_jobs()
            .into_iter()
            .find(|job| job.kind == KIND_SWEEP)
            .expect("the sweep is on the periodic schedule");
        assert_eq!(scheduled, sweep_job(), "from the registry entry here");
        assert_eq!(
            scheduled.queue, QUEUE_BACKGROUND,
            "on the maintenance queue"
        );
        assert_eq!(
            scheduled.every,
            std::time::Duration::from_secs(300),
            "once every five minutes"
        );
    }
}
