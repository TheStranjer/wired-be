//! Dispatch tests: what the worker does with each handler outcome.

use std::sync::atomic::Ordering;

use chrono::TimeDelta;

use super::*;
use crate::{
    config::Config,
    jobs::test_support::{
        HANDLER_RAN, LAST_ATTEMPT, discard_handler, job_row, marking_handler, ok_handler,
        panicking_handler, pending_handler, reporting_handler, retry_after_handler, retry_handler,
        run_until_state, test_job, worker_for,
    },
    models::job::enqueue,
};

#[test]
fn job_timeout_is_below_the_stuck_lock_age() {
    let (timeout, stuck_after) = (JOB_TIMEOUT, STUCK_LOCK_AGE);
    assert!(
        timeout < stuck_after,
        "a running handler must be aborted before the rescuer can re-queue it"
    );
}

#[tokio::test]
async fn a_handler_that_never_finishes_is_retried_after_the_timeout() {
    let state = AppState::lazy_for_tests(Config::for_tests());
    let mut registry = JobRegistry::new();
    registry.register("hung", pending_handler);

    let outcome = dispatch_handler(&registry, &state, &job_row("hung"), Duration::ZERO).await;

    match outcome {
        Err(JobError::Retry(msg)) => {
            assert!(msg.contains("exceeded"), "the message says why: {msg}");
            assert!(msg.contains("hung"), "the message names the kind: {msg}");
        }
        other => panic!("a hung handler must be retried, got {other:?}"),
    }
}

#[tokio::test]
async fn a_handler_that_finishes_within_the_timeout_succeeds() {
    let state = AppState::lazy_for_tests(Config::for_tests());
    let mut registry = JobRegistry::new();
    registry.register("quick", ok_handler);

    let outcome = dispatch_handler(
        &registry,
        &state,
        &job_row("quick"),
        Duration::from_secs(60),
    )
    .await;

    assert_eq!(outcome, Ok(()), "a fast handler is unaffected");
}

#[tokio::test]
async fn a_panicking_handler_is_retried() {
    let state = AppState::lazy_for_tests(Config::for_tests());
    let mut registry = JobRegistry::new();
    registry.register("boom", panicking_handler);

    let outcome =
        dispatch_handler(&registry, &state, &job_row("boom"), Duration::from_secs(60)).await;

    match outcome {
        Err(JobError::Retry(msg)) => {
            assert!(msg.contains("panicked"), "the message says why: {msg}");
        }
        other => panic!("a panic must map to Retry, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_runs_the_registered_handler() {
    let state = AppState::lazy_for_tests(Config::for_tests());
    let mut registry = JobRegistry::new();
    registry.register("flaky", retry_handler);
    let outcome = dispatch_handler(&registry, &state, &job_row("flaky"), JOB_TIMEOUT).await;
    assert_eq!(
        outcome,
        Err(JobError::Retry("temporarily broken".to_owned())),
        "the handler's error arrives unchanged"
    );
}

#[tokio::test]
async fn dispatch_discards_unknown_kinds() {
    let state = AppState::lazy_for_tests(Config::for_tests());
    let registry = JobRegistry::new();
    let outcome = dispatch_handler(&registry, &state, &job_row("mystery"), JOB_TIMEOUT).await;
    match outcome {
        Err(JobError::Discard(detail)) => {
            assert!(
                detail.contains("mystery"),
                "the discard message names the kind: {detail}"
            );
        }
        Err(retry) => panic!("an unknown kind must discard, not retry: {retry}"),
        Ok(()) => panic!("an unknown kind must not complete"),
    }
}

#[tokio::test]
async fn worker_runs_handler_to_completion() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    HANDLER_RAN.store(false, Ordering::SeqCst);
    let job = enqueue(&pool, test_job("greet"))
        .await
        .unwrap()
        .expect("the job inserts");
    let mut registry = JobRegistry::new();
    registry.register("greet", marking_handler);

    let done = run_until_state(worker_for(&pool, registry, 2), &pool, job.id, "completed").await;
    assert_eq!(done.attempts, 1, "the run consumed a single attempt");
    assert!(done.last_error.is_none(), "a successful job keeps no error");
    assert!(done.locked_at.is_none(), "completing releases the lock");
    assert!(
        HANDLER_RAN.load(Ordering::SeqCst),
        "the registered handler ran"
    );
}

#[tokio::test]
async fn retry_error_makes_the_job_retryable_later() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let job = enqueue(&pool, test_job("flaky"))
        .await
        .unwrap()
        .expect("the job inserts");
    let mut registry = JobRegistry::new();
    registry.register("flaky", retry_handler);

    let done = run_until_state(worker_for(&pool, registry, 1), &pool, job.id, "retryable").await;
    assert_eq!(
        done.last_error.as_deref(),
        Some("temporarily broken"),
        "the retry message is stored"
    );
    assert_eq!(done.attempts, 1, "the failed attempt is counted");
    assert!(done.locked_at.is_none(), "failing releases the lock");
    assert!(
        done.run_at > done.updated_at + TimeDelta::seconds(5),
        "the job is due again only after the 15 s backoff"
    );
}

#[tokio::test]
async fn dispatch_tells_the_handler_which_attempt_it_is() {
    let state = AppState::lazy_for_tests(Config::for_tests());
    let mut registry = JobRegistry::new();
    registry.register("greet", reporting_handler);
    let job = Job {
        attempts: 3,
        max_attempts: 7,
        ..job_row("greet")
    };

    let _drop = dispatch_handler(&registry, &state, &job, JOB_TIMEOUT).await;

    let seen = *LAST_ATTEMPT.lock().unwrap();
    assert_eq!(
        seen,
        Some(AttemptInfo {
            attempts: 3,
            max_attempts: 7,
        }),
        "the claimed row's attempt budget reaches the handler"
    );
}

#[tokio::test]
async fn retry_after_error_schedules_the_peers_delay() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let job = enqueue(&pool, test_job("throttled"))
        .await
        .unwrap()
        .expect("the job inserts");
    let mut registry = JobRegistry::new();
    registry.register("throttled", retry_after_handler);

    let done = run_until_state(worker_for(&pool, registry, 1), &pool, job.id, "retryable").await;
    assert_eq!(
        done.last_error.as_deref(),
        Some("peer asked to wait"),
        "the retry message is stored"
    );
    assert_eq!(done.attempts, 1, "the failed attempt is counted");
    assert!(
        done.run_at > done.updated_at + TimeDelta::seconds(20)
            && done.run_at < done.updated_at + TimeDelta::seconds(50),
        "the job is due at the peer's 30 s, not the 15 s backoff: {}",
        done.run_at
    );
}

#[tokio::test]
async fn discard_error_ends_the_job() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let job = enqueue(&pool, test_job("doomed"))
        .await
        .unwrap()
        .expect("the job inserts");
    let mut registry = JobRegistry::new();
    registry.register("doomed", discard_handler);

    let done = run_until_state(worker_for(&pool, registry, 1), &pool, job.id, "discarded").await;
    assert_eq!(
        done.last_error.as_deref(),
        Some("bad arguments"),
        "the discard message is stored"
    );
    assert_eq!(
        done.attempts, 1,
        "the attempt is counted even though it was the last"
    );
}

#[tokio::test]
async fn unknown_kind_is_discarded_with_a_message() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let job = enqueue(&pool, test_job("mystery"))
        .await
        .unwrap()
        .expect("the job inserts");
    let registry = JobRegistry::new();

    let done = run_until_state(worker_for(&pool, registry, 1), &pool, job.id, "discarded").await;
    let message = done.last_error.unwrap_or_default();
    assert!(
        message.contains("mystery"),
        "the error names the unknown kind: {message}"
    );
}
