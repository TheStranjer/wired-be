//! Dispatch tests: what the worker does with each handler outcome.

use std::sync::atomic::Ordering;

use chrono::TimeDelta;

use super::*;
use crate::{
    config::Config,
    http_client::HttpClient,
    jobs::test_support::{
        HANDLER_RAN, LAST_ATTEMPT, discard_handler, job_row, lazy_pool, marking_handler,
        reporting_handler, retry_after_handler, retry_handler, run_until_state, test_job,
        worker_for,
    },
    models::job::enqueue,
    uploads::UploaderBackend,
};

#[tokio::test]
async fn dispatch_runs_the_registered_handler() {
    let config = Arc::new(Config::for_tests());
    let db = lazy_pool();
    let state = AppState {
        hub: crate::streaming::hub::Hub::new(&db),
        db,
        config: Arc::clone(&config),
        http_client: HttpClient::from(config.as_ref()),
        uploader: UploaderBackend::from(config.as_ref()),
        instance_stats_cache: Arc::default(),
        mrf_config_cache: Arc::default(),
        translation_languages_cache: Arc::default(),
        rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
        mailer: Arc::new(crate::mailer::MemoryTransport::new()),
    };
    let mut registry = JobRegistry::new();
    registry.register("flaky", retry_handler);
    let outcome = dispatch_handler(&registry, &state, &job_row("flaky")).await;
    assert_eq!(
        outcome,
        Err(JobError::Retry("temporarily broken".to_owned())),
        "the handler's error arrives unchanged"
    );
}

#[tokio::test]
async fn dispatch_discards_unknown_kinds() {
    let config = Arc::new(Config::for_tests());
    let db = lazy_pool();
    let state = AppState {
        hub: crate::streaming::hub::Hub::new(&db),
        db,
        config: Arc::clone(&config),
        http_client: HttpClient::from(config.as_ref()),
        uploader: UploaderBackend::from(config.as_ref()),
        instance_stats_cache: Arc::default(),
        mrf_config_cache: Arc::default(),
        translation_languages_cache: Arc::default(),
        rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
        mailer: Arc::new(crate::mailer::MemoryTransport::new()),
    };
    let registry = JobRegistry::new();
    let outcome = dispatch_handler(&registry, &state, &job_row("mystery")).await;
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
    let config = Arc::new(Config::for_tests());
    let db = lazy_pool();
    let state = AppState {
        hub: crate::streaming::hub::Hub::new(&db),
        db,
        config: Arc::clone(&config),
        http_client: HttpClient::from(config.as_ref()),
        uploader: UploaderBackend::from(config.as_ref()),
        instance_stats_cache: Arc::default(),
        mrf_config_cache: Arc::default(),
        translation_languages_cache: Arc::default(),
        rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
        mailer: Arc::new(crate::mailer::MemoryTransport::new()),
    };
    let mut registry = JobRegistry::new();
    registry.register("greet", reporting_handler);
    let job = Job {
        attempts: 3,
        max_attempts: 7,
        ..job_row("greet")
    };

    let _drop = dispatch_handler(&registry, &state, &job).await;

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
