//! The prune pass's tests: which terminal job rows a daily run reclaims, the
//! batching loop that drains a backlog larger than one batch, that the
//! `received_activities` ledger is left untouched, the retry a database it
//! cannot reach buys it, and the two registries its kind is wired into.

use serde_json::json;
use sqlx::query_scalar;
use uuid::Uuid;

use super::*;
use crate::{
    jobs::{
        QUEUE_BACKGROUND,
        maintenance::test_support::{EVERY, FIRST_ATTEMPT, app_state, broken_state, row_exists},
        scheduler::periodic_jobs,
    },
    models::job::{NewJob, enqueue},
};

/// The `kind` the multi-batch fixture stamps its rows, so the test can
/// count exactly its own rows back out of the table.
const BULK_KIND: &str = "prune.bulk";

/// Enqueues a job on the background queue through the very
/// [`enqueue`] the producers use, then ages it: `state` and an
/// `updated_at` `days` in the past, written with the `UPDATE` the
/// worker's own transitions would have stamped. Returns the row id.
async fn aged_job(pool: &PgPool, kind: &str, state: &str, days: i32) -> Uuid {
    let job = enqueue(
        pool,
        NewJob {
            queue: QUEUE_BACKGROUND,
            kind,
            args: json!({}),
            run_at: None,
            max_attempts: None,
            unique_key: None,
        },
    )
    .await
    .expect("the fixture job enqueues")
    .expect("an unkeyed enqueue is never deduplicated away");
    let sql = "UPDATE jobs SET state = $2, updated_at = now() - make_interval(days => $3)
                   WHERE id = $1";
    let written = query(sql)
        .bind(job.id)
        .bind(state)
        .bind(days)
        .execute(pool)
        .await
        .expect("the fixture ages the job");
    assert_eq!(written.rows_affected(), 1, "one row was rewritten");
    job.id
}

/// Seeds `count` stale terminal rows: [`aged_job`]'s end state, reached
/// in two set-based statements because a multi-batch backlog is too
/// large to name one enqueue at a time.
async fn seed_many_stale(pool: &PgPool, count: i64) {
    let inserted = query(
        "INSERT INTO jobs (id, queue, kind, args, state, updated_at)
             SELECT gen_random_uuid(), 'background', $1, '{}'::jsonb, 'available', now()
               FROM generate_series(1, $2)",
    )
    .bind(BULK_KIND)
    .bind(count)
    .execute(pool)
    .await
    .expect("the bulk fixture inserts");
    assert_eq!(
        inserted.rows_affected(),
        u64::try_from(count).expect("the fixture seeds a positive count"),
        "every seeded row landed in the table"
    );
    let aged = query(
        "UPDATE jobs SET state = 'completed', updated_at = now() - make_interval(days => 8)
             WHERE kind = $1",
    )
    .bind(BULK_KIND)
    .execute(pool)
    .await
    .expect("the bulk fixture ages its rows");
    assert_eq!(
        aged.rows_affected(),
        u64::try_from(count).expect("the fixture ages a positive count"),
        "every seeded row turned stale and terminal"
    );
}

/// How many of the bulk-seeded rows are still in the table.
async fn surviving_bulk_rows(pool: &PgPool) -> i64 {
    query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE kind = $1")
        .bind(BULK_KIND)
        .fetch_one(pool)
        .await
        .expect("the rows read")
}

/// The jobs prune deletes exactly the stale terminal rows: a `completed`
/// or `discarded` row past the week goes, a fresh terminal row and every
/// live-state row — however old — stay.
#[tokio::test]
async fn the_jobs_prune_deletes_only_stale_terminal_rows() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let stale_completed = aged_job(&pool, "done.stale", "completed", 8).await;
    let stale_discarded = aged_job(&pool, "dead.stale", "discarded", 8).await;
    let fresh_completed = aged_job(&pool, "done.fresh", "completed", 1).await;
    let stale_available = aged_job(&pool, "wait.stale", "available", 8).await;
    let stale_retryable = aged_job(&pool, "retry.stale", "retryable", 8).await;
    let stale_executing = aged_job(&pool, "run.stale", "executing", 8).await;

    let outcome = handle_jobs(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "a prune over stale and live rows succeeds");
    assert!(
        !row_exists(&pool, "jobs", stale_completed).await,
        "a completed row past the week is gone"
    );
    assert!(
        !row_exists(&pool, "jobs", stale_discarded).await,
        "a discarded row past the week is gone"
    );
    assert!(
        row_exists(&pool, "jobs", fresh_completed).await,
        "a completed row inside the week stays"
    );
    assert!(
        row_exists(&pool, "jobs", stale_available).await,
        "a stale available row is live work, not garbage"
    );
    assert!(
        row_exists(&pool, "jobs", stale_retryable).await,
        "a stale retryable row stays claimable"
    );
    assert!(
        row_exists(&pool, "jobs", stale_executing).await,
        "a stale executing row may still have a worker on it"
    );
}

/// A backlog larger than one batch is cleared by a single handler run:
/// the loop keeps deleting until a round comes up short, it is not one
/// batch per run.
#[tokio::test]
async fn the_jobs_prune_loops_until_the_backlog_is_gone() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let backlog = PRUNE_BATCH_SIZE + 5;
    seed_many_stale(&pool, backlog).await;

    let outcome = handle_jobs(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "the multi-batch pass succeeds");
    assert_eq!(
        surviving_bulk_rows(&pool).await,
        0,
        "one handler run cleared more than {PRUNE_BATCH_SIZE} rows: \
             the loop ran, not a single batch"
    );
}

/// How many `received_activities` rows carry `ap_id`.
async fn ledger_rows(pool: &PgPool, ap_id: &str) -> i64 {
    query_scalar::<_, i64>("SELECT count(*) FROM received_activities WHERE ap_id = $1")
        .bind(ap_id)
        .fetch_one(pool)
        .await
        .expect("the ledger reads")
}

/// The jobs prune does not touch the replay ledger: `received_activities`
/// rows of any age survive the pass, whose retention belongs to the
/// dispatcher's `federation.cleanup_received`, while the stale terminal
/// job beside them is still deleted.
#[tokio::test]
async fn the_jobs_prune_leaves_the_received_activities_ledger_alone() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let _aged = query(
        "INSERT INTO received_activities (ap_id, inserted_at)
             VALUES ('https://peer.example/activities/old', now() - make_interval(days => 400))",
    )
    .execute(&pool)
    .await
    .expect("the old ledger row inserts");
    let _fresh = query(
        "INSERT INTO received_activities (ap_id, inserted_at)
             VALUES ('https://peer.example/activities/new', now() - make_interval(days => 1))",
    )
    .execute(&pool)
    .await
    .expect("the fresh ledger row inserts");
    let stale = aged_job(&pool, "done.stale", "completed", 8).await;
    let live = aged_job(&pool, "wait.live", "available", 8).await;

    let outcome = handle_jobs(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "the pass runs");
    assert_eq!(
        ledger_rows(&pool, "https://peer.example/activities/old").await,
        1,
        "a 400-day-old ledger row is not this job's to reclaim"
    );
    assert_eq!(
        ledger_rows(&pool, "https://peer.example/activities/new").await,
        1,
        "a fresh ledger row stays"
    );
    assert!(
        !row_exists(&pool, "jobs", stale).await,
        "the pass still prunes the stale terminal job"
    );
    assert!(
        row_exists(&pool, "jobs", live).await,
        "a live-state job stays however old"
    );
}

/// The jobs prune is wired into both registries the ticket names: the
/// worker can run its kind, and the scheduler places it on the
/// maintenance queue once a day.
#[test]
fn the_jobs_kind_is_registered_and_scheduled() {
    let registry = crate::job_registry();
    assert!(
        registry.get(KIND_JOBS).is_some(),
        "the worker knows `{KIND_JOBS}`"
    );

    let job = periodic_jobs()
        .into_iter()
        .find(|job| job.kind == KIND_JOBS)
        .expect("the prune is on the periodic schedule");
    assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
    assert_eq!(job.every, EVERY, "once a day");
}

/// The handler the registry binds to the kind is this module's
/// [`handle_jobs`], not some other sweep: calling the registered
/// handler deletes the stale terminal rows and keeps the fresh ones.
#[tokio::test]
async fn the_registered_jobs_handler_is_this_prune() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let stale = aged_job(&pool, "done.stale", "completed", 8).await;
    let live = aged_job(&pool, "done.live", "completed", 1).await;
    let registry = crate::job_registry();
    let handler = registry
        .get(KIND_JOBS)
        .expect("the prune's kind is registered");

    let outcome = handler(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "the registered handler runs");
    assert!(
        !row_exists(&pool, "jobs", stale).await,
        "and it is this module's prune: the stale terminal row is gone"
    );
    assert!(
        row_exists(&pool, "jobs", live).await,
        "and only the stale rows went: the fresh terminal row stays"
    );
}

/// A prune that cannot reach the database is a retryable failure, so
/// the queue keeps the pass while the scheduler enqueues the next day:
/// the stale rows wait for a working prune instead of being assumed
/// gone.
#[tokio::test]
async fn an_unreachable_database_retries_the_jobs_prune() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let stale = aged_job(&pool, "done.stale", "completed", 8).await;
    let broken = broken_state().await;

    let outcome = handle_jobs(broken, FIRST_ATTEMPT, json!({})).await;

    match outcome {
        Err(JobError::Retry(detail)) => assert!(
            detail.starts_with("prune sweep failed"),
            "the retry states what failed: {detail}"
        ),
        Ok(()) => panic!("a closed pool must not read as a successful prune"),
        Err(other) => panic!("a closed pool is retryable, not {other:?}"),
    }
    assert!(
        row_exists(&pool, "jobs", stale).await,
        "the healthy pool sees no work the broken one claimed to do"
    );
}
