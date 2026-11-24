//! The publish job's refusal of authors whose account can no longer post:
//! deleted, deactivated and approval-pending accounts.

use super::*;

/// Asserts that publishing `row` drops it and writes no status for `author`.
async fn assert_dropped_unpublished(pool: &PgPool, author: &User, row: &ScheduledStatus) {
    let outcome = publish_one(&app_state(pool), row.id).await;
    assert_eq!(outcome, Ok(true), "a barred author's schedule is dropped");
    assert!(
        fetched(pool, row.id).await.is_none(),
        "the schedule is gone"
    );
    assert!(
        posts_of(pool, author).await.is_empty(),
        "no status was created for the barred author"
    );
}

#[tokio::test]
async fn a_deleted_author_drops_the_row_through_the_handler() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let row = parked(
        &pool,
        &alice,
        Utc::now() - TimeDelta::hours(1),
        json!({ "status": "from beyond" }),
    )
    .await;
    user::mark_deleted(&pool, alice.id)
        .await
        .expect("the account is marked deleted");

    let outcome = handle_publish(app_state(&pool), FIRST_ATTEMPT, job_args(row.id)).await;

    assert_eq!(outcome, Ok(()), "a deleted author's fire completes");
    assert!(fetched(&pool, row.id).await.is_none(), "the row is gone");
    assert!(
        posts_of(&pool, &alice).await.is_empty(),
        "nothing is posted for a deleted account"
    );
}

#[tokio::test]
async fn a_deactivated_author_drops_the_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let row = parked(
        &pool,
        &alice,
        Utc::now() - TimeDelta::hours(1),
        json!({ "status": "suspended" }),
    )
    .await;
    user::set_activation(&pool, alice.id, false)
        .await
        .expect("the account deactivates");

    assert_dropped_unpublished(&pool, &alice, &row).await;
}

#[tokio::test]
async fn an_approval_pending_author_drops_the_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let row = parked(
        &pool,
        &alice,
        Utc::now() - TimeDelta::hours(1),
        json!({ "status": "not yet approved" }),
    )
    .await;
    sqlx::query("UPDATE users SET approval_pending = TRUE WHERE id = $1")
        .bind(alice.id)
        .execute(&pool)
        .await
        .map(drop)
        .expect("the approval flag is set");

    assert_dropped_unpublished(&pool, &alice, &row).await;
}

#[tokio::test]
async fn the_sweep_drops_a_barred_authors_row_and_publishes_an_active_ones() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    let now = Utc::now();
    let barred = parked(
        &pool,
        &alice,
        now - TimeDelta::hours(2),
        json!({ "status": "from the suspended" }),
    )
    .await;
    let live = parked(
        &pool,
        &bob,
        now - TimeDelta::hours(1),
        json!({ "status": "from the active" }),
    )
    .await;
    user::set_activation(&pool, alice.id, false)
        .await
        .expect("the account deactivates");

    let outcome = handle_sweep(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "the sweep completes");
    assert!(
        fetched(&pool, barred.id).await.is_none(),
        "the barred row is dropped"
    );
    assert!(
        fetched(&pool, live.id).await.is_none(),
        "the active row is fired"
    );
    assert!(
        posts_of(&pool, &alice).await.is_empty(),
        "nothing posted for the deactivated author"
    );
    let posts = posts_of(&pool, &bob).await;
    assert_eq!(posts.len(), 1, "the active author's post was published");
    assert_eq!(posts[0].raw_text, "from the active", "with its own text");
}
