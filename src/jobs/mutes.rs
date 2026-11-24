//! The mutes maintenance jobs: the periodic sweep of expired mute rows.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres rows,
//! and [`crate::models::mute`] treats a mute at or past its
//! [`expires_at`](crate::models::mute::Mute::expires_at) as absent from every
//! read, so a timed mute stops working the moment its duration runs out without
//! any job having run. What that leaves behind is a row no read answers and no
//! filter sees: [`handle_expire`] (job kind [`KIND_EXPIRE`]) deletes those rows,
//! one pass per period, on the schedule
//! [`periodic_jobs`](super::scheduler::periodic_jobs) declares.
//!
//! The reference behaviour is Pleroma's per-mute `MuteExpireWorker`
//! (`lib/pleroma/workers/mute_expire_worker.ex`), which `User.mute` schedules
//! for each mute with a duration; this server has one recurring sweep over the
//! whole table instead, which needs no per-write job row and converges on the
//! same state (see [`crate::models::mute`]'s notes on the lapsed rows the reads
//! already ignore).

use serde_json::Value;

use super::maintenance::run_sweep;
use crate::{
    jobs::{AttemptInfo, HandlerFuture},
    models::mute,
    state::AppState,
};

/// The job kind of the expired-mute sweep, run from the
/// [`periodic_jobs`](super::scheduler::periodic_jobs) registry through the
/// [`JobRegistry`](super::JobRegistry) handler [`handle_expire`], on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND).
pub const KIND_EXPIRE: &str = "mutes.expire";

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_EXPIRE`]: one
/// sweep of the mutes whose duration has run out, i.e.
/// [`mute::delete_expired`].
///
/// A failed sweep is a [`JobError::Retry`](crate::jobs::JobError::Retry), and the scheduler enqueues the next
/// period regardless, so a missed pass costs the lapsed rows their stay in the
/// table, never a mute's effect: the reads already treat an expired mute as
/// absent.
pub fn handle_expire(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(
        async move { run_sweep("expired mutes deleted", mute::delete_expired(&state.db)).await },
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sqlx::{PgPool, postgres::PgPoolOptions};
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        jobs::{JobError, QUEUE_BACKGROUND, scheduler::periodic_jobs},
        models::user::{NewUser, User, insert, local_ap_id},
    };

    /// The period the sweep is scheduled on: ten minutes.
    const EVERY: Duration = Duration::from_secs(600);

    /// Test state over `pool` with the fixed test config.
    fn app_state(pool: &PgPool) -> AppState {
        AppState::for_tests(pool.clone(), Config::for_tests())
    }

    /// Test state over a pool that cannot answer: closed before it ever
    /// connected, which is how a handler sees a database that went away.
    async fn broken_state() -> AppState {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        pool.close().await;
        app_state(&pool)
    }

    /// Inserts a local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// Writes `target` into `muter`'s mute list with `expires_at` set to
    /// `seconds` from now (negative for the past), so a test can put the table
    /// into the state the sweep is about without waiting out a duration.
    async fn mute_expiring_in(pool: &PgPool, muter: &User, target: &User, seconds: i64) {
        let sql = "INSERT INTO mutes (id, muter_id, muted_id, expires_at)
                    VALUES ($1, $2, $3, now() + make_interval(secs => $4))";
        let written = sqlx::query(sql)
            .bind(Uuid::now_v7())
            .bind(muter.id)
            .bind(target.id)
            .bind(seconds)
            .execute(pool)
            .await
            .expect("the fixture mute inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
    }

    /// The `muted_id`s of the table's rows that carry an expiry, ordered: what
    /// a test reads back to see which rows survived the sweep.
    async fn timed_mutes(pool: &PgPool) -> Vec<Uuid> {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT muted_id FROM mutes WHERE expires_at IS NOT NULL ORDER BY muted_id",
        )
        .fetch_all(pool)
        .await
        .expect("the rows read")
    }

    /// The handler's attempt info: the first of however many the sweep gets,
    /// since its work does not depend on which pass it is.
    const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };

    #[tokio::test]
    async fn the_handler_deletes_the_expired_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let dave = user(&pool, "dave").await;
        mute_expiring_in(&pool, &alice, &bob, -60).await;
        mute_expiring_in(&pool, &alice, &carol, -1).await;
        mute_expiring_in(&pool, &alice, &dave, 3_600).await;

        let outcome = handle_expire(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "a sweep of a lapsed row succeeds");
        assert_eq!(
            timed_mutes(&pool).await,
            vec![dave.id],
            "only the mute whose duration has not run out is left"
        );
    }

    #[tokio::test]
    async fn the_handler_passes_over_a_table_it_need_not_touch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let outcome = handle_expire(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "nothing to delete is a clean pass");
        assert!(timed_mutes(&pool).await.is_empty(), "and it wrote nothing");
    }

    /// A sweep that cannot reach the database is a retryable failure, so the
    /// queue keeps the pass while the scheduler enqueues the next period: a
    /// lapsed row survives a broken pass, and no mute's effect is lost.
    #[tokio::test]
    async fn an_unreachable_database_retries_the_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = broken_state().await;

        let outcome = handle_expire(state, FIRST_ATTEMPT, json!({})).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.starts_with("expired mutes deleted failed"),
                "the retry states what failed: {detail}"
            ),
            Ok(()) => panic!("a closed pool must not read as a successful sweep"),
            Err(other) => panic!("a closed pool is retryable, not {other:?}"),
        }
        assert!(
            timed_mutes(&pool).await.is_empty(),
            "the healthy pool sees no work the broken one claimed to do"
        );
    }

    /// The sweep is wired into both registries the ticket names: the worker can
    /// run its kind, and the scheduler places it on the maintenance queue every
    /// ten minutes.
    #[test]
    fn the_kind_is_registered_and_scheduled() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_EXPIRE).is_some(),
            "the worker knows `{KIND_EXPIRE}`"
        );

        let job = periodic_jobs()
            .into_iter()
            .find(|job| job.kind == KIND_EXPIRE)
            .expect("the sweep is on the periodic schedule");
        assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
        assert_eq!(job.every, EVERY, "once every ten minutes");
    }

    /// The handler the registry binds to the kind is this module's
    /// [`handle_expire`], not some other sweep: calling the registered handler
    /// deletes the rows.
    #[tokio::test]
    async fn the_registered_handler_is_this_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        mute_expiring_in(&pool, &alice, &bob, -60).await;
        let registry = crate::job_registry();
        let handler = registry
            .get(KIND_EXPIRE)
            .expect("the sweep's kind is registered");

        let outcome = handler(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "the registered handler runs");
        assert!(
            timed_mutes(&pool).await.is_empty(),
            "and it is this module's sweep: the lapsed row is gone"
        );
    }
}
