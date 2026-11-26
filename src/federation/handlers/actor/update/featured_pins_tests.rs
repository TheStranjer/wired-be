//! Tests of the featured-pins refresh an actor `Update` runs.

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::super::{
    test_support::{
        ALICE, app_state, assert_handled, mirror, mirror_post, person, ready_for_update, view,
    },
    update_actor,
};
use crate::models::pinned_status;

/// alice's document with an inline `featured` collection listing `uris`.
fn document_featuring(uris: &[&str]) -> Value {
    let mut document = person();
    document["featured"] = json!({
        "type": "OrderedCollection",
        "orderedItems": uris,
    });
    document
}

/// The ids of the statuses `user_id` pins, newest pin first.
async fn pinned_ids(pool: &PgPool, user_id: Uuid) -> Vec<Uuid> {
    pinned_status::list_for_user(pool, user_id)
        .await
        .expect("the pins read")
        .iter()
        .map(|status| status.id)
        .collect()
}

/// An `Update` whose document lists the actor's own statuses in an inline
/// `featured` collection pins exactly those.
#[tokio::test]
async fn an_update_pins_the_statuses_the_featured_collection_lists() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = mirror(&pool).await;
    let alice_id = alice.id;
    let first = mirror_post(&pool, &alice, "first").await;
    let second = mirror_post(&pool, &alice, "second").await;

    let document = document_featuring(&[&second.uri, &first.uri]);
    let folded = update_actor(state, view("Update", "pins-1", ALICE, document), alice).await;

    assert_handled(&folded);
    assert_eq!(
        pinned_ids(&pool, alice_id).await,
        [second.id, first.id],
        "both statuses are pinned, in the collection's order"
    );
}

/// A later `Update` that drops a status unpins it.
#[tokio::test]
async fn an_update_that_drops_a_status_unpins_it() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = mirror(&pool).await;
    let alice_id = alice.id;
    let kept = mirror_post(&pool, &alice, "kept").await;
    let dropped = mirror_post(&pool, &alice, "dropped").await;
    for pinned in [&kept, &dropped] {
        let _row = pinned_status::pin(&pool, alice_id, pinned.id)
            .await
            .expect("the fixture pin stores");
    }

    let document = document_featuring(&[&kept.uri]);
    let folded = update_actor(state, view("Update", "pins-2", ALICE, document), alice).await;

    assert_handled(&folded);
    assert_eq!(
        pinned_ids(&pool, alice_id).await,
        [kept.id],
        "the dropped status is unpinned"
    );
}

/// A document that names no featured collection leaves the pins alone.
#[tokio::test]
async fn an_update_without_a_featured_collection_keeps_the_pins() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = mirror(&pool).await;
    let alice_id = alice.id;
    let pinned = mirror_post(&pool, &alice, "pinned").await;
    let _row = pinned_status::pin(&pool, alice_id, pinned.id)
        .await
        .expect("the fixture pin stores");

    let current = ready_for_update(&pool, alice_id).await;
    let folded = update_actor(state, view("Update", "pins-3", ALICE, person()), current).await;

    assert_handled(&folded);
    assert_eq!(pinned_ids(&pool, alice_id).await, [pinned.id]);
}
