//! Periodic maintenance jobs: the housekeeping sweeps that reclaim rows whose
//! useful life has ended.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres rows;
//! this module holds the recurring sweeps whose kinds live under the
//! `maintenance.` prefix. Each entry is one pass over one table, enqueued by
//! the [`periodic_jobs`](super::scheduler::periodic_jobs) schedule and run
//! through the [`JobRegistry`](super::JobRegistry) like every other kind.
//!
//! [`handle_notifications`] (job kind [`KIND_NOTIFICATIONS`]) is the first.
//! Dismissal is soft: the dismissal endpoints stamp
//! [`dismissed_at`](crate::models::notification::Notification::dismissed_at)
//! (`migrations/1794145737_notification_dismissed_at.sql`) and every read
//! filters those rows out, so they stay on the books forever once hidden.
//! The sweep deletes the ones dismissed longer than `RETENTION_DAYS` ago,
//! once per day, the window Pleroma-style retention gives them.

use serde_json::Value;
use sqlx::{PgPool, query};
use tracing::info;

use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError},
    state::AppState,
};

/// The job kind of the dismissed-notification sweep, run from the
/// [`periodic_jobs`](super::scheduler::periodic_jobs) registry through the
/// [`JobRegistry`](super::JobRegistry) handler [`handle_notifications`], on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND).
pub const KIND_NOTIFICATIONS: &str = "maintenance.notifications";

/// How long a dismissed notification stays in the table before the sweep
/// reclaims it: thirty days. Reads stopped answering it the moment it was
/// dismissed; this is only how long the row itself lives on.
const RETENTION_DAYS: i32 = 30;

/// The `DELETE` behind [`handle_notifications`]: every notification row
/// whose soft dismissal (`dismissed_at IS NOT NULL`) is older than
/// [`RETENTION_DAYS`] days.
const SWEEP_SQL: &str = "DELETE FROM notifications
        WHERE dismissed_at IS NOT NULL
          AND dismissed_at < now() - make_interval(days => $1)";

/// Runs one sweep and returns the number of rows it deleted.
///
/// Split out from [`handle_notifications`] so the handler is only the
/// logging-and-error shape the [`JobRegistry`](super::JobRegistry) expects.
async fn delete_dismissed(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let deleted = query(SWEEP_SQL).bind(RETENTION_DAYS).execute(pool).await?;
    Ok(deleted.rows_affected())
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_NOTIFICATIONS`]:
/// one sweep of the notifications dismissed longer than `RETENTION_DAYS`
/// days ago, logging how many rows it deleted.
///
/// A failed sweep is a [`JobError::Retry`], and the scheduler enqueues the
/// next period regardless, so the same sweep runs again shortly; the rows are
/// only invisible, never needed, so a pass lost to a database outage costs
/// nothing but the storage.
pub fn handle_notifications(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        match delete_dismissed(&state.db).await {
            Ok(deleted) => {
                info!(deleted, "dismissed notifications deleted");
                Ok(())
            }
            Err(err) => Err(JobError::Retry(format!(
                "dismissed notification sweep failed: {err}"
            ))),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        http_client::HttpClient,
        jobs::{QUEUE_BACKGROUND, scheduler::periodic_jobs},
        models::user::{NewUser, User, insert, local_ap_id},
        streaming::hub::Hub,
        uploads::UploaderBackend,
    };

    /// The period the sweep is scheduled on: one day.
    const EVERY: Duration = Duration::from_hours(24);

    /// One day in the seconds the fixture helpers count dismissals in.
    const DAY: i64 = 86_400;

    /// Test state over `pool` with the fixed test config.
    fn app_state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
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

    /// Inserts a notification for `recipient` caused by `actor` whose
    /// dismissal is stamped `secs` seconds in the past, so a test can put the
    /// table into the state the sweep is about without waiting it out.
    async fn dismissed_ago(pool: &PgPool, recipient: &User, actor: &User, secs: i64) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO notifications (id, user_id, from_account_id, type, dismissed_at)
                    VALUES ($1, $2, $3, 'mention', now() - make_interval(secs => $4))";
        let written = query(sql)
            .bind(id)
            .bind(recipient.id)
            .bind(actor.id)
            .bind(secs)
            .execute(pool)
            .await
            .expect("the fixture notification inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Inserts a notification for `recipient` caused by `actor` that was
    /// never dismissed, returning its id.
    async fn undismissed(pool: &PgPool, recipient: &User, actor: &User) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO notifications (id, user_id, from_account_id, type, dismissed_at)
                    VALUES ($1, $2, $3, 'mention', NULL)";
        let written = query(sql)
            .bind(id)
            .bind(recipient.id)
            .bind(actor.id)
            .execute(pool)
            .await
            .expect("the fixture notification inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// The ids of every notification row left in the table, ordered, so a
    /// test can see exactly which rows survived the sweep.
    async fn surviving(pool: &PgPool) -> Vec<Uuid> {
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM notifications ORDER BY id")
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
    async fn the_handler_deletes_only_the_stale_dismissed_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let stale = dismissed_ago(&pool, &alice, &bob, 31 * DAY).await;
        let recent = dismissed_ago(&pool, &alice, &bob, DAY).await;
        let live = undismissed(&pool, &alice, &bob).await;

        let outcome = handle_notifications(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "a sweep of stale rows succeeds");
        let survivors = surviving(&pool).await;
        assert!(
            !survivors.contains(&stale),
            "the row dismissed beyond the retention window is gone"
        );
        let mut expected = vec![recent, live];
        expected.sort();
        assert_eq!(
            survivors, expected,
            "only the stale row is gone: the recently dismissed one and \
             the undismissed one stay"
        );
    }

    #[tokio::test]
    async fn the_handler_passes_over_a_table_it_need_not_touch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let outcome = handle_notifications(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "nothing to delete is a clean pass");
        assert!(surviving(&pool).await.is_empty(), "and it wrote nothing");
    }

    /// A sweep that cannot reach the database is a retryable failure, so the
    /// queue keeps the pass while the scheduler enqueues the next period: the
    /// stale rows wait for a working sweep instead of being assumed gone.
    #[tokio::test]
    async fn an_unreachable_database_retries_the_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = broken_state().await;

        let outcome = handle_notifications(state, FIRST_ATTEMPT, json!({})).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.starts_with("dismissed notification sweep failed"),
                "the retry states what failed: {detail}"
            ),
            Ok(()) => panic!("a closed pool must not read as a successful sweep"),
            Err(other) => panic!("a closed pool is retryable, not {other:?}"),
        }
        assert!(
            surviving(&pool).await.is_empty(),
            "the healthy pool sees no work the broken one claimed to do"
        );
    }

    /// The sweep is wired into both registries the ticket names: the worker
    /// can run its kind, and the scheduler places it on the maintenance queue
    /// once a day.
    #[test]
    fn the_kind_is_registered_and_scheduled() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_NOTIFICATIONS).is_some(),
            "the worker knows `{KIND_NOTIFICATIONS}`"
        );

        let job = periodic_jobs()
            .into_iter()
            .find(|job| job.kind == KIND_NOTIFICATIONS)
            .expect("the sweep is on the periodic schedule");
        assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
        assert_eq!(job.every, EVERY, "once a day");
    }

    /// The handler the registry binds to the kind is this module's
    /// [`handle_notifications`], not some other sweep: calling the registered
    /// handler deletes the rows.
    #[tokio::test]
    async fn the_registered_handler_is_this_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let stale = dismissed_ago(&pool, &alice, &bob, 31 * DAY).await;
        let registry = crate::job_registry();
        let handler = registry
            .get(KIND_NOTIFICATIONS)
            .expect("the sweep's kind is registered");

        let outcome = handler(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "the registered handler runs");
        let survivors = surviving(&pool).await;
        assert!(
            !survivors.contains(&stale),
            "and it is this module's sweep: the stale row is gone"
        );
        assert!(survivors.is_empty(), "and nothing was left behind");
    }
}
