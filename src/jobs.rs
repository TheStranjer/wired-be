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

use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::PgPool;
use tracing::{debug, warn};
use uuid::Uuid;

pub use self::{
    registry::{JobRegistry, backoff},
    worker::Worker,
};
use crate::{
    models::{
        job::{Job, NewJob, enqueue},
        user::{self, AccountStatus, User},
    },
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

/// Reads a claimed job's `args` payload as a `T`; arguments this shape
/// cannot deserialize are permanent — [`JobError::Discard`], never retried.
/// `job` names the job in the discard text
/// (`"<job> job arguments are unusable: <cause>"`).
pub(crate) fn parse_args<T: DeserializeOwned>(args: &Value, job: &str) -> Result<T, JobError> {
    serde_json::from_value(args.clone()).job_discard(&format!("{job} job arguments are unusable"))
}

/// Lifts a failed [`Result`] into a [`JobError`], naming what failed.
///
/// The one place a handler's failure gains its context: the job's treatment
/// ([`JobError::Retry`] or [`JobError::Discard`]) is the method picked, and
/// the text reads `"{what}: {err}"`.
pub trait JobErrorExt<T> {
    /// Maps an `Err` to [`JobError::Retry`] reading `"{what}: {err}"`; an
    /// `Ok` passes through.
    ///
    /// # Errors
    ///
    /// [`JobError::Retry`] when `self` is an `Err`.
    fn job_retry(self, what: &str) -> Result<T, JobError>;

    /// Maps an `Err` to [`JobError::Discard`] reading `"{what}: {err}"`; an
    /// `Ok` passes through.
    ///
    /// # Errors
    ///
    /// [`JobError::Discard`] when `self` is an `Err`.
    fn job_discard(self, what: &str) -> Result<T, JobError>;
}

impl<T, E: fmt::Display> JobErrorExt<T> for Result<T, E> {
    fn job_retry(self, what: &str) -> Result<T, JobError> {
        self.map_err(|source| JobError::Retry(format!("{what}: {source}")))
    }

    fn job_discard(self, what: &str) -> Result<T, JobError> {
        self.map_err(|source| JobError::Discard(format!("{what}: {source}")))
    }
}

/// Lifts a transient failure into the queue's retry arm, naming the `job`
/// (or the step of it) that hit it: `"<job> failed: <err>"`.
pub(crate) fn retry(job: &str, err: &dyn fmt::Display) -> JobError {
    JobError::Retry(format!("{job} failed: {err}"))
}

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

/// The user a job acts as, read once for every job module that needs one.
///
/// `job` names the job in the failure text (`"import"`, `"backup"`, ...). A
/// vanished row is permanent: [`JobError::Discard`]. A database that cannot
/// answer is transient: [`JobError::Retry`]. With `require_active`, an
/// account that can no longer act — deactivated, deleted, or awaiting
/// approval, the states the API refuses at token resolution — is a
/// [`JobError::Discard`] too; jobs that must still run for such an account
/// (the sweep that deletes it, the archive it asked for) pass `false`.
///
/// # Errors
///
/// [`JobError::Discard`] when the user row is gone, or when `require_active`
/// is set and the account can no longer act; [`JobError::Retry`] on a
/// database failure.
pub(crate) async fn load_user(
    state: &AppState,
    id: Uuid,
    job: &str,
    require_active: bool,
) -> Result<User, JobError> {
    match user::get_by_id(&state.db, id).await {
        Ok(Some(found)) => {
            let barred = require_active
                && (found.deleted_at.is_some()
                    || matches!(
                        found.account_status(),
                        AccountStatus::Deactivated | AccountStatus::ApprovalPending
                    ));
            if barred {
                return Err(JobError::Discard(format!(
                    "the {job} job's user {id} can no longer act"
                )));
            }
            Ok(found)
        }
        Ok(None) => Err(JobError::Discard(format!(
            "the {job} job's user {id} no longer exists"
        ))),
        Err(err) => Err(JobError::Retry(format!("{job} user read failed: {err}"))),
    }
}

/// Enqueues `job` (normally carrying a `unique_key`) and logs the outcome,
/// the one shape every deduplicating job producer shares.
///
/// `what` names the thing queued in the log line: `"{what} queued"` at
/// `debug` for an inserted row, `"{what} already queued"` at `debug` when a
/// live job already holds the key, and `"could not queue {what}"` at `warn`
/// for a database failure. A failure is logged, not returned: every caller
/// has stored the state the job acts on and has a sweep or a retry behind it,
/// so refusing the client's request would be the worse answer. Returns the
/// inserted job, `None` when it was deduplicated or the enqueue failed.
pub(crate) async fn enqueue_keyed(pool: &PgPool, job: NewJob<'_>, what: &str) -> Option<Job> {
    let kind = job.kind;
    let key = job.unique_key;
    match enqueue(pool, job).await {
        Ok(Some(row)) => {
            debug!(kind, key, id = %row.id, "{what} queued");
            Some(row)
        }
        Ok(None) => {
            debug!(kind, key, "{what} already queued");
            None
        }
        Err(err) => {
            warn!(%err, kind, key, "could not queue {what}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        models::user::{Flag, NewUser, insert, local_ap_id, mark_deleted, set_activation},
    };

    /// Inserts a local user named `nickname` with the given flags.
    async fn user_with(state: &AppState, nickname: &str, approval_pending: bool) -> User {
        let new = NewUser {
            approval_pending: Flag::new(approval_pending),
            ..NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
        };
        insert(&state.db, &new)
            .await
            .expect("the user fixture inserts")
    }

    #[tokio::test]
    async fn a_missing_user_discards_with_or_without_the_active_gate() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), Config::for_tests());
        let id = Uuid::now_v7();

        for require_active in [false, true] {
            let outcome = load_user(&state, id, "probe", require_active).await;
            assert!(
                matches!(&outcome, Err(JobError::Discard(detail))
                    if detail.contains("probe") && detail.contains("no longer exists")),
                "a vanished user discards (require_active={require_active}): {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_deactivated_user_is_discarded_only_when_activity_is_required() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), Config::for_tests());
        let alice = user_with(&state, "alice", false).await;
        set_activation(&state.db, alice.id, false)
            .await
            .expect("the user deactivates");

        let gated = load_user(&state, alice.id, "probe", true).await;
        assert!(
            matches!(&gated, Err(JobError::Discard(detail)) if detail.contains("can no longer act")),
            "a deactivated user is discarded under the gate: {gated:?}"
        );
        let open = load_user(&state, alice.id, "probe", false)
            .await
            .expect("without the gate the deactivated user loads");
        assert_eq!(open.id, alice.id, "the same row comes back");
    }

    #[tokio::test]
    async fn an_approval_pending_user_is_discarded_only_when_activity_is_required() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), Config::for_tests());
        let bob = user_with(&state, "bob", true).await;

        let gated = load_user(&state, bob.id, "probe", true).await;
        assert!(
            matches!(&gated, Err(JobError::Discard(detail)) if detail.contains("can no longer act")),
            "an unapproved user is discarded under the gate: {gated:?}"
        );
        assert!(
            load_user(&state, bob.id, "probe", false).await.is_ok(),
            "without the gate the unapproved user loads"
        );
    }

    #[tokio::test]
    async fn a_deleted_user_is_discarded_only_when_activity_is_required() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), Config::for_tests());
        let carol = user_with(&state, "carol", false).await;
        mark_deleted(&state.db, carol.id)
            .await
            .expect("the user is marked deleted");

        let gated = load_user(&state, carol.id, "probe", true).await;
        assert!(
            matches!(&gated, Err(JobError::Discard(detail)) if detail.contains("can no longer act")),
            "a deleted user is discarded under the gate: {gated:?}"
        );
        assert!(
            load_user(&state, carol.id, "probe", false).await.is_ok(),
            "without the gate the deleted user loads"
        );
    }

    #[tokio::test]
    async fn an_active_user_loads_with_or_without_the_active_gate() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), Config::for_tests());
        let dave = user_with(&state, "dave", false).await;

        for require_active in [false, true] {
            let loaded = load_user(&state, dave.id, "probe", require_active)
                .await
                .expect("an active user loads");
            assert_eq!(
                loaded.id, dave.id,
                "the same row comes back (require_active={require_active})"
            );
        }
    }

    #[tokio::test]
    async fn a_closed_pool_retries_naming_the_job() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        pool.close().await;
        let state = AppState::for_tests(pool, Config::for_tests());

        let outcome = load_user(&state, Uuid::now_v7(), "probe", true).await;
        assert!(
            matches!(&outcome, Err(JobError::Retry(detail)) if detail.starts_with("probe user read failed")),
            "a database failure retries: {outcome:?}"
        );
    }

    /// A keyed job of kind `test.keyed` on the default queue.
    fn keyed(key: &str) -> NewJob<'_> {
        NewJob {
            queue: QUEUE_DEFAULT,
            kind: "test.keyed",
            args: json!({}),
            run_at: None,
            max_attempts: None,
            unique_key: Some(key),
        }
    }

    /// How many rows the `jobs` table holds.
    async fn job_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn enqueue_keyed_returns_the_inserted_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        let job = enqueue_keyed(&pool, keyed("k:1"), "test job").await;

        let job = job.expect("the first claim of a key inserts");
        assert_eq!(job.kind, "test.keyed", "the row carries the kind");
        assert_eq!(job.state, "available", "a new row waits for a worker");
        assert_eq!(job.unique_key.as_deref(), Some("k:1"), "the key is stored");
        assert_eq!(job_count(&pool).await, 1, "one row was inserted");
    }

    #[tokio::test]
    async fn enqueue_keyed_swallows_a_second_claim_of_a_live_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let first = enqueue_keyed(&pool, keyed("k:2"), "test job").await;
        assert!(first.is_some(), "the first claim inserts");

        let second = enqueue_keyed(&pool, keyed("k:2"), "test job").await;

        assert!(second.is_none(), "the key is held by an available job");
        assert_eq!(job_count(&pool).await, 1, "the duplicate inserted nothing");
    }

    #[tokio::test]
    async fn enqueue_keyed_lets_distinct_keys_both_in() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        let a = enqueue_keyed(&pool, keyed("k:a"), "test job").await;
        let b = enqueue_keyed(&pool, keyed("k:b"), "test job").await;

        assert!(a.is_some() && b.is_some(), "different keys do not collide");
        assert_eq!(job_count(&pool).await, 2, "both rows were inserted");
    }

    #[tokio::test]
    async fn enqueue_keyed_reports_a_database_failure_as_none() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        pool.close().await;

        let job = enqueue_keyed(&pool, keyed("k:3"), "test job").await;

        assert!(job.is_none(), "a failed enqueue is logged and yields None");
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

    /// A one-field arguments shape for the helper tests.
    #[derive(Debug, serde::Deserialize, PartialEq, Eq)]
    struct SampleArgs {
        /// The only field.
        id: u32,
    }

    #[test]
    fn parse_args_reads_a_valid_object() {
        let parsed = parse_args::<SampleArgs>(&serde_json::json!({ "id": 7 }), "sample")
            .expect("a well-formed object parses");
        assert_eq!(parsed, SampleArgs { id: 7 }, "the struct comes back whole");
    }

    #[test]
    fn parse_args_discards_a_missing_field_and_names_the_job() {
        let err = parse_args::<SampleArgs>(&serde_json::json!({}), "sample")
            .expect_err("a payload without the field is unusable");
        let JobError::Discard(detail) = &err else {
            panic!("unusable arguments must discard, not {err:?}");
        };
        assert!(
            detail.starts_with("sample job arguments are unusable: ") && detail.contains("id"),
            "the discard names the job and the missing field: {detail}"
        );
    }

    #[test]
    fn parse_args_discards_a_payload_of_the_wrong_type() {
        let err = parse_args::<SampleArgs>(&serde_json::json!("nope"), "sample")
            .expect_err("a string is not an arguments object");
        assert!(
            matches!(err, JobError::Discard(_)),
            "a wrong-typed payload is permanent too"
        );
    }

    #[test]
    fn retry_names_the_job_and_the_cause() {
        assert_eq!(
            retry("sample pass", &"disk full"),
            JobError::Retry("sample pass failed: disk full".to_owned()),
            "the retry text is `<job> failed: <err>`"
        );
    }

    #[test]
    fn job_error_ext_passes_ok_and_maps_err_to_the_chosen_arm() {
        let fine_retry: Result<u8, &str> = Ok(3);
        assert_eq!(fine_retry.job_retry("the read"), Ok(3));
        let fine_discard: Result<u8, &str> = Ok(3);
        assert_eq!(fine_discard.job_discard("the read"), Ok(3));

        let failed_retry: Result<u8, &str> = Err("boom");
        assert_eq!(
            failed_retry.job_retry("the read failed"),
            Err(JobError::Retry("the read failed: boom".to_owned())),
            "a retry reads `<what>: <err>`"
        );
        let failed_discard: Result<u8, &str> = Err("boom");
        assert_eq!(
            failed_discard.job_discard("the args are unusable"),
            Err(JobError::Discard("the args are unusable: boom".to_owned())),
            "a discard reads `<what>: <err>`"
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
