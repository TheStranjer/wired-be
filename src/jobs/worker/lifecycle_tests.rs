//! Worker lifecycle tests: panics, shutdown, configuration, idle polling
//! and the stuck-lock sweep.

use std::{sync::atomic::Ordering, time::Duration};

use chrono::TimeDelta;
use serde_json::json;
use tokio::sync::oneshot;

use super::*;
use crate::{
    jobs::{
        QUEUE_DEFAULT,
        test_support::{
            HANDLER_STARTED, TEST_IDLE_POLL, fetch, ok_handler, panicking_handler, run_until_state,
            slow_handler, test_job, wait_for_state, worker_for,
        },
    },
    models::job::{NewJob, enqueue},
    test_support::app_state,
};

#[tokio::test]
async fn panicking_handler_retries_and_the_worker_keeps_going() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let boom = enqueue(&pool, test_job("boom"))
        .await
        .unwrap()
        .expect("the crashing job inserts");
    let mut registry = JobRegistry::new();
    registry.register("boom", panicking_handler);
    registry.register("greet", ok_handler);
    let worker = worker_for(&pool, registry, 1);

    // The shutdown future doubles as a script: once the crash has left
    // `boom` retryable, enqueue a fresh job and require this same worker
    // to complete it. A worker killed by the panic could not, and the
    // deadline inside `wait_for_state` would fail the test.
    let boom_id = boom.id;
    let script_pool = pool.clone();
    worker
        .run(async move {
            wait_for_state(&script_pool, boom_id, "retryable").await;
            let late = enqueue(&script_pool, test_job("greet"))
                .await
                .unwrap()
                .expect("the follow-up job inserts");
            wait_for_state(&script_pool, late.id, "completed").await;
        })
        .await;

    let crashed = fetch(&pool, boom_id).await;
    assert_eq!(
        crashed.state, "retryable",
        "a panicked handler is a retryable failure"
    );
    let message = crashed.last_error.unwrap_or_default();
    assert!(
        message.contains("boom") && message.contains("panicked"),
        "the error names the kind and the crash: {message}"
    );
    assert!(
        crashed.run_at > crashed.updated_at + TimeDelta::seconds(5),
        "the retry waits out the backoff"
    );
}

#[tokio::test]
async fn shutdown_waits_for_in_flight_jobs() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    HANDLER_STARTED.store(false, Ordering::SeqCst);
    let job = enqueue(&pool, test_job("slow"))
        .await
        .unwrap()
        .expect("the job inserts");
    let mut registry = JobRegistry::new();
    registry.register("slow", slow_handler);
    let worker = worker_for(&pool, registry, 1);

    let (tx, rx) = oneshot::channel::<()>();
    let running = tokio::spawn(worker.run(async move {
        let _signal = rx.await.is_ok();
    }));
    wait_started().await;
    tx.send(()).unwrap();
    running.await.unwrap();

    let done = fetch(&pool, job.id).await;
    assert_eq!(
        done.state, "completed",
        "run() returned only after the in-flight job recorded its outcome"
    );
}

/// Waits until [`slow_handler`] has announced that it started running.
async fn wait_started() {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !HANDLER_STARTED.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the handler never started");
        sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn worker_uses_the_queues_and_concurrency_from_config() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let job = enqueue(
        &pool,
        NewJob {
            queue: QUEUE_DEFAULT,
            kind: "greet",
            args: json!({}),
            run_at: None,
            max_attempts: None,
            unique_key: None,
        },
    )
    .await
    .unwrap()
    .expect("the job inserts");
    let mut registry = JobRegistry::new();
    registry.register("greet", ok_handler);
    let queues = state.config.job_queues.clone();
    let concurrency = state.config.job_concurrency;
    let worker = Worker::new(state, Arc::new(registry), queues, concurrency)
        .with_idle_poll_interval(TEST_IDLE_POLL);

    let done = run_until_state(worker, &pool, job.id, "completed").await;
    assert_eq!(
        done.state, "completed",
        "the config-shaped worker drained the queue"
    );
}

#[tokio::test]
async fn shutdown_during_an_idle_sleep_returns_immediately() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    // Nothing is ever enqueued, so after its first empty claim round the
    // worker sits in the default one-second idle sleep; the shutdown
    // below at 100 ms always lands mid-idle.
    let worker = Worker::new(
        app_state(&pool),
        Arc::new(JobRegistry::new()),
        vec!["test".to_owned()],
        1,
    );

    let started = Instant::now();
    // Generous bound, still below the idle poll interval: the worker
    // must return while its first idle sleep is counting down, not
    // after it has run its course.
    let ran = tokio::time::timeout(
        Duration::from_millis(800),
        worker.run(sleep(Duration::from_millis(100))),
    )
    .await;
    let elapsed = started.elapsed();
    assert!(
        ran.is_ok(),
        "shutdown mid-idle waited out the sleep: run() had not returned \
         after {elapsed:?}, against a {IDLE_POLL_INTERVAL:?} idle poll"
    );
    assert!(
        elapsed < IDLE_POLL_INTERVAL,
        "run() took {elapsed:?} to notice shutdown, well into the \
         {IDLE_POLL_INTERVAL:?} idle sleep it was supposed to interrupt"
    );
}

#[tokio::test]
async fn idle_poll_interval_is_honoured_for_work_enqueued_while_idle() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let mut registry = JobRegistry::new();
    registry.register("greet", ok_handler);
    let worker = Worker::new(
        app_state(&pool),
        Arc::new(registry),
        vec!["test".to_owned()],
        1,
    )
    .with_idle_poll_interval(Duration::from_millis(20));

    // Go idle on the empty queue for several short rounds, then enqueue:
    // the job must be claimed and completed within 600 ms — a dozen
    // rounds at the 20 ms interval, but far inside the one-second
    // default the builder is meant to have replaced. Were the field
    // ignored, the claim after the enqueue could not happen before the
    // first default sleep expired and this would time out.
    let ran = tokio::time::timeout(
        Duration::from_millis(600),
        worker.run(async move {
            sleep(Duration::from_millis(100)).await;
            let late = enqueue(&pool, test_job("greet"))
                .await
                .unwrap()
                .expect("the follow-up job inserts");
            wait_for_state(&pool, late.id, "completed").await;
        }),
    )
    .await;
    assert!(
        ran.is_ok(),
        "a job enqueued into an idling worker was not drained within \
         600 ms: the 20 ms idle poll interval was not honoured"
    );
}

#[tokio::test]
async fn sweep_leaves_fresh_locks_alone() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let job = enqueue(&pool, test_job("held"))
        .await
        .unwrap()
        .expect("the job inserts");
    let claimed = claim_next(&pool, &["test"])
        .await
        .unwrap()
        .expect("the job is due and claimable");
    assert_eq!(claimed.id, job.id, "the held job was claimed");

    sweep_stuck(&app_state(&pool)).await;

    let still = fetch(&pool, job.id).await;
    assert_eq!(
        still.state, "executing",
        "a lock younger than the stuck age is left alone"
    );
}
