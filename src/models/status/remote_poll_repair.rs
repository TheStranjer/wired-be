//! Tests of the `restore_remote_poll_statuses` migration (WBE-821): remote poll
//! posts the expiry sweep tombstoned come back, those a `Delete` removed stay
//! gone. Test-only.

use chrono::{DateTime, SubsecRound as _, TimeDelta, Utc};
use sqlx::PgPool;

use super::{
    Counter, NewStatus, Status, insert,
    test_support::{BASE, counter, fresh_user, mirrored, post, remote_user, stored},
};
use crate::models::poll::{self, PollParams};

/// The repair, as the migration runner applies it.
const REPAIR_SQL: &str =
    include_str!("../../../migrations/20261125070906_restore_remote_poll_statuses.sql");

/// Runs [`REPAIR_SQL`] in one transaction, which its temporary table needs.
async fn run_repair(pool: &PgPool) {
    let mut tx = pool.begin().await.expect("the transaction opens");
    sqlx::raw_sql(REPAIR_SQL)
        .execute(&mut *tx)
        .await
        .map(drop)
        .expect("the repair runs");
    tx.commit().await.expect("the repair commits");
}

/// A remote poll post whose status and poll both close at `ends`, the shape
/// the old parser stored, optionally answering `parent`.
async fn remote_poll_post(
    pool: &PgPool,
    uri: &str,
    ends: DateTime<Utc>,
    parent: Option<&Status>,
) -> Status {
    let bob = remote_user(pool, "bob").await;
    let new = NewStatus {
        expires_at: Some(ends),
        in_reply_to_id: parent.map(|row| row.id),
        ..mirrored(&bob, uri)
    };
    let row = insert(pool, BASE, &new)
        .await
        .expect("the remote post inserts");
    let params = PollParams {
        options: vec!["Red".to_owned(), "Blue".to_owned()],
        expires_at: Some(ends),
        ..PollParams::default()
    };
    let mut conn = pool.acquire().await.expect("a connection");
    let _poll = poll::create(&mut conn, row.id, &params)
        .await
        .expect("the poll inserts");
    row
}

#[tokio::test]
async fn a_sweep_tombstone_is_restored_with_its_counters() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = crate::test_fixtures::local_user(&pool, "alice").await;
    let parent = post(&pool, &alice, "a question for the network").await;
    let ends = (Utc::now() - TimeDelta::minutes(1)).trunc_subsecs(6);
    let victim = remote_poll_post(
        &pool,
        "https://remote.example/objects/swept",
        ends,
        Some(&parent),
    )
    .await;
    let before = fresh_user(&pool, victim.user_id).await.statuses_count;
    let _tombstone = super::soft_delete(&pool, victim.id)
        .await
        .expect("the sweep's delete runs");
    assert_eq!(
        counter(&pool, parent.id, Counter::Replies).await,
        0,
        "the tombstone stepped the reply count down"
    );

    run_repair(&pool).await;

    let restored = stored(&pool, victim.id).await;
    assert!(restored.deleted_at.is_none(), "the post is live again");
    assert_eq!(restored.expires_at, None, "and no longer expires");
    assert_eq!(
        counter(&pool, parent.id, Counter::Replies).await,
        1,
        "the parent's reply count is stepped back up"
    );
    assert_eq!(
        fresh_user(&pool, victim.user_id).await.statuses_count,
        before,
        "so is the author's post count"
    );
    let stand = poll::get_for_status(&pool, victim.id)
        .await
        .expect("the poll reads")
        .expect("the poll survived the tombstone");
    assert_eq!(stand.expires_at, Some(ends), "the poll keeps its end time");
}

#[tokio::test]
async fn a_delete_activity_tombstone_stays_deleted() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    // The origin deleted the post long after its poll closed: far outside the
    // sweep's window.
    let ends = (Utc::now() - TimeDelta::hours(2)).trunc_subsecs(6);
    let deleted =
        remote_poll_post(&pool, "https://remote.example/objects/deleted", ends, None).await;
    let _tombstone = super::soft_delete(&pool, deleted.id)
        .await
        .expect("the delete runs");

    run_repair(&pool).await;

    assert!(
        stored(&pool, deleted.id).await.deleted_at.is_some(),
        "a post the origin deleted stays deleted"
    );
}

#[tokio::test]
async fn a_live_remote_poll_post_only_loses_its_expiry() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let ends = (Utc::now() + TimeDelta::hours(1)).trunc_subsecs(6);
    let live = remote_poll_post(&pool, "https://remote.example/objects/open", ends, None).await;

    run_repair(&pool).await;

    let after = stored(&pool, live.id).await;
    assert!(after.deleted_at.is_none(), "still live");
    assert_eq!(after.expires_at, None, "the bogus expiry is cleared");
}

#[tokio::test]
async fn a_local_expiring_status_is_left_alone() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = crate::test_fixtures::local_user(&pool, "alice").await;
    let ends = (Utc::now() + TimeDelta::hours(1)).trunc_subsecs(6);
    let new = NewStatus {
        expires_at: Some(ends),
        ..NewStatus::local(&alice, "gone in an hour")
    };
    let row = insert(&pool, BASE, &new)
        .await
        .expect("the local post inserts");
    let mut conn = pool.acquire().await.expect("a connection");
    let params = PollParams {
        options: vec!["a".to_owned(), "b".to_owned()],
        expires_at: Some(ends),
        ..PollParams::default()
    };
    let _poll = poll::create(&mut conn, row.id, &params)
        .await
        .expect("the poll inserts");

    run_repair(&pool).await;

    assert_eq!(
        stored(&pool, row.id).await.expires_at,
        Some(ends),
        "a local post keeps the expiry its author asked for"
    );
}
