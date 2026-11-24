//! Tests of the claim fence on [`complete`] and [`fail`]: a write from an
//! attempt whose claim was rescued must change nothing.

use sqlx::PgPool;

use super::{
    tests::{fetch, new_job},
    *,
};

/// Enqueues a job on `queue`, claims it, ages its lock past the stuck
/// limit, rescues it and claims it again; returns the second claim
/// (`attempts = 2`), leaving the row `executing` under it.
async fn claim_rescue_and_reclaim(pool: &PgPool, queue: &'static str) -> Job {
    let enqueued = enqueue(pool, new_job(queue, "slow"))
        .await
        .unwrap()
        .expect("the job inserts");
    assert_eq!(enqueued.queue, queue, "the job went to the asked queue");
    let first = claim_next(pool, &[queue])
        .await
        .unwrap()
        .expect("the first claim succeeds");
    assert_eq!(first.attempts, 1, "the first claim is attempt one");
    let aged = sqlx::query("UPDATE jobs SET locked_at = now() - interval '1 hour' WHERE id = $1")
        .bind(first.id)
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(aged.rows_affected(), 1, "the lock was aged");
    let rescued = rescue_stuck(pool, Duration::from_secs(600)).await.unwrap();
    assert_eq!(rescued, 1, "the aged lock is rescued");
    let second = claim_next(pool, &[queue])
        .await
        .unwrap()
        .expect("the rescued job is claimed again");
    assert_eq!(second.id, first.id, "it is the same row");
    assert_eq!(second.attempts, 2, "the new claim is attempt two");
    second
}

#[tokio::test]
async fn complete_with_the_current_attempt_completes() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let enqueued = enqueue(&pool, new_job("default", "quick"))
        .await
        .unwrap()
        .expect("the job inserts");
    assert_eq!(enqueued.queue, "default", "the job went to its queue");
    let claimed = claim_next(&pool, &["default"])
        .await
        .unwrap()
        .expect("the job is claimed");

    let written = complete(&pool, claimed.id, claimed.attempts).await.unwrap();
    assert!(written, "the claim that ran the attempt may complete it");
    let row = fetch(&pool, claimed.id).await;
    assert_eq!(row.state, "completed", "the row is terminal");
    assert!(row.locked_at.is_none(), "and the lock is released");

    let again = complete(&pool, claimed.id, claimed.attempts).await.unwrap();
    assert!(!again, "a row that is no longer executing is not rewritten");
    let unknown = complete(&pool, Uuid::nil(), 1).await.unwrap();
    assert!(!unknown, "an unknown id matches nothing");
}

#[tokio::test]
async fn complete_with_a_stale_attempt_changes_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let second = claim_rescue_and_reclaim(&pool, "default").await;

    let written = complete(&pool, second.id, 1).await.unwrap();
    assert!(!written, "attempt one lost its claim to attempt two");
    let row = fetch(&pool, second.id).await;
    assert_eq!(row.state, "executing", "the second claim still runs");
    assert_eq!(row.attempts, 2, "its attempt count is intact");
    assert!(row.locked_at.is_some(), "and it keeps its lock");

    let current = complete(&pool, second.id, 2).await.unwrap();
    assert!(current, "the second claim can still finish");
}

#[tokio::test]
async fn fail_with_a_stale_attempt_changes_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();

    // Retry arm.
    let retrying = claim_rescue_and_reclaim(&pool, "retrying").await;
    let written = fail(&pool, retrying.id, 1, "late", Some(Duration::from_secs(60)))
        .await
        .unwrap();
    assert!(!written, "a stale retry write matches no row");
    let row = fetch(&pool, retrying.id).await;
    assert_eq!(row.state, "executing", "the second claim still runs");
    assert_eq!(row.attempts, 2, "its attempt count is intact");
    assert!(row.last_error.is_none(), "no stale error is recorded");
    assert!(row.locked_at.is_some(), "and it keeps its lock");

    // Discard arm.
    let discarding = claim_rescue_and_reclaim(&pool, "discarding").await;
    let discarded = fail(&pool, discarding.id, 1, "late", None).await.unwrap();
    assert!(!discarded, "a stale discard write matches no row");
    let kept = fetch(&pool, discarding.id).await;
    assert_eq!(kept.state, "executing", "the second claim still runs");
    assert_eq!(kept.attempts, 2, "its attempt count is intact");
    assert!(kept.last_error.is_none(), "no stale error is recorded");

    let current = fail(&pool, discarding.id, 2, "real", None).await.unwrap();
    assert!(current, "the current claim's failure lands");
    assert_eq!(
        fetch(&pool, discarding.id).await.state,
        "discarded",
        "and discards the job"
    );
}
