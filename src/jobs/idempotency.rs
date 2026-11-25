//! The idempotency-key maintenance jobs: the periodic sweep of expired
//! `Idempotency-Key` mappings.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres rows,
//! and the replay window the create path reads through
//! [`crate::models::idempotency_key`] is only a *window* because this sweep
//! prunes it: [`handle_cleanup`] (job kind [`KIND_CLEANUP`]) deletes every
//! mapping older than [`MAX_AGE`] — the 24 hours of Pleroma's
//! `:idempotency_ttl` behind its `IdempotencyPlug`
//! (`lib/pleroma/web/plugs/idempotency_plug.ex`) — one pass per period, on
//! the schedule [`periodic_jobs`](super::scheduler::periodic_jobs) declares.
//!
//! Pleroma needs no such job: its plug's Cachex entries expire on their own.
//! A durable table has no clock of their own, so the horizon is enforced
//! here, and the create path needs nothing of it either way — a pruned key
//! simply claims again like a first-time one, and a sweep that missed a
//! period only keeps a dead mapping replayable a while longer.

use chrono::TimeDelta;
use serde_json::Value;

use super::maintenance::run_sweep;
use crate::{
    jobs::{AttemptInfo, HandlerFuture},
    models::idempotency_key,
    state::AppState,
};

/// The job kind of the expired-key sweep, run from the
/// [`periodic_jobs`](super::scheduler::periodic_jobs) registry through the
/// [`JobRegistry`](super::JobRegistry) handler [`handle_cleanup`], on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND).
pub const KIND_CLEANUP: &str = "idempotency.cleanup";

/// How long a claimed key replays: 24 hours from the create that stored it
/// (Pleroma's plug TTL, in its `:idempotency_ttl` config), after which the
/// next sweep deletes the mapping and the key is free again.
pub const MAX_AGE: TimeDelta = TimeDelta::hours(24);

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_CLEANUP`]: one
/// sweep of the mappings older than [`MAX_AGE`], i.e.
/// [`idempotency_key::delete_older_than`].
///
/// A failed sweep is a [`JobError::Retry`](crate::jobs::JobError::Retry), and the scheduler enqueues the
/// next period regardless, so a missed pass costs a dead key its stay in the
/// table, never a create: the create path treats an unclaimed key exactly as
/// it always has.
pub fn handle_cleanup(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        run_sweep(
            "expired idempotency keys deleted",
            idempotency_key::delete_older_than(&state.db, MAX_AGE),
        )
        .await
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::{
        jobs::{JobError, QUEUE_BACKGROUND, scheduler::periodic_jobs},
        models::{idempotency_key::live_status, status::Status, user::User},
        test_fixtures::local_user as user,
        test_support::{app_state, broken_state},
    };

    /// The period the sweep is scheduled on: ten minutes.
    const EVERY: Duration = Duration::from_secs(600);

    /// The keys still mapping one account's creates, as `(key, status)`
    /// pairs ordered by key.
    async fn mappings(pool: &PgPool, alice: &User) -> Vec<(String, Status)> {
        let mut found = Vec::new();
        for key in ["old", "edge", "fresh"] {
            if let Some(status) = live_status(pool, alice.id, key)
                .await
                .expect("the mapping reads")
            {
                found.push((key.to_owned(), status));
            }
        }
        found
    }

    /// The handler's attempt info: the first of however many the sweep gets,
    /// since its work does not depend on which pass it is.
    const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };

    #[tokio::test]
    async fn the_handler_deletes_the_expired_mappings() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let day_and_hour = 24 * 3_600 + 3_600;
        let (old, edge, fresh) = (
            idempotency_key::insert_aged(&pool, &alice, "old", day_and_hour).await,
            idempotency_key::insert_aged(&pool, &alice, "edge", 24 * 3_600 - 60).await,
            idempotency_key::insert_aged(&pool, &alice, "fresh", 60).await,
        );

        let outcome = handle_cleanup(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "a sweep of an aged row succeeds");
        let left: Vec<(String, Uuid)> = mappings(&pool, &alice)
            .await
            .into_iter()
            .map(|(key, status)| (key, status.id))
            .collect();
        assert_eq!(
            left,
            vec![("edge".to_owned(), edge), ("fresh".to_owned(), fresh)],
            "only the mapping past its 24 hours is gone, the two live keys \
             keep their own statuses ({old} was the aged one's)"
        );
    }

    #[tokio::test]
    async fn the_handler_passes_over_a_table_it_need_not_touch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let outcome = handle_cleanup(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "nothing to delete is a clean pass");
    }

    /// A sweep that cannot reach the database is a retryable failure, so the
    /// queue keeps the pass while the scheduler enqueues the next period: a
    /// dead mapping survives a broken pass, and no create's answer is lost.
    #[tokio::test]
    async fn an_unreachable_database_retries_the_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let aged = idempotency_key::insert_aged(&pool, &alice, "old", 24 * 3_600 + 60).await;
        let state = broken_state().await;

        let outcome = handle_cleanup(state, FIRST_ATTEMPT, json!({})).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.starts_with("expired idempotency keys deleted failed"),
                "the retry states what failed: {detail}"
            ),
            Ok(()) => panic!("a closed pool must not read as a successful sweep"),
            Err(other) => panic!("a closed pool is retryable, not {other:?}"),
        }
        assert!(
            live_status(&pool, alice.id, "old")
                .await
                .expect("the mapping reads")
                .is_some_and(|status| status.id == aged),
            "the healthy pool sees no work the broken one claimed to do"
        );
    }

    /// The sweep is wired into both registries the ticket names: the worker
    /// can run its kind, and the scheduler places it on the maintenance
    /// queue every ten minutes.
    #[test]
    fn the_kind_is_registered_and_scheduled() {
        let registry = crate::job_registry(&crate::config::Config::for_tests());
        assert!(
            registry.get(KIND_CLEANUP).is_some(),
            "the worker knows `{KIND_CLEANUP}`"
        );

        let job = periodic_jobs()
            .into_iter()
            .find(|job| job.kind == KIND_CLEANUP)
            .expect("the sweep is on the periodic schedule");
        assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
        assert_eq!(job.every, EVERY, "once every ten minutes");
    }

    /// The handler the registry binds to the kind is this module's
    /// [`handle_cleanup`], not some other sweep: calling the registered
    /// handler deletes the aged rows.
    #[tokio::test]
    async fn the_registered_handler_is_this_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let aged = idempotency_key::insert_aged(&pool, &alice, "old", 25 * 3_600).await;
        assert!(
            live_status(&pool, alice.id, "old")
                .await
                .expect("the mapping reads")
                .is_some_and(|status| status.id == aged),
            "the aged mapping stands before the sweep"
        );
        let registry = crate::job_registry(&crate::config::Config::for_tests());
        let handler = registry
            .get(KIND_CLEANUP)
            .expect("the sweep's kind is registered");

        let outcome = handler(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "the registered handler runs");
        assert!(
            mappings(&pool, &alice).await.is_empty(),
            "and it is this module's sweep: the aged mapping is gone"
        );
    }
}
