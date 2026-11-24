//! Tests of the attempt budget at the stuck-lock sweep and at the claim: a
//! job that keeps losing its worker is discarded, never rescued forever.

use sqlx::PgPool;

use super::{
    tests::{fetch, new_job},
    *,
};

/// Enqueues a job with `max_attempts` on the default queue and claims it
/// `claims` times, ageing and rescuing its lock between claims, so the row
/// ends `executing` under the last claim with `attempts == claims`.
async fn claim_n_times(pool: &PgPool, max_attempts: i32, claims: i32) -> Job {
    let enqueued = enqueue(
        pool,
        NewJob {
            max_attempts: Some(max_attempts),
            ..new_job("default", "poison")
        },
    )
    .await
    .unwrap()
    .expect("the job inserts");
    let mut claimed = None;
    for round in 1..=claims {
        if round > 1 {
            let rescued = rescue_stuck(pool, Duration::from_secs(600)).await.unwrap();
            assert_eq!(rescued, 1, "the previous claim is rescued");
        }
        let job = claim_next(pool, &["default"])
            .await
            .unwrap()
            .expect("the job is claimable while attempts remain");
        assert_eq!(job.id, enqueued.id, "it is the same row each time");
        assert_eq!(job.attempts, round, "each claim is one more attempt");
        age_lock(pool, job.id).await;
        claimed = Some(job);
    }
    claimed.expect("at least one claim was asked for")
}

/// Back-dates the lock of `id` by an hour, past any sweep limit used here.
async fn age_lock(pool: &PgPool, id: Uuid) {
    let aged = sqlx::query("UPDATE jobs SET locked_at = now() - interval '1 hour' WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(aged.rows_affected(), 1, "the lock was aged");
}

#[tokio::test]
async fn rescue_returns_a_stuck_job_with_attempts_left() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let stuck = claim_n_times(&pool, 3, 1).await;

    let rescued = rescue_stuck(&pool, Duration::from_secs(600)).await.unwrap();
    assert_eq!(rescued, 1, "the expired lock is rescued");
    let row = fetch(&pool, stuck.id).await;
    assert_eq!(row.state, "retryable", "attempts remain, so it retries");
    assert!(row.locked_at.is_none(), "the lock is released");
    assert!(
        row.last_error.is_none(),
        "a rescue below budget adds no error"
    );
    assert_eq!(row.attempts, 1, "the lost attempt stays counted");
}

#[tokio::test]
async fn rescue_discards_a_stuck_job_at_its_budget() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let stuck = claim_n_times(&pool, 2, 2).await;
    assert_eq!(stuck.attempts, stuck.max_attempts, "the budget is spent");

    let rescued = rescue_stuck(&pool, Duration::from_secs(600)).await.unwrap();
    assert_eq!(rescued, 1, "the discarded row still counts");
    let row = fetch(&pool, stuck.id).await;
    assert_eq!(
        row.state, "discarded",
        "no attempts left, so it is terminal"
    );
    assert_eq!(
        row.last_error.as_deref(),
        Some("lock expired at the attempt budget"),
        "the reason is recorded"
    );
    assert!(row.locked_at.is_none(), "the lock is released");

    let after = claim_next(&pool, &["default"]).await.unwrap();
    assert!(after.is_none(), "a discarded job is never claimed again");
}

#[tokio::test]
async fn rescue_keeps_the_last_error_of_a_job_below_its_budget() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let stuck = claim_n_times(&pool, 3, 1).await;
    let marked = sqlx::query("UPDATE jobs SET last_error = 'earlier failure' WHERE id = $1")
        .bind(stuck.id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(marked.rows_affected(), 1, "the earlier error was stored");

    let rescued = rescue_stuck(&pool, Duration::from_secs(600)).await.unwrap();
    assert_eq!(rescued, 1, "the expired lock is rescued");
    let row = fetch(&pool, stuck.id).await;
    assert_eq!(row.state, "retryable", "attempts remain");
    assert_eq!(
        row.last_error.as_deref(),
        Some("earlier failure"),
        "the earlier error survives the rescue"
    );
}

#[tokio::test]
async fn rescue_ignores_fresh_locks() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let enqueued = enqueue(
        &pool,
        NewJob {
            max_attempts: Some(1),
            ..new_job("default", "running")
        },
    )
    .await
    .unwrap()
    .expect("the job inserts");
    let claimed = claim_next(&pool, &["default"])
        .await
        .unwrap()
        .expect("the job is claimed");
    assert_eq!(claimed.id, enqueued.id, "it is the enqueued row");

    let rescued = rescue_stuck(&pool, Duration::from_secs(600)).await.unwrap();
    assert_eq!(rescued, 0, "a fresh lock is not stuck, even at the budget");
    let row = fetch(&pool, claimed.id).await;
    assert_eq!(row.state, "executing", "the job keeps running");
    assert!(row.locked_at.is_some(), "and keeps its lock");
    assert!(row.last_error.is_none(), "no error was written");
}

#[tokio::test]
async fn repeated_rescues_stop_at_max_attempts() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let last = claim_n_times(&pool, 3, 3).await;

    let rescued = rescue_stuck(&pool, Duration::from_secs(600)).await.unwrap();
    assert_eq!(rescued, 1, "the expired lock is settled");
    let row = fetch(&pool, last.id).await;
    assert_eq!(row.state, "discarded", "the third lost claim ends the job");
    assert_eq!(row.attempts, 3, "attempts never pass the budget");
}

#[tokio::test]
async fn claim_skips_jobs_at_their_budget() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let stuck = claim_n_times(&pool, 2, 1).await;
    let rescued = rescue_stuck(&pool, Duration::from_secs(600)).await.unwrap();
    assert_eq!(rescued, 1, "the expired lock is rescued");
    let spent = sqlx::query("UPDATE jobs SET attempts = max_attempts WHERE id = $1")
        .bind(stuck.id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(spent.rows_affected(), 1, "the budget was spent");
    let row = fetch(&pool, stuck.id).await;
    assert_eq!(row.state, "retryable", "the row is due and retryable");

    let claimed = claim_next(&pool, &["default"]).await.unwrap();
    assert!(claimed.is_none(), "no claim once attempts hit the budget");
    let unchanged = fetch(&pool, stuck.id).await;
    assert_eq!(unchanged.attempts, 2, "the refused claim costs no attempt");
}
