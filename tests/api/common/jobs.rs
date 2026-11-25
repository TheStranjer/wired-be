//! The synchronous job drain.

use uuid::Uuid;
use wired_be::{
    job_registry,
    jobs::{AttemptInfo, JobError, JobRegistry, backoff},
    models::job::{Job, claim_next, complete, fail},
    state::AppState,
};

/// Drains the job queues of `state` synchronously and returns how many
/// jobs it ran.
///
/// The one-shot twin of [`wired_be::jobs::Worker`]: it loops
/// [`claim_next`] over [`config.job_queues`](wired_be::config::Config::job_queues),
/// hands each claimed job to the handler [`job_registry`] binds for its
/// kind — with the claim's own [`AttemptInfo`] and the row's `args` — and
/// writes the outcome back through [`complete`] or [`fail`], exactly as
/// the worker does: a retryable failure moves `run_at` into the future, so
/// the drain ends once nothing is due. A job kind no handler serves is
/// discarded, never requeued. Tests call this after an HTTP request
/// enqueued the work (backdating a time-gated row's `run_at` first, since
/// a claim only takes due jobs).
///
/// # Panics
///
/// If a claim or a terminal write fails, or a handler panics.
pub async fn run_all_jobs(state: &AppState) -> usize {
    let registry = job_registry(&state.config);
    let queues: Vec<&str> = state.config.job_queues.iter().map(String::as_str).collect();
    let mut run = 0;
    while let Some(job) = claim_next(&state.db, &queues)
        .await
        .expect("a job claim runs")
    {
        run_one(state, &registry, job).await;
        run += 1;
    }
    run
}

/// Runs one claimed `job` through its registered handler and records the
/// outcome, mirroring the worker's dispatch: [`AttemptInfo`] built from
/// the claim's own counters, the row's `args` handed over verbatim, and an
/// unknown kind discarded like [`wired_be::jobs::Worker`] discards it.
async fn run_one(state: &AppState, registry: &JobRegistry, job: Job) {
    let Job {
        id,
        kind,
        args,
        attempts,
        max_attempts,
        ..
    } = job;
    let attempt = AttemptInfo {
        attempts,
        max_attempts,
    };
    let outcome = match registry.get(&kind) {
        Some(handler) => handler(state.clone(), attempt, args).await,
        None => Err(JobError::Discard(format!(
            "no handler registered for job kind '{kind}'"
        ))),
    };
    record_outcome(state, id, attempts, outcome).await;
}

/// Moves a finished attempt to its terminal state the way
/// [`wired_be::jobs`] does: `completed` on success, retryable with the
/// queue's own [`backoff`] (or the handler's) delay on a transient failure,
/// discarded on a fatal one.
///
/// # Panics
///
/// If the terminal write fails.
async fn record_outcome(state: &AppState, id: Uuid, attempts: i32, outcome: Result<(), JobError>) {
    let stored = match outcome {
        Ok(()) => complete(&state.db, id, attempts).await,
        Err(JobError::Retry(detail)) => {
            fail(&state.db, id, attempts, &detail, Some(backoff(attempts))).await
        }
        Err(JobError::RetryAfter(delay, detail)) => {
            fail(&state.db, id, attempts, &detail, Some(delay)).await
        }
        Err(JobError::Discard(detail)) => fail(&state.db, id, attempts, &detail, None).await,
    };
    let _written = stored.expect("the terminal job write runs");
}
