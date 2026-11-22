//! The favourite and unfavourite writes: rows, counters and idempotence.

use super::*;

#[tokio::test]
async fn favouriting_writes_the_row_and_bumps_the_counter() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let status_id = post_by(&pool, "bob").await;

    let created = favourite(&pool, BASE, alice.id, status_id)
        .await
        .expect("the first favourite stores")
        .expect("the pair had nothing to keep");

    assert_eq!(created.user_id, alice.id, "who favourited");
    assert_eq!(created.status_id, status_id, "what they favourited");
    let ap_id = created
        .ap_id
        .clone()
        .expect("a local favourite mints an activity id");
    let activity = ap_id
        .strip_prefix(&format!("{BASE}{ACTIVITY_PREFIX}"))
        .unwrap_or_default();
    assert!(
        crate::util::id::parse_id(activity).is_some(),
        "the id is this instance's origin, `/activities/` and a uuid, \
             not {ap_id}"
    );
    assert_eq!(
        stored(&pool).await,
        vec![(created.id, alice.id, status_id)],
        "one row"
    );
    assert_eq!(count(&pool, status_id).await, 1, "and one count");
}

#[tokio::test]
async fn favouriting_twice_is_one_row_and_one_count() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let status_id = post_by(&pool, "bob").await;
    let first = favourite(&pool, BASE, alice.id, status_id)
        .await
        .expect("the first favourite stores")
        .expect("the pair had nothing to keep");

    let again = favourite(&pool, BASE, alice.id, status_id)
        .await
        .expect("repeating a favourite is not an error");

    assert!(again.is_none(), "the repeat created no row and no count");
    assert_eq!(
        stored(&pool).await,
        vec![(first.id, alice.id, status_id)],
        "still one row in the table"
    );
    assert_eq!(count(&pool, status_id).await, 1, "the count stayed one");
}

#[tokio::test]
async fn unfavouriting_removes_the_row_and_steps_the_counter() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let status_id = post_by(&pool, "bob").await;
    favourite_as(&pool, &alice, status_id).await;

    let removed = unfavourite(&pool, alice.id, status_id)
        .await
        .expect("the withdrawal runs")
        .expect("the favourite was there");
    assert_eq!(removed.user_id, alice.id, "the row that went");
    assert_eq!(removed.status_id, status_id, "and its status");
    assert_eq!(count(&pool, status_id).await, 0, "the count steps down");

    assert!(
        unfavourite(&pool, alice.id, status_id)
            .await
            .expect("repeating a withdrawal runs")
            .is_none(),
        "withdrawing what is already gone removes nothing"
    );
    assert!(stored(&pool).await.is_empty(), "the table is empty again");
    assert_eq!(
        count(&pool, status_id).await,
        0,
        "and a no-op steps nothing, never below zero either"
    );
}

#[tokio::test]
async fn favourites_are_per_account_and_per_status() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let carol = user(&pool, "carol").await;
    let first = post(&pool, &carol).await;
    let second = post(&pool, &carol).await;

    favourite_as(&pool, &alice, first.id).await;
    favourite_as(&pool, &bob, first.id).await;
    favourite_as(&pool, &alice, second.id).await;

    let flagged = favourited_status_ids(&pool, alice.id, &[first.id, second.id])
        .await
        .expect("the flags read");
    assert!(flagged.contains(&first.id), "alice liked the first");
    assert!(flagged.contains(&second.id), "and the second");
    assert!(
        !favourited_status_ids(&pool, bob.id, &[first.id, second.id])
            .await
            .expect("bob's flags read")
            .contains(&second.id),
        "but bob only liked the first"
    );
    assert_eq!(
        favourited_status_ids(&pool, alice.id, &[Uuid::now_v7()])
            .await
            .expect("an unliked id reads"),
        HashSet::new(),
        "an id with no row is simply absent"
    );
    assert_eq!(
        count(&pool, first.id).await,
        2,
        "the first post carries both likes"
    );
    assert_eq!(count(&pool, second.id).await, 1, "the second only alice's");
}
