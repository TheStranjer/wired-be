//! Tests that a live featured `Add`/`Remove` records itself for the syncs
//! (WBE-881) and still applies at once.

use serde_json::{Value, json};

use super::{
    handle_add, handle_remove,
    test_support::{
        ALICE, FEATURED, app_state, assert_handled, featured_signer, fresh_user, remote_post, view,
    },
};
use crate::models::{
    featured_live::{self, Collection},
    featured_tag, pinned_status,
};

/// Mastodon's id-less `Hashtag` object.
fn hashtag(name: &str) -> Value {
    json!({
        "type": "Hashtag",
        "href": format!("https://peer.example/tags/{name}"),
        "name": format!("#{name}"),
    })
}

/// A live pin `Add` and `Remove` apply immediately and mark the pins, not the
/// tags.
#[tokio::test]
async fn a_live_pin_change_applies_and_marks_only_the_pins() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = featured_signer(&pool).await;
    let alice_id = alice.id;
    let posted = remote_post(&pool, &alice, "live").await;
    assert!(!featured_live::is_recent(&pool, alice_id, Collection::Pins).await);

    let added = handle_add(
        state.clone(),
        view(
            "Add",
            "live-add",
            ALICE,
            json!({"object": posted.uri.clone(), "target": FEATURED}),
        ),
        alice,
    )
    .await;
    assert_handled(&added);
    assert_eq!(pinned_status::count(&pool, alice_id).await.unwrap(), 1);
    assert!(featured_live::is_recent(&pool, alice_id, Collection::Pins).await);
    assert!(!featured_live::is_recent(&pool, alice_id, Collection::Tags).await);

    let removed = handle_remove(
        state,
        view(
            "Remove",
            "live-remove",
            ALICE,
            json!({"object": posted.uri.clone(), "target": FEATURED}),
        ),
        fresh_user(&pool, alice_id).await,
    )
    .await;
    assert_handled(&removed);
    assert_eq!(pinned_status::count(&pool, alice_id).await.unwrap(), 0);
}

/// A live id-less `Hashtag` `Add` and `Remove` apply immediately and mark the
/// tags, not the pins.
#[tokio::test]
async fn a_live_tag_change_applies_and_marks_only_the_tags() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = featured_signer(&pool).await;
    let alice_id = alice.id;

    let added = handle_add(
        state.clone(),
        view(
            "Add",
            "live-tag-add",
            ALICE,
            json!({"object": hashtag("R6Race"), "target": FEATURED}),
        ),
        alice,
    )
    .await;
    assert_handled(&added);
    assert_eq!(featured_tag::list(&pool, alice_id).await.unwrap().len(), 1);
    assert!(featured_live::is_recent(&pool, alice_id, Collection::Tags).await);
    assert!(!featured_live::is_recent(&pool, alice_id, Collection::Pins).await);

    let removed = handle_remove(
        state,
        view(
            "Remove",
            "live-tag-remove",
            ALICE,
            json!({"object": hashtag("R6Race"), "target": FEATURED}),
        ),
        fresh_user(&pool, alice_id).await,
    )
    .await;
    assert_handled(&removed);
    let remaining: Vec<String> = featured_tag::list(&pool, alice_id)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(remaining, Vec::<String>::new());
}

/// An `Add` aimed at some other collection marks nothing.
#[tokio::test]
async fn an_add_for_another_collection_marks_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = featured_signer(&pool).await;
    let alice_id = alice.id;

    let _outcome = handle_add(
        state,
        view(
            "Add",
            "other-add",
            ALICE,
            json!({"object": hashtag("x"), "target": "https://peer.example/users/alice/other"}),
        ),
        alice,
    )
    .await;

    assert!(!featured_live::is_recent(&pool, alice_id, Collection::Pins).await);
    assert!(!featured_live::is_recent(&pool, alice_id, Collection::Tags).await);
}
