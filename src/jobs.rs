//! The job worker runtime: queue names, the handler registry, retry
//! [`backoff`] and the [`Worker`] claim-dispatch loop for the queue core in
//! [`crate::models::job`].
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres rows;
//! this module is the consumer side of those rows. A job kind is mapped to an
//! async handler by [`JobRegistry`]; [`Worker`] polls the configured queues,
//! runs at most `concurrency` handlers at a time (bounded by a semaphore), and
//! moves every attempt to its terminal state: `complete` on success,
//! retryable with [`backoff`] delay on [`JobError::Retry`] — or with the
//! handler's own delay on [`JobError::RetryAfter`], which is how a peer's
//! `Retry-After` is honoured — including handler panics, which run on their
//! own task — and `discarded` on [`JobError::Discard`] or an unknown kind.
//! Handlers are told which attempt they are running
//! ([`AttemptInfo`]), because some failures only deserve a second chance once
//! (see [`crate::federation::delivery`]). When the shutdown future resolves
//! the worker stops claiming and waits for its in-flight jobs. Concrete
//! handlers are registered by [`crate::serve`], which starts with the
//! federation delivery and mailer job kinds. The sibling [`scheduler`] module
//! is the producer side: it enqueues the recurring, cron-like jobs.

pub mod deletion;
pub mod idempotency;
pub mod imports;
pub mod mutes;
pub mod scheduler;
pub mod statuses;

use std::{
    collections::HashMap, error::Error, fmt, future::Future, pin::Pin, sync::Arc, time::Duration,
};

use serde_json::Value;
use sqlx::PgPool;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Instant, MissedTickBehavior, interval_at, sleep},
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{
    models::job::{Job, claim_next, complete, fail, rescue_stuck},
    state::AppState,
};

/// Queue for general-purpose jobs ([`crate::models::job`] callers that have
/// no queue of their own).
pub const QUEUE_DEFAULT: &str = "default";
/// Queue for inbound federation work (processing incoming activities).
pub const QUEUE_FEDERATOR_INCOMING: &str = "federator_incoming";
/// Queue for outbound federation work (signature, delivery and retries).
pub const QUEUE_FEDERATOR_OUTGOING: &str = "federator_outgoing";
/// Queue for sending email.
pub const QUEUE_MAILER: &str = "mailer";
/// Queue for cron-like recurring work.
pub const QUEUE_SCHEDULED: &str = "scheduled";
/// Queue for slow background maintenance (pruning, housekeeping).
pub const QUEUE_BACKGROUND: &str = "background";
/// Default `JOB_QUEUES` value: every queue above, comma-separated, claim
/// priority in list order.
pub const DEFAULT_JOB_QUEUES: &str =
    "default,federator_incoming,federator_outgoing,mailer,scheduled,background";

/// Default pause between claim rounds when no job is due (keeps an idle
/// worker off the database without a busy loop). [`Worker`] stores this as
/// its `idle_poll_interval`; [`Worker::with_idle_poll_interval`] overrides
/// it.
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How often the running worker sweeps jobs stuck in `executing`.
const RESCUE_INTERVAL: Duration = Duration::from_secs(60);
/// An `executing` job locked for longer than this is considered stuck.
const STUCK_LOCK_AGE: Duration = Duration::from_mins(10);
/// Retry delay after the first failed attempt: `backoff(1)`.
const BACKOFF_BASE_SECS: u64 = 15;
/// Ceiling for [`backoff`]: six hours.
const BACKOFF_CAP_SECS: u64 = 6 * 60 * 60;

/// How a handler failure should move the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobError {
    /// Transient failure: retry the job with [`backoff`] delay (until the
    /// attempt budget is spent).
    Retry(String),
    /// Transient failure whose delay the peer set: retry the job after
    /// exactly the given [`Duration`] instead of [`backoff`], until the
    /// attempt budget is spent. The string states what failed, as in
    /// [`Self::Retry`]; a peer's `Retry-After` on an inbox delivery is the
    /// case this exists for.
    RetryAfter(Duration, String),
    /// Permanent failure: discard the job, never retry.
    Discard(String),
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retry(detail) => write!(f, "retryable job failure: {detail}"),
            Self::RetryAfter(delay, detail) => write!(
                f,
                "retryable job failure: {detail} (after {}s)",
                delay.as_secs()
            ),
            Self::Discard(detail) => write!(f, "fatal job failure: {detail}"),
        }
    }
}

impl Error for JobError {}

/// What a handler is told about the claim it is running, beside the job's
/// arguments.
///
/// A failure whose right treatment depends on how many times it has already
/// happened — a delivery refused `401`, retried once for a key refresh and
/// discarded after that — reads [`attempts`](Self::attempts) here; the
/// worker's own retry budget still decides when the job runs out of attempts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptInfo {
    /// Claims made so far, this one included: `1` on the first attempt.
    pub attempts: i32,
    /// The attempt budget of the job: [`attempts`](Self::attempts) at or above
    /// it means this is the last chance.
    pub max_attempts: i32,
}

/// The boxed future an async job handler returns.
pub type HandlerFuture = Pin<Box<dyn Future<Output = Result<(), JobError>> + Send>>;

/// An async job handler: takes the shared application state, the
/// [`AttemptInfo`] of the claim it is running, and the job's JSON arguments,
/// and reports its outcome as a [`HandlerFuture`].
///
/// Implementations are plain `fn`s (not capturing closures) so they can live
/// in a static registry; whatever they need is already in
/// [`AppState`] or the job's `args`.
///
/// ```
/// use serde_json::Value;
/// use wired_be::jobs::{AttemptInfo, HandlerFuture};
/// use wired_be::state::AppState;
///
/// /// Handler shape: do the work, then report success or a failure.
/// fn hello(_state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
///     Box::pin(async { Ok(()) })
/// }
/// ```
pub type HandlerFn = fn(AppState, AttemptInfo, Value) -> HandlerFuture;

/// Maps job kinds to their handlers.
#[derive(Debug, Default)]
pub struct JobRegistry {
    /// Kind to handler table.
    handlers: HashMap<&'static str, HandlerFn>,
}

impl JobRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds `kind` to `handler`, replacing any previous binding.
    pub fn register(&mut self, kind: &'static str, handler: HandlerFn) {
        let _previous = self.handlers.insert(kind, handler);
    }

    /// The handler bound to `kind`, `None` when the kind is unknown.
    #[must_use]
    pub fn get(&self, kind: &str) -> Option<HandlerFn> {
        self.handlers.get(kind).copied()
    }
}

/// The exponential retry delay for the `attempt`-th claim of a job:
/// 15 s doubled per attempt (`15 s * 2^(attempt - 1)`), capped at six hours.
///
/// Deliberately deterministic — no jitter — so tests can assert exact
/// values. Attempts below 1 (which cannot occur after a claim) get the base
/// delay; large attempts saturate at the cap without overflowing.
#[must_use]
pub fn backoff(attempt: i32) -> Duration {
    let exponent = u32::try_from(attempt.saturating_sub(1)).unwrap_or(0);
    let factor = 2_u64.checked_pow(exponent).unwrap_or(u64::MAX);
    let secs = BACKOFF_BASE_SECS
        .saturating_mul(factor)
        .min(BACKOFF_CAP_SECS);
    Duration::from_secs(secs)
}

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
    let outcome = dispatch_handler(&registry, &state, &job).await;
    record_outcome(&state.db, job.id, job.attempts, outcome).await;
    drop(permit);
}

/// Invokes the handler registered for `job`'s kind on a task of its own, so
/// a panicking handler surfaces as a retryable [`JobError`] (a `JoinError`
/// becomes a [`JobError::Retry`]) instead of tearing down the worker. An
/// unregistered kind is discarded: nothing can run it, and retrying cannot
/// change that.
async fn dispatch_handler(
    registry: &JobRegistry,
    state: &AppState,
    job: &Job,
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
    match handle.await {
        Ok(outcome) => outcome,
        Err(join_err) => Err(JobError::Retry(format!(
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
async fn record_outcome(db: &PgPool, id: Uuid, attempts: i32, outcome: Result<(), JobError>) {
    let stored = match outcome {
        Ok(()) => complete(db, id).await,
        Err(JobError::Retry(detail)) => fail(db, id, &detail, Some(backoff(attempts))).await,
        Err(JobError::RetryAfter(delay, detail)) => fail(db, id, &detail, Some(delay)).await,
        Err(JobError::Discard(detail)) => fail(db, id, &detail, None).await,
    };
    if let Err(err) = stored {
        error!(%err, %id, "could not record job outcome");
    }
}

/// Returns expired locks to the queue ([`rescue_stuck`] past
/// [`STUCK_LOCK_AGE`]), logging how many rows the sweep rescued.
async fn sweep_stuck(state: &AppState) {
    match rescue_stuck(&state.db, STUCK_LOCK_AGE).await {
        Ok(count) => {
            if count > 0 {
                info!(count, "rescued stuck jobs");
            }
        }
        Err(err) => error!(%err, "stuck job rescue failed"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use chrono::{TimeDelta, Utc};
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use tokio::sync::oneshot;

    use super::*;
    use crate::{
        config::Config,
        http_client::HttpClient,
        models::job::{NewJob, enqueue},
        uploads::UploaderBackend,
    };

    /// Set once by [`marking_handler`] when its body runs.
    static HANDLER_RAN: AtomicBool = AtomicBool::new(false);
    /// Set once by [`slow_handler`] before it pauses.
    static HANDLER_STARTED: AtomicBool = AtomicBool::new(false);
    /// What [`reporting_handler`] was told about its attempt.
    static LAST_ATTEMPT: Mutex<Option<AttemptInfo>> = Mutex::new(None);

    /// Test state over `pool` with the fixed test config.
    fn app_state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// A lazy pool for handler tests that never touch the database.
    fn lazy_pool() -> PgPool {
        PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap()
    }

    /// Idle-poll interval for worker tests: short enough that a claim round
    /// after an empty one costs milliseconds, so tests never sit through the
    /// production second.
    const TEST_IDLE_POLL: Duration = Duration::from_millis(10);

    /// A worker over the `test` queue with `concurrency` slots and a fast
    /// idle poll.
    fn worker_for(pool: &PgPool, registry: JobRegistry, concurrency: usize) -> Worker {
        Worker::new(
            app_state(pool),
            Arc::new(registry),
            vec!["test".to_owned()],
            concurrency,
        )
        .with_idle_poll_interval(TEST_IDLE_POLL)
    }

    /// A job payload on the `test` queue with `kind`.
    fn test_job(kind: &'static str) -> NewJob<'static> {
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
    fn job_row(kind: &str) -> Job {
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
    async fn fetch(pool: &PgPool, id: Uuid) -> Job {
        sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Resolves once the stored state of job `id` is `want`, failing the
    /// test if that never happens within 10 seconds.
    async fn wait_for_state(pool: &PgPool, id: Uuid, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while fetch(pool, id).await.state != want {
            assert!(Instant::now() < deadline, "job never reached state {want}");
            sleep(Duration::from_millis(20)).await;
        }
    }

    /// Runs `worker` until job `id` reports state `want` (the wait doubles
    /// as the shutdown signal), then returns the stored row.
    async fn run_until_state(worker: Worker, pool: &PgPool, id: Uuid, want: &str) -> Job {
        worker.run(wait_for_state(pool, id, want)).await;
        fetch(pool, id).await
    }

    /// A handler that always succeeds.
    fn ok_handler(_state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
        Box::pin(async { Ok(()) })
    }

    /// A handler that records that it ran, then succeeds.
    fn marking_handler(_state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
        Box::pin(async {
            HANDLER_RAN.store(true, Ordering::SeqCst);
            Ok(())
        })
    }

    /// A handler that reports its [`AttemptInfo`] and succeeds, so a test can
    /// read what the dispatch handed it.
    fn reporting_handler(_state: AppState, attempt: AttemptInfo, _args: Value) -> HandlerFuture {
        Box::pin(async move {
            let _previous = LAST_ATTEMPT.lock().unwrap().replace(attempt);
            Ok(())
        })
    }

    /// A handler that reports a transient failure.
    fn retry_handler(_state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
        Box::pin(async { Err(JobError::Retry("temporarily broken".to_owned())) })
    }

    /// A handler that reports a transient failure with the peer's own delay:
    /// half a minute, long enough that the retry cannot fire while a test is
    /// still watching the job's first terminal write.
    fn retry_after_handler(_state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
        Box::pin(async {
            Err(JobError::RetryAfter(
                Duration::from_secs(30),
                "peer asked to wait".to_owned(),
            ))
        })
    }

    /// A handler that reports a permanent failure.
    fn discard_handler(_state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
        Box::pin(async { Err(JobError::Discard("bad arguments".to_owned())) })
    }

    /// A handler that crashes its task mid-flight.
    fn panicking_handler(_state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
        Box::pin(async {
            let _boom = "not-a-number".parse::<u64>().unwrap();
            Ok(())
        })
    }

    /// A handler that announces its start and then runs for a moment.
    fn slow_handler(_state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
        Box::pin(async {
            HANDLER_STARTED.store(true, Ordering::SeqCst);
            sleep(Duration::from_millis(400)).await;
            Ok(())
        })
    }

    #[test]
    fn backoff_doubles_per_attempt() {
        assert_eq!(backoff(1), Duration::from_secs(15), "first retry");
        assert_eq!(backoff(2), Duration::from_secs(30), "second retry");
        assert_eq!(backoff(3), Duration::from_secs(60), "third retry");
        assert_eq!(backoff(4), Duration::from_secs(120), "fourth retry");
        assert_eq!(
            backoff(11),
            Duration::from_mins(256),
            "last step under the cap"
        );
    }

    #[test]
    fn backoff_caps_at_six_hours() {
        assert_eq!(backoff(12), Duration::from_hours(6), "first capped attempt");
        assert_eq!(backoff(50), Duration::from_hours(6), "far beyond the cap");
        assert_eq!(
            backoff(i32::MAX),
            Duration::from_hours(6),
            "huge exponents saturate at the cap instead of overflowing"
        );
    }

    #[test]
    fn backoff_below_the_first_attempt_is_the_base_delay() {
        assert_eq!(backoff(0), Duration::from_secs(15), "attempt zero");
        assert_eq!(
            backoff(i32::MIN),
            Duration::from_secs(15),
            "negative attempts"
        );
    }

    #[test]
    fn registry_maps_kinds_to_handlers() {
        let mut registry = JobRegistry::new();
        assert!(
            registry.get("greet").is_none(),
            "unknown kinds resolve to nothing"
        );
        registry.register("greet", ok_handler);
        assert!(registry.get("greet").is_some(), "registered kinds resolve");
        registry.register("greet", retry_handler);
        assert!(
            registry.get("greet").is_some(),
            "re-registering a kind keeps it bound"
        );
    }

    #[test]
    fn default_queues_list_names_every_constant() {
        let constants = [
            QUEUE_DEFAULT,
            QUEUE_FEDERATOR_INCOMING,
            QUEUE_FEDERATOR_OUTGOING,
            QUEUE_MAILER,
            QUEUE_SCHEDULED,
            QUEUE_BACKGROUND,
        ];
        assert_eq!(
            DEFAULT_JOB_QUEUES,
            constants.join(","),
            "the documented default is exactly the six queue constants"
        );
    }

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

        let done =
            run_until_state(worker_for(&pool, registry, 2), &pool, job.id, "completed").await;
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

        let done =
            run_until_state(worker_for(&pool, registry, 1), &pool, job.id, "retryable").await;
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

    #[test]
    fn job_error_display_states_the_outcome_and_the_wait() {
        assert_eq!(
            JobError::Retry("later".to_owned()).to_string(),
            "retryable job failure: later",
            "a plain retry states its detail"
        );
        assert_eq!(
            JobError::RetryAfter(Duration::from_mins(4), "peer throttled us".to_owned())
                .to_string(),
            "retryable job failure: peer throttled us (after 240s)",
            "a delayed retry states its detail and the delay"
        );
        assert_eq!(
            JobError::Discard("never".to_owned()).to_string(),
            "fatal job failure: never",
            "a discard says the failure is fatal"
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

        let done =
            run_until_state(worker_for(&pool, registry, 1), &pool, job.id, "retryable").await;
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

        let done =
            run_until_state(worker_for(&pool, registry, 1), &pool, job.id, "discarded").await;
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

        let done =
            run_until_state(worker_for(&pool, registry, 1), &pool, job.id, "discarded").await;
        let message = done.last_error.unwrap_or_default();
        assert!(
            message.contains("mystery"),
            "the error names the unknown kind: {message}"
        );
    }

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
}
