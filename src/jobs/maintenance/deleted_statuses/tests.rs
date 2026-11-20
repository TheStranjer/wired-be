//! The deleted-status purge's tests: the stale local tombstone a daily pass
//! hard-deletes, the live rows, in-window deletions and remote tombstones it
//! leaves alone, the dependents that ride the cascade, the configuration that
//! widens or disables the window, the retry a database it cannot reach buys
//! it, and the two registries its kind is wired into.

use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::{
    config::{Config, TEST_BASE_URL},
    jobs::{
        QUEUE_BACKGROUND,
        maintenance::test_support::{
            DAY, EVERY, FIRST_ATTEMPT, app_state, broken_state, row_exists, state_with_config, user,
        },
        scheduler::periodic_jobs,
    },
    models::user::User,
};

/// Inserts a live status — never deleted, owned by this instance when
/// `local` — authored by `author`, and returns its id. Written
/// directly, the way the deletion fixtures are: the purge reads only
/// `deleted_at` and `local`, and the dependent cascade only needs the
/// row to exist.
async fn live_status_row(pool: &PgPool, author: &User, local: bool) -> Uuid {
    let id = Uuid::now_v7();
    let sql = "INSERT INTO statuses
                       (id, uri, url, user_id, visibility, conversation_id, local)
                   VALUES ($1, $2, $3, $4, 'public', $1, $5)";
    let written = query(sql)
        .bind(id)
        .bind(format!("{TEST_BASE_URL}/objects/{id}"))
        .bind(format!("{TEST_BASE_URL}/notice/{id}"))
        .bind(author.id)
        .bind(local)
        .execute(pool)
        .await
        .expect("the live status fixture inserts");
    assert_eq!(written.rows_affected(), 1, "one row was written");
    id
}

/// Inserts a status — owned by this instance when `local` — authored
/// by `author` whose soft deletion was stamped `secs` seconds in the
/// past, and returns its id. Same direct write as [`live_status_row`].
async fn deleted_status_row(pool: &PgPool, author: &User, local: bool, secs: i64) -> Uuid {
    let id = Uuid::now_v7();
    let sql = "INSERT INTO statuses
                       (id, uri, url, user_id, visibility, conversation_id, local, deleted_at)
                   VALUES ($1, $2, $3, $4, 'public', $1, $5,
                           now() - make_interval(secs => $6))";
    let written = query(sql)
        .bind(id)
        .bind(format!("{TEST_BASE_URL}/objects/{id}"))
        .bind(format!("{TEST_BASE_URL}/notice/{id}"))
        .bind(author.id)
        .bind(local)
        .bind(secs)
        .execute(pool)
        .await
        .expect("the deleted status fixture inserts");
    assert_eq!(written.rows_affected(), 1, "one row was written");
    id
}

/// Inserts a `media_attachments` row owned by `owner` attached to
/// `status_id`, and returns its id — the dependent the status purge
/// must take down with the status through the `status_id` cascade.
async fn media_row(pool: &PgPool, owner: &User, status_id: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    let sql = "INSERT INTO media_attachments (id, user_id, status_id, url, content_type)
                    VALUES ($1, $2, $3, $4, 'image/png')";
    let written = query(sql)
        .bind(id)
        .bind(owner.id)
        .bind(status_id)
        .bind(format!("{TEST_BASE_URL}/media/{id}.png"))
        .execute(pool)
        .await
        .expect("the media fixture inserts");
    assert_eq!(written.rows_affected(), 1, "one row was written");
    id
}

/// The status purge deletes exactly the stale soft-deleted LOCAL rows:
/// a local deletion inside the window, a live local post and — the
/// tombstone rule — a stale deleted REMOTE row all stay.
#[tokio::test]
async fn the_status_purge_deletes_only_stale_deleted_local_rows() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let stale_local = deleted_status_row(&pool, &alice, true, 31 * DAY).await;
    let stale_live = live_status_row(&pool, &alice, true).await;
    let stale_remote = deleted_status_row(&pool, &alice, false, 31 * DAY).await;
    let fresh_local = deleted_status_row(&pool, &alice, true, DAY).await;

    let outcome = handle_purge_deleted_statuses(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "a purge over stale and kept rows succeeds");
    assert!(
        !row_exists(&pool, "statuses", stale_local).await,
        "the local tombstone past the default 30-day window is gone"
    );
    assert!(
        row_exists(&pool, "statuses", stale_live).await,
        "a live local row is nobody's garbage, however old"
    );
    assert!(
        row_exists(&pool, "statuses", stale_remote).await,
        "the remote tombstone stays: its row keeps the deleted post from being re-imported"
    );
    assert!(
        row_exists(&pool, "statuses", fresh_local).await,
        "a local deletion inside the window stays"
    );
}

/// The dependents ride the cascade: a stale deleted local status
/// carrying a media attachment loses both, while the sweep itself
/// names only the `statuses` table.
#[tokio::test]
async fn the_status_purge_takes_dependent_rows_with_the_cascade() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let stale = deleted_status_row(&pool, &alice, true, 31 * DAY).await;
    let attachment = media_row(&pool, &alice, stale).await;

    let outcome = handle_purge_deleted_statuses(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "the cascade pass succeeds");
    assert!(
        !row_exists(&pool, "statuses", stale).await,
        "the stale local tombstone is gone"
    );
    assert!(
        !row_exists(&pool, "media_attachments", attachment).await,
        "and its attachment followed the status_id cascade"
    );
}

/// `purge_deleted_statuses_days = 0` on the config disables the sweep:
/// the stale local tombstone every other pass above reclaims stays
/// put, and the pass still ends clean.
#[tokio::test]
async fn zero_days_disables_the_status_purge() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let stale_local = deleted_status_row(&pool, &alice, true, 31 * DAY).await;
    let stale_remote = deleted_status_row(&pool, &alice, false, 31 * DAY).await;
    let mut config = Config::for_tests();
    config.purge_deleted_statuses_days = 0;

    let outcome =
        handle_purge_deleted_statuses(state_with_config(&pool, config), FIRST_ATTEMPT, json!({}))
            .await;

    assert_eq!(outcome, Ok(()), "the disabled pass is a clean no-op");
    assert!(
        row_exists(&pool, "statuses", stale_local).await,
        "the stale local tombstone survives while the purge is off"
    );
    assert!(
        row_exists(&pool, "statuses", stale_remote).await,
        "and the remote tombstone was never in question anyway"
    );
}

/// The purge's window comes from the configuration, not a literal: a
/// 45-day-old local tombstone sits inside an overridden 60-day window
/// the default 30-day one would reclaim.
#[tokio::test]
async fn the_status_purge_window_comes_from_the_config() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let aged = deleted_status_row(&pool, &alice, true, 45 * DAY).await;
    let mut config = Config::for_tests();
    config.purge_deleted_statuses_days = 60;

    let outcome =
        handle_purge_deleted_statuses(state_with_config(&pool, config), FIRST_ATTEMPT, json!({}))
            .await;

    assert_eq!(outcome, Ok(()), "the widened-window pass succeeds");
    assert!(
        row_exists(&pool, "statuses", aged).await,
        "a 45-day tombstone sits inside a 60-day window and stays"
    );
}

/// The status purge is wired into both registries the ticket names:
/// the worker can run its kind, and the scheduler places it on the
/// maintenance queue once a day.
#[test]
fn the_deleted_statuses_kind_is_registered_and_scheduled() {
    let registry = crate::job_registry();
    assert!(
        registry.get(KIND_DELETED_STATUSES).is_some(),
        "the worker knows `{KIND_DELETED_STATUSES}`"
    );
    assert_eq!(
        KIND_DELETED_STATUSES, "maintenance.deleted_statuses",
        "the ticket's kind name"
    );

    let job = periodic_jobs()
        .into_iter()
        .find(|job| job.kind == KIND_DELETED_STATUSES)
        .expect("the purge is on the periodic schedule");
    assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
    assert_eq!(job.every, EVERY, "once a day");
}

/// The handler the registry binds to the kind is this module's
/// [`handle_purge_deleted_statuses`], not some other sweep: calling
/// the registered handler reclaims the stale local tombstone and keeps
/// the remote one.
#[tokio::test]
async fn the_registered_status_handler_is_this_purge() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let stale_local = deleted_status_row(&pool, &alice, true, 31 * DAY).await;
    let stale_remote = deleted_status_row(&pool, &alice, false, 31 * DAY).await;
    let registry = crate::job_registry();
    let handler = registry
        .get(KIND_DELETED_STATUSES)
        .expect("the purge's kind is registered");

    let outcome = handler(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "the registered handler runs");
    assert!(
        !row_exists(&pool, "statuses", stale_local).await,
        "and it is this module's purge: the stale local tombstone is gone"
    );
    assert!(
        row_exists(&pool, "statuses", stale_remote).await,
        "and only the local tombstones went: the remote one stays"
    );
}

/// A purge that cannot reach the database is a retryable failure, so
/// the queue keeps the pass while the scheduler enqueues the next day:
/// the stale tombstones wait for a working purge instead of being
/// assumed gone.
#[tokio::test]
async fn an_unreachable_database_retries_the_status_purge() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let stale = deleted_status_row(&pool, &alice, true, 31 * DAY).await;
    let broken = broken_state().await;

    let outcome = handle_purge_deleted_statuses(broken, FIRST_ATTEMPT, json!({})).await;

    match outcome {
        Err(JobError::Retry(detail)) => assert!(
            detail.starts_with("deleted-status purge failed"),
            "the retry states what failed: {detail}"
        ),
        Ok(()) => panic!("a closed pool must not read as a successful purge"),
        Err(other) => panic!("a closed pool is retryable, not {other:?}"),
    }
    assert!(
        row_exists(&pool, "statuses", stale).await,
        "the healthy pool sees no work the broken one claimed to do"
    );
}
