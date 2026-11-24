//! The publish claim: at most one runner posts a due schedule, none posts it
//! before its moment, and a failed publish leaves the row unlocked.

use super::*;

/// A due schedule for `who` carrying a plain one-line post.
async fn due_row(pool: &PgPool, who: &User, text: &str) -> ScheduledStatus {
    parked(
        pool,
        who,
        Utc::now() - TimeDelta::minutes(1),
        json!({ "status": text }),
    )
    .await
}

#[tokio::test]
async fn a_row_another_runner_holds_is_left_alone_by_both_runners() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let row = due_row(&pool, &alice, "held elsewhere").await;
    let state = app_state(&pool);

    let mut holder = pool.begin().await.expect("the holder's transaction opens");
    let held = scheduled_status::lock_due(&mut holder, row.id)
        .await
        .expect("the holder's claim runs")
        .expect("the holder claims the due row");
    assert_eq!(held.id, row.id, "the holder has the row");

    let fire = handle_publish(state.clone(), FIRST_ATTEMPT, job_args(row.id)).await;
    let sweep = handle_sweep(state, FIRST_ATTEMPT, json!({})).await;

    assert_eq!(fire, Ok(()), "the per-row fire yields to the holder");
    assert_eq!(sweep, Ok(()), "and so does the sweep, without looping");
    assert_eq!(
        post_count(&pool).await,
        0,
        "neither runner created a status"
    );
    assert!(
        fetched(&pool, row.id).await.is_some(),
        "the row is still there for the holder to finish"
    );

    // The holder publishes, so its delete and commit are what remove the row.
    assert!(
        scheduled_status::delete_on(&mut holder, row.id)
            .await
            .expect("the holder's delete runs"),
        "the holder deletes its claimed row"
    );
    holder.commit().await.expect("the holder commits");

    let later = handle_publish(app_state(&pool), FIRST_ATTEMPT, job_args(row.id)).await;
    assert_eq!(later, Ok(()), "a fire after the holder is done is a no-op");
    assert_eq!(
        post_count(&pool).await,
        0,
        "the second runner created no status at all"
    );
    assert!(
        fetched(&pool, row.id).await.is_none(),
        "the holder's commit removed the row"
    );
}

#[tokio::test]
async fn the_fire_then_the_sweep_post_a_due_row_once() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let row = due_row(&pool, &alice, "exactly once").await;
    let state = app_state(&pool);

    let fire = handle_publish(state.clone(), FIRST_ATTEMPT, job_args(row.id)).await;
    let sweep = handle_sweep(state, FIRST_ATTEMPT, json!({})).await;

    assert_eq!(fire, Ok(()), "the fire publishes");
    assert_eq!(sweep, Ok(()), "the sweep finds nothing left");
    let posts = posts_of(&pool, &alice).await;
    assert_eq!(posts.len(), 1, "one status, not two: {posts:?}");
    assert_eq!(posts[0].raw_text, "exactly once", "with the stored text");
    assert!(fetched(&pool, row.id).await.is_none(), "the row is spent");
}

#[tokio::test]
async fn publishing_a_row_twice_in_sequence_claims_it_once() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let row = due_row(&pool, &alice, "claimed once").await;
    let state = app_state(&pool);

    let first = publish_one(&state, row.id).await;
    let second = publish_one(&state, row.id).await;

    assert_eq!(first, Ok(true), "the first runner claims and posts");
    assert_eq!(second, Ok(false), "the second finds the row gone");
    assert_eq!(post_count(&pool).await, 1, "exactly one status exists");
}

#[tokio::test]
async fn a_row_moved_into_the_future_is_not_fired_early() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let row = due_row(&pool, &alice, "postponed").await;
    // The user moves the schedule out after the job was queued for the old
    // moment, and that stale job fires.
    let moved =
        scheduled_status::reschedule(&pool, row.id, alice.id, Utc::now() + TimeDelta::hours(3))
            .await
            .expect("the reschedule runs")
            .expect("the owner's row moved");
    assert!(moved.scheduled_at > Utc::now(), "the row now waits");
    let state = app_state(&pool);

    let stale_fire = handle_publish(state.clone(), FIRST_ATTEMPT, job_args(row.id)).await;
    let sweep = handle_sweep(state, FIRST_ATTEMPT, json!({})).await;

    assert_eq!(stale_fire, Ok(()), "the stale fire completes quietly");
    assert_eq!(sweep, Ok(()), "and the sweep has nothing due");
    assert_eq!(post_count(&pool).await, 0, "nothing posted early");
    assert!(
        fetched(&pool, row.id).await.is_some(),
        "the postponed row waits for its new moment"
    );
}

#[tokio::test]
async fn a_refused_create_still_deletes_the_row_in_the_claim() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let row = due_row(&pool, &alice, "").await;

    let outcome = publish_one(&app_state(&pool), row.id).await;

    assert_eq!(outcome, Ok(true), "the runner claimed and dropped the row");
    assert!(
        fetched(&pool, row.id).await.is_none(),
        "the refused row was deleted and the delete committed"
    );
    assert_eq!(post_count(&pool).await, 0, "no status was written");
}

#[tokio::test]
async fn a_retryable_failure_leaves_the_row_in_place_and_unlocked() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let row = due_row(&pool, &alice, "will retry").await;
    let broken = {
        let unusable = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        unusable.close().await;
        app_state(&unusable)
    };

    // The claim is on the live pool; the publish's own reads go through the
    // closed one, so the failure strikes after the row is locked.
    let mut tx = pool.begin().await.expect("the claim transaction opens");
    let claimed = scheduled_status::lock_due(&mut tx, row.id)
        .await
        .expect("the claim runs")
        .expect("the due row is claimed");
    let outcome = publish_locked(&broken, &mut tx, &claimed).await;
    roll_back(tx).await;

    match outcome {
        Err(JobError::Retry(detail)) => assert!(
            detail.starts_with("scheduled status user read failed"),
            "the retry states what failed: {detail}"
        ),
        other => panic!("a database failure is retryable, not {other:?}"),
    }
    assert!(
        fetched(&pool, row.id).await.is_some(),
        "the row survived the failed publish"
    );
    let mut next = pool.begin().await.expect("the next transaction opens");
    assert!(
        scheduled_status::lock_due(&mut next, row.id)
            .await
            .expect("the second claim runs")
            .is_some(),
        "the rolled-back claim left the row unlocked"
    );
    roll_back(next).await;
    assert_eq!(post_count(&pool).await, 0, "and nothing was posted");
}
