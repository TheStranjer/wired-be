//! Tests for the sweep's actor `Delete` announcement: a failed announcement
//! retries the job before any follow row is cleared.

use super::super::test_support::{
    accepted_follow, app_state, broken_state, count_where, follows, has_follow, local_user, run,
};
use super::*;
use crate::models::user::{Flag, NewUser, insert};

/// A remote follower of `alice` with a shared inbox, accepted.
async fn remote_fan_of(pool: &sqlx::PgPool, alice: &user::User) -> user::User {
    let fan = insert(
        pool,
        &NewUser {
            nickname: "fan@peer.example".to_owned(),
            ap_id: "https://peer.example/users/fan".to_owned(),
            local: Flag::new(false),
            domain: Some("peer.example".to_owned()),
            inbox: Some("https://peer.example/users/fan/inbox".to_owned()),
            shared_inbox: Some("https://peer.example/inbox".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the remote fixture inserts");
    accepted_follow(pool, &fan, alice).await;
    fan
}

/// The number of queued `federator.publish` jobs.
async fn publish_job_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE kind = 'federator.publish'")
        .fetch_one(pool)
        .await
        .expect("the publish jobs count")
}

/// When the recipient reads fail, the announcement errors, the job
/// retries, and no follow row is cleared; the next healthy attempt still
/// finds its recipients and queues the `Delete`.
#[tokio::test]
async fn a_failed_announcement_retries_and_keeps_every_follow_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    let _fan = remote_fan_of(&pool, &alice).await;
    follows(&pool, &alice, &bob).await;
    let broken = broken_state().await;

    let err = crate::services::federation_out::after_account_deleted(&broken, &alice)
        .await
        .expect_err("a closed pool cannot read the recipients");
    match retry(&err) {
        JobError::Retry(detail) => assert!(
            detail.starts_with("account delete pass failed"),
            "the retry states what failed: {detail}"
        ),
        other @ (JobError::RetryAfter(..) | JobError::Discard(_)) => {
            panic!("an announcement failure must retry, not {other:?}")
        }
    }
    match run(&broken, alice.id).await {
        Err(JobError::Retry(_)) => {}
        other => panic!("the failed pass must retry, not {other:?}"),
    }
    assert_eq!(
        count_where(&pool, "follows", "followee_id", alice.id).await,
        1,
        "the follower's row survives the failed attempt"
    );
    assert!(
        has_follow(&pool, &alice, &bob).await,
        "so does the follow alice holds"
    );
    assert_eq!(publish_job_count(&pool).await, 0, "nothing was queued");

    run(&app_state(&pool), alice.id)
        .await
        .expect("the retry runs");

    assert_eq!(publish_job_count(&pool).await, 1, "the retry queues it");
    assert!(
        !has_follow(&pool, &alice, &bob).await,
        "and only then do the follow rows go"
    );
}

/// An instance that does not federate announces nothing, succeeds, and
/// still runs the sweep.
#[tokio::test]
async fn with_federation_disabled_the_sweep_runs_without_a_delete() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let _fan = remote_fan_of(&pool, &alice).await;
    let mut config = crate::config::Config::for_tests();
    config.federation_enabled = crate::config::Toggle::new(false);
    let state = AppState::for_tests(pool.clone(), config);

    crate::services::federation_out::after_account_deleted(&state, &alice)
        .await
        .expect("a disabled federation is not an error");
    run(&state, alice.id).await.expect("the delete job runs");

    assert_eq!(publish_job_count(&pool).await, 0, "no Delete is queued");
    assert_eq!(
        count_where(&pool, "follows", "followee_id", alice.id).await,
        0,
        "the sweep cleared the follow rows"
    );
}
