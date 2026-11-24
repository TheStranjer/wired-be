//! The publish and the media it claims: a schedule's own reservation never
//! blocks its fire, and another schedule's reservation still does.

use super::*;

/// A due schedule for `who` carrying one post that names `media`.
async fn due_with_media(pool: &PgPool, who: &User, text: &str, media: Uuid) -> ScheduledStatus {
    parked(
        pool,
        who,
        Utc::now() - TimeDelta::minutes(1),
        json!({ "status": text, "media_ids": [media.to_string()] }),
    )
    .await
}

#[tokio::test]
async fn a_schedule_claims_the_upload_it_reserves_itself() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let pic = upload(&pool, &alice).await;
    let row = due_with_media(&pool, &alice, "with its own picture", pic.id).await;

    let outcome = handle_publish(app_state(&pool), FIRST_ATTEMPT, job_args(row.id)).await;

    assert_eq!(outcome, Ok(()), "the schedule publishes");
    assert!(
        fetched(&pool, row.id).await.is_none(),
        "the fired schedule is gone"
    );
    let posts = posts_of(&pool, &alice).await;
    assert_eq!(posts.len(), 1, "exactly one status was written");
    let claimed = media_attachment::get_by_id(&pool, pic.id)
        .await
        .expect("the media reads")
        .expect("the fixture upload is stored");
    assert_eq!(
        claimed.status_id,
        Some(posts[0].id),
        "its own reservation did not stop the claim"
    );
}

#[tokio::test]
async fn an_upload_another_schedule_reserves_drops_the_row_unpublished() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let pic = upload(&pool, &alice).await;
    let row = due_with_media(&pool, &alice, "first claim", pic.id).await;
    let _other: ScheduledStatus = parked(
        &pool,
        &alice,
        Utc::now() + TimeDelta::hours(3),
        json!({ "status": "second claim", "media_ids": [pic.id.to_string()] }),
    )
    .await;

    let outcome = handle_publish(app_state(&pool), FIRST_ATTEMPT, job_args(row.id)).await;

    assert_eq!(outcome, Ok(()), "a refused media claim completes the job");
    assert!(
        fetched(&pool, row.id).await.is_none(),
        "the unpublishable row is dropped"
    );
    assert_eq!(post_count(&pool).await, 0, "no status was written");
    let untouched = media_attachment::get_by_id(&pool, pic.id)
        .await
        .expect("the media reads")
        .expect("the fixture upload is stored");
    assert!(
        untouched.status_id.is_none(),
        "the other schedule's reservation still holds the upload"
    );
}
