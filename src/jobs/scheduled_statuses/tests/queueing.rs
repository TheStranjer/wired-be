//! The queueing side: [`schedule`] and [`reschedule`] and the publish job
//! rows they leave behind.

use chrono::SubsecRound as _;

use super::*;

#[tokio::test]
async fn schedule_queues_one_job_due_exactly_at_the_parked_moment() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let row = parked(
        &pool,
        &alice,
        Utc::now() + TimeDelta::hours(1),
        json!({ "status": "hello tomorrow" }),
    )
    .await;
    let state = app_state(&pool);

    schedule(&state, &row).await;
    // The dedupe key swallows the second claim, so a replayed create
    // cannot queue the same fire twice.
    schedule(&state, &row).await;

    let rows = sqlx::query_as::<_, crate::models::job::Job>(
        "SELECT * FROM jobs WHERE kind = $1 ORDER BY id",
    )
    .bind(KIND_PUBLISH)
    .fetch_all(&pool)
    .await
    .expect("the job rows read");
    assert_eq!(rows.len(), 1, "one job per schedule, second call deduped");
    let job = &rows[0];
    assert_eq!(job.queue, QUEUE_SCHEDULED, "on the scheduled queue");
    assert_eq!(
        job.args,
        job_args(row.id),
        "the args name exactly the schedule id"
    );
    assert_eq!(
        job.args["scheduled_id"],
        row.id.to_string(),
        "the key is scheduled_id"
    );
    assert_eq!(
        job.run_at, row.scheduled_at,
        "due at the parked moment, to the microsecond"
    );
    assert_eq!(
        job.unique_key.as_deref(),
        Some(unique_key(row.id).as_str()),
        "the ticket's dedupe key"
    );
    assert_eq!(job.max_attempts, 5, "default attempts");
}

/// The `scheduled_statuses.publish` job rows, oldest id first.
async fn publish_jobs(pool: &PgPool) -> Vec<crate::models::job::Job> {
    sqlx::query_as::<_, crate::models::job::Job>("SELECT * FROM jobs WHERE kind = $1 ORDER BY id")
        .bind(KIND_PUBLISH)
        .fetch_all(pool)
        .await
        .expect("the job rows read")
}

#[tokio::test]
async fn reschedule_replaces_the_waiting_fire_at_the_new_moment() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let row = parked(
        &pool,
        &alice,
        Utc::now() + TimeDelta::hours(1),
        json!({ "status": "hello tomorrow" }),
    )
    .await;
    let state = app_state(&pool);
    schedule(&state, &row).await;
    let waiting = publish_jobs(&pool).await;
    assert_eq!(waiting.len(), 1, "the create queued one fire");
    assert_eq!(waiting[0].run_at, row.scheduled_at, "due at the old moment");

    // The endpoint's own move, then the re-queue it runs afterwards.
    // Whole microseconds, because that is what a `timestamptz` keeps and
    // the comparison below is against a row read back out of one.
    let later = Utc::now().trunc_subsecs(6) + TimeDelta::hours(6);
    let moved = scheduled_status::reschedule(&pool, row.id, alice.id, later)
        .await
        .expect("the reschedule runs")
        .expect("the owner's row moved");
    reschedule(&state, &moved).await;

    let after = publish_jobs(&pool).await;
    assert_eq!(
        after.len(),
        1,
        "the old fire went and the new one took its place"
    );
    assert_eq!(after[0].run_at, later, "due at the new moment");
    assert_ne!(
        after[0].id, waiting[0].id,
        "a fresh row, not the old one edited"
    );
    assert_eq!(
        after[0].unique_key.as_deref(),
        Some(unique_key(row.id).as_str()),
        "and still exactly one key per schedule"
    );
}

#[tokio::test]
async fn reschedule_queues_a_fire_when_none_was_waiting() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let row = parked(
        &pool,
        &alice,
        Utc::now() + TimeDelta::hours(2),
        json!({ "status": "never queued" }),
    )
    .await;
    let state = app_state(&pool);
    assert!(
        publish_jobs(&pool).await.is_empty(),
        "the fixture row has no fire waiting"
    );

    // A lost or already-claimed job leaves nothing to cancel; the moved
    // schedule still gets its fire, and twice over the key holds one.
    reschedule(&state, &row).await;
    reschedule(&state, &row).await;

    let after = publish_jobs(&pool).await;
    assert_eq!(after.len(), 1, "one fire for the row, deduped by key");
    assert_eq!(
        after[0].run_at, row.scheduled_at,
        "due at the moment the row carries"
    );
}
