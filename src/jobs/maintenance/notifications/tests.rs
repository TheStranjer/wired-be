//! The dismissed-notification sweep's tests: the stale dismissal the daily
//! pass reclaims, the recent and undismissed rows it leaves, the retry a
//! database it cannot reach buys it, and the two registries its kind is wired
//! into.

use serde_json::json;
use sqlx::query_scalar;
use uuid::Uuid;

use super::*;
use crate::{
    jobs::{
        JobError, QUEUE_BACKGROUND,
        maintenance::test_support::{DAY, EVERY, FIRST_ATTEMPT, user},
        scheduler::periodic_jobs,
    },
    models::user::User,
    test_support::{app_state, broken_state},
};

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
    query_scalar::<_, Uuid>("SELECT id FROM notifications ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("the rows read")
}

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
            detail.starts_with("dismissed notifications deleted failed"),
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
    let registry = crate::job_registry(&crate::config::Config::for_tests());
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
    let registry = crate::job_registry(&crate::config::Config::for_tests());
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
