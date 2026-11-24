//! [`Relationship::load_one`] and [`Relationship::load_map`].

use sqlx::{PgPool, postgres::PgPoolOptions};

use super::test_support::local_user;
use super::*;
use crate::{
    config::TEST_BASE_URL,
    models::follow::{self, FollowState},
};

#[tokio::test]
async fn load_one_answers_the_defaults_for_a_stranger() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;

    let relationship = Relationship::load_one(&pool, alice.id, bob.id)
        .await
        .unwrap();
    assert_eq!(relationship.id, bob.id.to_string(), "about the target");
    assert!(!relationship.following.0, "a stranger is not followed");
    assert_same_as_batch(&pool, &alice, &bob, &relationship).await;
}

/// Asserts `one` serializes the same as the batch loader's row for the pair.
async fn assert_same_as_batch(pool: &PgPool, viewer: &User, target: &User, one: &Relationship) {
    let many = Relationship::load_many(pool, viewer.id, &[target.id])
        .await
        .unwrap();
    assert_eq!(many.len(), 1, "one target in answers one row out");
    assert_eq!(
        serde_json::to_value(many.first().expect("one row")).unwrap(),
        serde_json::to_value(one).unwrap(),
        "load_one answers the batch's row"
    );
}

#[tokio::test]
async fn load_one_answers_the_flags_for_a_followed_account() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    let granted = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
        .await
        .unwrap();
    assert_eq!(
        granted.state,
        FollowState::Accepted,
        "an open account accepts"
    );

    let relationship = Relationship::load_one(&pool, alice.id, bob.id)
        .await
        .unwrap();
    assert!(relationship.following.0, "the accepted follow shows");
    assert_same_as_batch(&pool, &alice, &bob, &relationship).await;
}

#[tokio::test]
async fn load_map_is_none_for_an_anonymous_viewer_without_a_query() {
    // A lazy pool to an address nothing listens on: any query would fail.
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://nobody@127.0.0.1:1/none")
        .expect("a lazy pool never connects");
    let map = Relationship::load_map(&pool, None, &[Uuid::nil()])
        .await
        .unwrap();
    assert!(map.is_none(), "anonymous viewers get no map");
}

#[tokio::test]
async fn load_map_keys_every_target() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    let carol = local_user(&pool, "carol").await;
    let granted = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
        .await
        .unwrap();
    assert_eq!(
        granted.state,
        FollowState::Accepted,
        "an open account accepts"
    );

    let map = Relationship::load_map(&pool, Some(&alice), &[bob.id, carol.id])
        .await
        .unwrap()
        .expect("a signed-in viewer gets a map");
    assert_eq!(map.len(), 2, "both targets are keyed");
    let for_bob = map.get(&bob.id).expect("bob is keyed");
    let for_carol = map.get(&carol.id).expect("carol is keyed");
    assert!(for_bob.following.0, "bob is followed");
    assert!(!for_carol.following.0, "carol is not");
    assert_eq!(for_carol.id, carol.id.to_string(), "keyed by target");
}
