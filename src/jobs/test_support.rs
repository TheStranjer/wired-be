//! Fixtures the job runtime tests share: test state, job rows, queue
//! helpers and a family of canned handlers.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

use super::{AttemptInfo, HandlerFuture, JobError, JobRegistry, Worker};
use crate::{
    config::Config,
    models::job::{Job, NewJob},
    state::AppState,
};

/// Set once by [`marking_handler`] when its body runs.
pub(super) static HANDLER_RAN: AtomicBool = AtomicBool::new(false);
/// Set once by [`slow_handler`] before it pauses.
pub(super) static HANDLER_STARTED: AtomicBool = AtomicBool::new(false);
/// What [`reporting_handler`] was told about its attempt.
pub(super) static LAST_ATTEMPT: Mutex<Option<AttemptInfo>> = Mutex::new(None);

/// Test state over `pool` with the fixed test config.
pub(super) fn app_state(pool: &PgPool) -> AppState {
    AppState::for_tests(pool.clone(), Config::for_tests())
}

/// Idle-poll interval for worker tests: short enough that a claim round
/// after an empty one costs milliseconds, so tests never sit through the
/// production second.
pub(super) const TEST_IDLE_POLL: Duration = Duration::from_millis(10);

/// A worker over the `test` queue with `concurrency` slots and a fast
/// idle poll.
pub(super) fn worker_for(pool: &PgPool, registry: JobRegistry, concurrency: usize) -> Worker {
    Worker::new(
        app_state(pool),
        Arc::new(registry),
        vec!["test".to_owned()],
        concurrency,
    )
    .with_idle_poll_interval(TEST_IDLE_POLL)
}

/// A job payload on the `test` queue with `kind`.
pub(super) fn test_job(kind: &'static str) -> NewJob<'static> {
    NewJob {
        queue: "test",
        kind,
        args: json!({}),
        run_at: None,
        max_attempts: None,
        unique_key: None,
    }
}

/// A bare in-memory job row of `kind`, for handler-level tests.
pub(super) fn job_row(kind: &str) -> Job {
    let now = Utc::now();
    Job {
        id: Uuid::now_v7(),
        queue: "test".to_owned(),
        kind: kind.to_owned(),
        args: json!({}),
        state: "executing".to_owned(),
        attempts: 1,
        max_attempts: 5,
        run_at: now,
        locked_at: Some(now),
        last_error: None,
        unique_key: None,
        inserted_at: now,
        updated_at: now,
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

/// Resolves once the stored state of job `id` is `want`, failing the
/// test if that never happens within 10 seconds.
pub(super) async fn wait_for_state(pool: &PgPool, id: Uuid, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while fetch(pool, id).await.state != want {
        assert!(Instant::now() < deadline, "job never reached state {want}");
        sleep(Duration::from_millis(20)).await;
    }
}

/// Runs `worker` until job `id` reports state `want` (the wait doubles
/// as the shutdown signal), then returns the stored row.
pub(super) async fn run_until_state(worker: Worker, pool: &PgPool, id: Uuid, want: &str) -> Job {
    worker.run(wait_for_state(pool, id, want)).await;
    fetch(pool, id).await
}

/// A handler that always succeeds.
pub(super) fn ok_handler(_state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async { Ok(()) })
}

/// A handler that records that it ran, then succeeds.
pub(super) fn marking_handler(
    _state: AppState,
    _attempt: AttemptInfo,
    _args: Value,
) -> HandlerFuture {
    Box::pin(async {
        HANDLER_RAN.store(true, Ordering::SeqCst);
        Ok(())
    })
}

/// A handler that reports its [`AttemptInfo`] and succeeds, so a test can
/// read what the dispatch handed it.
pub(super) fn reporting_handler(
    _state: AppState,
    attempt: AttemptInfo,
    _args: Value,
) -> HandlerFuture {
    Box::pin(async move {
        let _previous = LAST_ATTEMPT.lock().unwrap().replace(attempt);
        Ok(())
    })
}

/// A handler that reports a transient failure.
pub(super) fn retry_handler(
    _state: AppState,
    _attempt: AttemptInfo,
    _args: Value,
) -> HandlerFuture {
    Box::pin(async { Err(JobError::Retry("temporarily broken".to_owned())) })
}

/// A handler that reports a transient failure with the peer's own delay:
/// half a minute, long enough that the retry cannot fire while a test is
/// still watching the job's first terminal write.
pub(super) fn retry_after_handler(
    _state: AppState,
    _attempt: AttemptInfo,
    _args: Value,
) -> HandlerFuture {
    Box::pin(async {
        Err(JobError::RetryAfter(
            Duration::from_secs(30),
            "peer asked to wait".to_owned(),
        ))
    })
}

/// A handler that reports a permanent failure.
pub(super) fn discard_handler(
    _state: AppState,
    _attempt: AttemptInfo,
    _args: Value,
) -> HandlerFuture {
    Box::pin(async { Err(JobError::Discard("bad arguments".to_owned())) })
}

/// A handler that crashes its task mid-flight.
pub(super) fn panicking_handler(
    _state: AppState,
    _attempt: AttemptInfo,
    _args: Value,
) -> HandlerFuture {
    Box::pin(async {
        let _boom = "not-a-number".parse::<u64>().unwrap();
        Ok(())
    })
}

/// A handler that never finishes.
pub(super) fn pending_handler(
    _state: AppState,
    _attempt: AttemptInfo,
    _args: Value,
) -> HandlerFuture {
    Box::pin(std::future::pending())
}

/// A handler that announces its start and then runs for a moment.
pub(super) fn slow_handler(_state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async {
        HANDLER_STARTED.store(true, Ordering::SeqCst);
        sleep(Duration::from_millis(400)).await;
        Ok(())
    })
}
