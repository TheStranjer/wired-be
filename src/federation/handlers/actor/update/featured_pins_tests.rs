//! Tests of the featured-pins refresh an actor `Update` runs.

use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::super::{
    test_support::{
        ALICE, app_state, assert_handled, mirror, mirror_post, person, ready_for_update, view,
    },
    update_actor,
};
use crate::models::{
    featured_live::{self, Collection, FEATURED_SYNC_GRACE},
    pinned_status,
};

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

/// An `Update` inside the grace of a live pin change keeps the stored pins; a
/// mark back-dated past the grace lets the same `Update` replace them.
#[tokio::test]
async fn an_update_inside_the_grace_of_a_live_pin_change_keeps_the_pins() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = mirror(&pool).await;
    let alice_id = alice.id;
    let live = mirror_post(&pool, &alice, "live").await;
    let cached = mirror_post(&pool, &alice, "cached").await;
    let _row = pinned_status::pin(&pool, alice_id, live.id)
        .await
        .expect("the live pin stores");
    featured_live::mark(&pool, alice_id, Collection::Pins)
        .await
        .expect("the mark stores");

    let document = document_featuring(&[&cached.uri]);
    let folded = update_actor(
        state.clone(),
        view("Update", "pins-4", ALICE, document.clone()),
        alice,
    )
    .await;
    assert_handled(&folded);
    assert_eq!(
        pinned_ids(&pool, alice_id).await,
        [live.id],
        "the cached collection does not undo the live pin"
    );

    featured_live::mark_at(
        &pool,
        alice_id,
        Collection::Pins,
        Utc::now() - FEATURED_SYNC_GRACE - Duration::seconds(1),
    )
    .await
    .expect("the mark back-dates");
    let current = ready_for_update(&pool, alice_id).await;
    let refolded = update_actor(state, view("Update", "pins-5", ALICE, document), current).await;
    assert_handled(&refolded);
    assert_eq!(
        pinned_ids(&pool, alice_id).await,
        [cached.id],
        "past the grace the collection is the truth again"
    );
}

/// A recent live change to the featured tags does not hold the pin sync back.
#[tokio::test]
async fn a_recent_tag_change_does_not_stop_an_update_syncing_pins() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = mirror(&pool).await;
    let alice_id = alice.id;
    let listed = mirror_post(&pool, &alice, "listed").await;
    featured_live::mark(&pool, alice_id, Collection::Tags)
        .await
        .expect("the mark stores");

    let document = document_featuring(&[&listed.uri]);
    let folded = update_actor(state, view("Update", "pins-6", ALICE, document), alice).await;

    assert_handled(&folded);
    assert_eq!(pinned_ids(&pool, alice_id).await, [listed.id]);
}
