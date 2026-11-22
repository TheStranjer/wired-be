//! Notes, endorsements and domain blocking on the relationship.

use super::test_support::{assert_placeholder_flags, local_user, one_relationship, remote_user};
use super::*;
use crate::{
    config::TEST_BASE_URL,
    models::{
        account_note, domain_block, endorsement,
        follow::{self, FollowState},
    },
};

#[tokio::test]
async fn a_stored_note_rides_the_relationship() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    let carol = local_user(&pool, "carol").await;
    let granted = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
        .await
        .unwrap();
    assert_eq!(granted.state, FollowState::Accepted, "the fixture follows");
    account_note::set(&pool, alice.id, bob.id, "colleague from work")
        .await
        .unwrap();
    account_note::set(&pool, bob.id, alice.id, "who?")
        .await
        .unwrap();

    let relationship = one_relationship(&pool, &alice, &bob).await;
    assert_eq!(
        relationship.note, "colleague from work",
        "the viewer's own note on the target loads"
    );
    assert!(relationship.following.0, "with the follow flags beside it");

    let loaded = Relationship::load_many(&pool, alice.id, &[bob.id, carol.id])
        .await
        .unwrap();
    assert_eq!(loaded.len(), 2, "one answer per requested id");
    assert_eq!(loaded[0].note, "colleague from work", "noted target");
    assert!(loaded[1].note.is_empty(), "unnoted target keeps empty");

    let bobs_view = one_relationship(&pool, &bob, &alice).await;
    assert_eq!(
        bobs_view.note, "who?",
        "and bob sees his own note, not alice's"
    );
}

#[tokio::test]
async fn a_cleared_note_leaves_the_relationship() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    account_note::set(&pool, alice.id, bob.id, "here for a while")
        .await
        .unwrap();
    assert_eq!(
        one_relationship(&pool, &alice, &bob).await.note,
        "here for a while",
        "the note loads while it is stored"
    );

    account_note::set(&pool, alice.id, bob.id, "")
        .await
        .unwrap();
    let relationship = one_relationship(&pool, &alice, &bob).await;
    assert!(
        relationship.note.is_empty(),
        "the empty write deleted the row, and the loader reads the default"
    );
    assert!(
        !relationship.following.0,
        "and nothing else about the pair changed"
    );
}

#[tokio::test]
async fn a_remote_target_on_a_blocked_domain_is_domain_blocking() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote_user(&pool, "bob", "spam.example").await;
    let carol = remote_user(&pool, "carol", "other.example").await;
    domain_block::block_domain(&pool, alice.id, "spam.example")
        .await
        .expect("the fixture block stores");

    let loaded = Relationship::load_many(&pool, alice.id, &[bob.id, carol.id])
        .await
        .expect("the batch reads");
    let flags: Vec<bool> = loaded.iter().map(|row| row.domain_blocking.0).collect();
    assert_eq!(
        flags,
        vec![true, false],
        "the blocked host's remote account is domain-blocking, another host's is not"
    );

    // The flag is the viewer's own: another account that has not
    // blocked anything reads the same target unblocked.
    let dave = local_user(&pool, "dave").await;
    let other_view = one_relationship(&pool, &dave, &bob).await;
    assert!(
        !other_view.domain_blocking.0,
        "dave blocked nothing, so bob's host is open to him"
    );

    domain_block::unblock_domain(&pool, alice.id, "spam.example")
        .await
        .expect("the fixture unblock runs");
    let after = one_relationship(&pool, &alice, &bob).await;
    assert!(!after.domain_blocking.0, "lifting the block lifts the flag");
}

#[tokio::test]
async fn local_accounts_are_never_domain_blocking() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    // Block this very instance's domain: even a "blocked" host cannot
    // make a local target domain-blocking (the join's `NOT local`).
    domain_block::block_domain(&pool, alice.id, "test.example")
        .await
        .expect("the fixture block stores");

    let relationship = one_relationship(&pool, &alice, &bob).await;
    assert!(
        !relationship.domain_blocking.0,
        "the local target of a local viewer stands unblocked"
    );
    assert_placeholder_flags(&relationship);
}

#[tokio::test]
async fn an_endorsement_lights_the_endorsed_flag() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    let carol = local_user(&pool, "carol").await;
    let granted = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
        .await
        .unwrap();
    assert_eq!(granted.state, FollowState::Accepted, "the fixture follows");
    endorsement::endorse(&pool, alice.id, bob.id)
        .await
        .expect("the fixture endorsement stores");

    let relationship = one_relationship(&pool, &alice, &bob).await;
    assert!(
        relationship.endorsed.0,
        "a target the viewer pinned is endorsed"
    );
    assert!(relationship.following.0, "with the follow flags beside it");

    // The flag is the viewer's own: carol, who pinned nothing, reads
    // the same target unendorsed, and within alice's batch only the
    // pinned target lights up.
    let other_view = one_relationship(&pool, &carol, &bob).await;
    assert!(!other_view.endorsed.0, "carol endorsed nothing");
    let loaded = Relationship::load_many(&pool, alice.id, &[bob.id, carol.id])
        .await
        .unwrap();
    let flags: Vec<bool> = loaded.iter().map(|row| row.endorsed.0).collect();
    assert_eq!(flags, vec![true, false], "pinned, then not");

    endorsement::unendorse(&pool, alice.id, bob.id)
        .await
        .expect("the fixture withdrawal runs");
    let after = one_relationship(&pool, &alice, &bob).await;
    assert!(
        !after.endorsed.0,
        "withdrawing the pin falls back to the default"
    );
    assert!(after.following.0, "and nothing else about the pair changed");
}
