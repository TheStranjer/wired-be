//! The worker loop: claiming, dispatching and recording jobs, and the
//! sweep of stuck locks.

use std::{future::Future, sync::Arc, time::Duration};

use sqlx::PgPool;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Instant, MissedTickBehavior, interval_at, sleep},
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::{AttemptInfo, JobError, JobRegistry, backoff};
use crate::{
    models::job::{Job, claim_next, complete, fail, rescue_stuck},
    state::AppState,
};

/// Default pause between claim rounds when no job is due (keeps an idle
/// worker off the database without a busy loop). [`Worker`] stores this as
/// its `idle_poll_interval`; [`Worker::with_idle_poll_interval`] overrides
/// it.
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How often the running worker sweeps jobs stuck in `executing`.
const RESCUE_INTERVAL: Duration = Duration::from_secs(60);
/// An `executing` job locked for longer than this is considered stuck.
const STUCK_LOCK_AGE: Duration = Duration::from_mins(10);
/// Longest a handler may run before it is aborted and its job retried.
/// Invariant: strictly below [`STUCK_LOCK_AGE`], so the rescuer only ever
/// re-queues jobs whose worker died, never one whose handler is still running.
const JOB_TIMEOUT: Duration = Duration::from_mins(8);

/// Compile-time proof of the [`JOB_TIMEOUT`] invariant.
const _: () = assert!(
    JOB_TIMEOUT.as_secs() < STUCK_LOCK_AGE.as_secs(),
    "JOB_TIMEOUT must stay below STUCK_LOCK_AGE"
);

/// Drains the configured job queues until a shutdown signal arrives.
///
/// Each round claims the due job at the front of the queues (decision 11's
/// `FOR UPDATE SKIP LOCKED` core in [`crate::models::job`]) and runs its
/// handler on a task of its own; a [`Semaphore`] of `concurrency` permits
/// bounds how many handlers run at once. Nothing due costs a sleep of
/// [`idle_poll_interval`](Self::with_idle_poll_interval) that the shutdown
/// future interrupts at any time, expired locks are swept every minute,
/// and once the shutdown future resolves the worker stops claiming and
/// returns only after every in-flight job has reached a terminal state.
#[derive(Debug)]
pub struct Worker {
    /// Shared state handed to every handler.
    state: AppState,
    /// Kind to handler table consulted for each claimed job.
    registry: Arc<JobRegistry>,
    /// Queue names this worker drains.
    queues: Vec<String>,
    /// Maximum number of handlers running at once.
    concurrency: usize,
    /// Pause between claim rounds when nothing was due; the loop sleeps in
    /// interruptible increments of this, not inside a claim.
    idle_poll_interval: Duration,
}

impl Worker {
    /// Builds a worker over `queues` with at most `concurrency` handlers
    /// running at once and the default (one second) idle poll interval;
    /// see [`with_idle_poll_interval`](Self::with_idle_poll_interval).
    #[must_use]
    pub const fn new(
        state: AppState,
        registry: Arc<JobRegistry>,
        queues: Vec<String>,
        concurrency: usize,
    ) -> Self {
        Self {
            state,
            registry,
            queues,
            concurrency,
            idle_poll_interval: IDLE_POLL_INTERVAL,
        }
    }

    /// Sets the pause between claim rounds when nothing is due, replacing
    /// the default one second. The wait is interruptible either way, so a
    /// shorter interval only shortens how long newly enqueued work takes
    /// to be claimed; tests use this to keep claim rounds in milliseconds,
    /// while production keeps the default.
    #[must_use]
    pub const fn with_idle_poll_interval(mut self, interval: Duration) -> Self {
        self.idle_poll_interval = interval;
        self
    }

    /// Runs the claim-dispatch loop until `shutdown` resolves.
    ///
    /// On shutdown the loop stops claiming and waits for the jobs already in
    /// flight: each holds a semaphore permit until its terminal database
    /// write is done, so `run` returns only when every permit is back.
    /// Handler panics cost the job a retry; they never end this loop.
    pub async fn run<F>(self, shutdown: F)
    where
        F: Future<Output = ()>,
    {
        let Self {
            state,
            registry,
            queues,
            concurrency,
            idle_poll_interval,
        } = self;
        let semaphore = Arc::new(Semaphore::new(concurrency));
        let mut rescuer = interval_at(Instant::now() + RESCUE_INTERVAL, RESCUE_INTERVAL);
        rescuer.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut shutdown = Box::pin(shutdown);
        // The idle wait lives here, not inside a claim: `waiting` gates the
        // permit arm, and the pinned sleep is an arm of its own, so a
        // shutdown (or a rescuer tick) lands while the worker is idle
        // instead of after the interval has run its course.
        let mut idle = Box::pin(sleep(idle_poll_interval));
        let mut waiting = false;
        let mut draining = false;
        while !draining {
            let mut go_idle = false;
            tokio::select! {
                biased;
                () = &mut shutdown => draining = true,
                () = &mut idle, if waiting => waiting = false,
                _ = rescuer.tick() => sweep_stuck(&state).await,
                maybe_permit = Arc::clone(&semaphore).acquire_owned(), if !waiting => {
                    match maybe_permit {
                        // `claim_and_dispatch` returns quickly either way:
                        // an empty queue arms the idle timer below instead
                        // of sleeping here.
                        Ok(permit) => {
                            go_idle =
                                !claim_and_dispatch(&state, &registry, &queues, permit).await;
                        }
                        Err(_closed) => draining = true,
                    }
                }
            }
            if go_idle {
                idle.as_mut().reset(Instant::now() + idle_poll_interval);
                waiting = true;
            }
        }
        // Every permit back means every dispatched job reached its terminal
        // state; a rejection here (closed or a request larger than the pool)
        // likewise proves nothing is in flight.
        let slots = u32::try_from(concurrency).unwrap_or(u32::MAX);
        if let Ok(_all) = semaphore.acquire_many_owned(slots).await {
            debug!("all in-flight jobs finished");
        }
    }
}

/// Claims one job with `permit` and hands it to [`run_job`]. Returns
/// whether the claim found work; when it did not, the permit is back and
/// the caller waits [`Worker::idle_poll_interval`] before the next round —
/// an interruptible wait in [`Worker::run`], not a sleep here.
async fn claim_and_dispatch(
    state: &AppState,
    registry: &Arc<JobRegistry>,
    queues: &[String],
    permit: OwnedSemaphorePermit,
) -> bool {
    let names: Vec<&str> = queues.iter().map(String::as_str).collect();
    match claim_next(&state.db, &names).await {
        Ok(Some(job)) => {
            let state = state.clone();
            let registry = Arc::clone(registry);
            let _dispatched = tokio::spawn(run_job(state, registry, job, permit));
            true
        }
        Ok(None) => {
            drop(permit);
            false
        }
        Err(err) => {
            warn!(%err, "job claim failed");
            drop(permit);
            false
        }
    }
}

/// Runs one claimed job end to end and writes its terminal state, releasing
/// `permit` only after the database has the outcome.
async fn run_job(
    state: AppState,
    registry: Arc<JobRegistry>,
    job: Job,
    permit: OwnedSemaphorePermit,
) {
    debug!(job_id = %job.id, kind = %job.kind, attempts = job.attempts, "running job");
    let outcome = dispatch_handler(&registry, &state, &job, JOB_TIMEOUT).await;
    record_outcome(&state.db, job.id, job.attempts, outcome).await;
    drop(permit);
}

/// Invokes the handler registered for `job`'s kind on a task of its own, so
/// a panicking handler surfaces as a retryable [`JobError`] (a `JoinError`
/// becomes a [`JobError::Retry`]) instead of tearing down the worker. An
/// unregistered kind is discarded: nothing can run it, and retrying cannot
/// change that.
///
/// A handler still running after `timeout` is aborted and the job retried,
/// so it releases its worker permit and cannot overlap a rescued re-run.
async fn dispatch_handler(
    registry: &JobRegistry,
    state: &AppState,
    job: &Job,
    timeout: Duration,
) -> Result<(), JobError> {
    let Some(handler) = registry.get(&job.kind) else {
        return Err(JobError::Discard(format!(
            "no handler registered for job kind '{}'",
            job.kind
        )));
    };
    let attempt = AttemptInfo {
        attempts: job.attempts,
        max_attempts: job.max_attempts,
    };
    let handle = tokio::spawn(handler(state.clone(), attempt, job.args.clone()));
    let abort = handle.abort_handle();
    match tokio::time::timeout(timeout, handle).await {
        Err(_elapsed) => {
            abort.abort();
            Err(JobError::Retry(format!(
                "handler for job kind '{}' exceeded {}s",
                job.kind,
                timeout.as_secs()
            )))
        }
        Ok(Ok(outcome)) => outcome,
        Ok(Err(join_err)) => Err(JobError::Retry(format!(
            "handler for job kind '{}' panicked or was cancelled: {join_err}",
            job.kind
        ))),
    }
}

/// Moves a finished attempt to its terminal state: `completed` on success,
/// retryable with [`backoff`] delay (or the handler's own) on a retryable
/// failure, discarded on a fatal failure. Whether the attempt budget allows
/// the retry is decided by [`fail`] (the models layer), atomically with the
/// write.
///
/// `attempts` is the claim that ran the attempt and fences the write: when
/// [`rescue_stuck`] gave the job to another claim meanwhile, the write
/// matches no row and is dropped with a warning, not an error.
async fn record_outcome(db: &PgPool, id: Uuid, attempts: i32, outcome: Result<(), JobError>) {
    let stored = match outcome {
        Ok(()) => complete(db, id, attempts).await,
        Err(JobError::Retry(detail)) => {
            fail(db, id, attempts, &detail, Some(backoff(attempts))).await
        }
        Err(JobError::RetryAfter(delay, detail)) => {
            fail(db, id, attempts, &detail, Some(delay)).await
        }
        Err(JobError::Discard(detail)) => fail(db, id, attempts, &detail, None).await,
    };
    match stored {
        Ok(true) => {}
        Ok(false) => warn!(%id, attempts, "job outcome dropped: the claim was rescued"),
        Err(err) => error!(%err, %id, "could not record job outcome"),
    }
}

/// Settles expired locks ([`rescue_stuck`] past [`STUCK_LOCK_AGE`]): jobs
/// with attempts left return to the queue, jobs at their budget are
/// discarded. Logs how many rows the sweep touched.
async fn sweep_stuck(state: &AppState) {
    match rescue_stuck(&state.db, STUCK_LOCK_AGE).await {
        Ok(count) => {
            if count > 0 {
                info!(count, "settled stuck jobs (retried or discarded)");
            }
        }
        Err(err) => error!(%err, "stuck job rescue failed"),
    }
}

#[cfg(test)]
mod dispatch_tests;
#[cfg(test)]
mod lifecycle_tests;
