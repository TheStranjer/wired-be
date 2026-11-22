//! Tombstones, cascades, refused writes and the activity-id lookups.

use super::*;

#[tokio::test]
async fn tombstones_drop_out_of_pages_and_deletes_cascade() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let liked = post_by(&pool, "bob").await;
    let kept = post_by(&pool, "carol").await;
    favourite_as(&pool, &alice, liked).await;
    favourite_as(&pool, &alice, kept).await;

    // A soft delete leaves the row (and the like) in place: the like
    // survives, the count stays, and only the page read drops the
    // tombstone — while the visibility gate is the endpoint's step.
    let tombstone = soft_delete(&pool, liked)
        .await
        .expect("the delete runs")
        .expect("the status was live");
    assert!(tombstone.deleted_at.is_some(), "the row is a tombstone");
    assert_eq!(
        count(&pool, liked).await,
        1,
        "soft_delete counts nothing back"
    );
    let pairs = page_for_user(&pool, alice.id, page(""))
        .await
        .expect("the page reads");
    assert_eq!(pairs.len(), 1, "the tombstoned like is off the page");
    assert_eq!(pairs[0].1.id, kept, "and the kept one stays on it");

    // A hard delete takes the like rows with it through the cascade.
    let erased = sqlx::query("DELETE FROM statuses WHERE id = $1")
        .bind(kept)
        .execute(&pool)
        .await
        .expect("the fixture status deletes")
        .rows_affected();
    assert_eq!(erased, 1, "the kept post was there to delete");
    let remaining = stored(&pool).await;
    assert_eq!(
        remaining
            .iter()
            .map(|(_, _, status_id)| *status_id)
            .collect::<Vec<Uuid>>(),
        vec![liked],
        "only the tombstone's like remains — a hard-deleted status \
             takes its likes with it"
    );
}

#[tokio::test]
async fn deleting_a_user_cascades_their_favourites() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let status_id = post_by(&pool, "bob").await;
    favourite_as(&pool, &alice, status_id).await;

    let erased = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(alice.id)
        .execute(&pool)
        .await
        .expect("the fixture user deletes")
        .rows_affected();
    assert_eq!(erased, 1, "the favouriter was there to delete");
    assert!(
        stored(&pool).await.is_empty(),
        "and the cascade emptied the table"
    );
}

#[tokio::test]
async fn an_unknown_status_refuses_the_write() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;

    let err = favourite(&pool, BASE, alice.id, Uuid::now_v7())
        .await
        .expect_err("a status that does not exist cannot be favourited");
    assert!(
        matches!(err, AppError::Database(_)),
        "the foreign key refuses, not a silent no-op: {err}"
    );
}

/// The mirror's write: [`favourite_like`] stores the delivered activity
/// id as the row's own and steps the counter, and a pair that already
/// liked — under any id — keeps its first row, its first id and the
/// first count.
#[tokio::test]
async fn favourite_like_stands_under_the_delivered_activity_id() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let status_id = post_by(&pool, "bob").await;

    let created = favourite_like(
        &pool,
        alice.id,
        status_id,
        "https://peer.example/activities/like-1",
    )
    .await
    .expect("the mirror write stores")
    .expect("the pair had nothing to keep");
    assert_eq!(
        created.ap_id.as_deref(),
        Some("https://peer.example/activities/like-1"),
        "the row carries the delivered id, not a minted one"
    );
    assert_eq!(count(&pool, status_id).await, 1, "and the counter stepped");

    let restated = favourite_like(
        &pool,
        alice.id,
        status_id,
        "https://peer.example/activities/like-2",
    )
    .await
    .expect("restating a like is not an error");
    assert!(restated.is_none(), "the pair keeps its one row");
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT ap_id FROM favourites WHERE user_id = $1 AND status_id = $2",
        )
        .bind(alice.id)
        .bind(status_id)
        .fetch_one(&pool)
        .await
        .expect("the id reads"),
        Some("https://peer.example/activities/like-1".to_owned()),
        "the first delivery's id stands"
    );
    assert_eq!(count(&pool, status_id).await, 1, "and the count stayed one");
}

/// The two new reads beside the write: [`get`] finds the pair's row
/// only once it exists, and [`get_by_ap_id`] finds it by the
/// `{BASE_URL}/activities/{uuid}` id [`favourite`] minted — and finds
/// nothing for another account quoting the same id, which is what
/// keeps a C2S `Undo` inside its maker's facts.
#[tokio::test]
async fn get_reads_the_pair_and_get_by_ap_id_the_activity_id() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let carol = user(&pool, "carol").await;
    let status_id = post_by(&pool, "bob").await;

    assert!(
        get(&pool, alice.id, status_id)
            .await
            .expect("the pair read runs")
            .is_none(),
        "no like yet, no row"
    );

    let created = favourite(&pool, BASE, alice.id, status_id)
        .await
        .expect("the favourite stores")
        .expect("the pair had not favourited yet");
    let found = get(&pool, alice.id, status_id)
        .await
        .expect("the pair read runs")
        .expect("the write left a row");
    assert_eq!(found.id, created.id, "the pair read finds the row");

    let ap_id = created.ap_id.expect("a local like carries an activity id");
    assert!(ap_id.starts_with(&format!("{BASE}/activities/")));
    let by_id = get_by_ap_id(&pool, alice.id, &ap_id)
        .await
        .expect("the activity-id read runs")
        .expect("the id names the row");
    assert_eq!(by_id.id, created.id, "the id read finds the same row");
    assert!(
        get_by_ap_id(&pool, carol.id, &ap_id)
            .await
            .expect("the activity-id read runs")
            .is_none(),
        "and another account quoting it names none of its own"
    );
}
