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

pub mod backups;
pub mod deletion;
pub mod idempotency;
pub mod imports;
pub mod instances;
pub mod maintenance;
pub mod mutes;
pub mod polls;
mod registry;
pub mod rich_media;
pub mod scheduled_statuses;
pub mod scheduler;
pub mod statuses;
#[cfg(test)]
mod test_support;
pub mod web_push;
mod worker;

use std::{error::Error, fmt, future::Future, pin::Pin, time::Duration};

use serde_json::Value;

pub use self::{
    registry::{JobRegistry, backoff},
    worker::Worker,
};
use crate::state::AppState;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
