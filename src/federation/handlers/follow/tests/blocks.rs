//! Inbound `Follow`s and blocks: only a target that blocks the follower
//! refuses, and a follower's own block of the target is lifted by the Follow.

use super::{
    refusals::{answerable_mirror, answering_local, delivery_args},
    *,
};
use crate::federation::handlers::misc::handle_undo_block;

/// The `Follow` named `n` from `peer.example`'s `nickname` of `object`.
fn follow_of(n: &str, nickname: &str, object: &str) -> ActivityView {
    view(
        "Follow",
        n,
        &format!("https://peer.example/users/{nickname}"),
        &json!(object),
    )
}

/// A remote follower who had blocked the local target, with no `Undo` of
/// the block: the Follow is accepted, stored, and the follower's block row
/// goes with it.
#[tokio::test]
async fn a_follow_from_an_account_that_blocked_the_target_is_accepted_and_lifts_the_block() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = answerable_mirror(&pool, "blk-alice").await;
    let alice_id = alice.id;
    let bob = answering_local(&pool, "blk-bob").await;
    let _row = block::block(&pool, TEST_BASE_URL, &alice, &bob)
        .await
        .expect("the remote follower's block stores");
    let follow_view = follow_of("blk-1", "blk-alice", &bob.ap_id);

    let outcome = handle_follow(state, follow_view, alice).await;

    assert!(matches!(outcome, Ok(Outcome::Handled)), "{outcome:?}");
    let stored = follow::get(&pool, alice_id, bob.id)
        .await
        .expect("the pair reads")
        .expect("the follow is stored");
    assert_eq!(stored.state, FollowState::Accepted, "and granted");
    assert!(
        !block::is_blocking(&pool, alice_id, bob.id).await.unwrap(),
        "the follower's block row is gone"
    );
    let jobs = delivery_args(&pool).await;
    assert_eq!(jobs.len(), 1, "one answer is queued");
    assert_eq!(jobs[0]["body"]["type"], "Accept");
}

/// A local target that blocks the remote follower still refuses: a
/// `Reject` is queued, nothing is stored, and the block stands.
#[tokio::test]
async fn a_follow_from_an_account_the_target_blocks_is_rejected() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = answerable_mirror(&pool, "blk-rej-alice").await;
    let alice_id = alice.id;
    let bob = answering_local(&pool, "blk-rej-bob").await;
    let _row = block::block(&pool, TEST_BASE_URL, &bob, &alice)
        .await
        .expect("the local target's block stores");
    let follow_view = follow_of("blk-2", "blk-rej-alice", &bob.ap_id);

    let outcome = handle_follow(state, follow_view, alice).await;

    assert!(matches!(outcome, Ok(Outcome::Handled)), "{outcome:?}");
    assert!(
        follow::get(&pool, alice_id, bob.id)
            .await
            .expect("the pair reads")
            .is_none(),
        "nothing was stored"
    );
    assert!(
        block::is_blocking(&pool, bob.id, alice_id).await.unwrap(),
        "the target's block stands"
    );
    let jobs = delivery_args(&pool).await;
    assert_eq!(jobs.len(), 1, "one answer is queued");
    assert_eq!(jobs[0]["body"]["type"], "Reject");
}

/// A pair that blocks each other is refused by the target's block, and the
/// follower's own block row is left alone.
#[tokio::test]
async fn a_mutually_blocked_pair_is_rejected_and_keeps_the_followers_block() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = answerable_mirror(&pool, "blk-mut-alice").await;
    let alice_id = alice.id;
    let bob = answering_local(&pool, "blk-mut-bob").await;
    let _theirs = block::block(&pool, TEST_BASE_URL, &alice, &bob)
        .await
        .expect("the follower's block stores");
    let _ours = block::block(&pool, TEST_BASE_URL, &bob, &alice)
        .await
        .expect("the target's block stores");
    let follow_view = follow_of("blk-3", "blk-mut-alice", &bob.ap_id);

    let outcome = handle_follow(state, follow_view, alice).await;

    assert!(matches!(outcome, Ok(Outcome::Handled)), "{outcome:?}");
    assert!(
        follow::get(&pool, alice_id, bob.id)
            .await
            .expect("the pair reads")
            .is_none(),
        "nothing was stored"
    );
    assert!(
        block::is_blocking(&pool, alice_id, bob.id).await.unwrap(),
        "the refused follower's block row is untouched"
    );
    let jobs = delivery_args(&pool).await;
    assert_eq!(jobs[0]["body"]["type"], "Reject");
}

/// The Follow lands before the `Undo` of the Block: the follow stands and
/// the late `Undo` finds no block, so it is ignored.
#[tokio::test]
async fn a_follow_then_the_undo_of_the_block_leaves_the_follow_standing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = answerable_mirror(&pool, "blk-undo-alice").await;
    let alice_id = alice.id;
    let bob = answering_local(&pool, "blk-undo-bob").await;
    let _row = block::block(&pool, TEST_BASE_URL, &alice, &bob)
        .await
        .expect("the remote follower's block stores");
    let follow_view = follow_of("blk-4", "blk-undo-alice", &bob.ap_id);
    let followed = handle_follow(state.clone(), follow_view, alice).await;
    assert!(matches!(followed, Ok(Outcome::Handled)), "{followed:?}");

    let undo = view(
        "Undo",
        "blk-undo-1",
        "https://peer.example/users/blk-undo-alice",
        &json!({
            "type": "Block",
            "actor": "https://peer.example/users/blk-undo-alice",
            "object": bob.ap_id.clone(),
        }),
    );
    let undone = handle_undo_block(state, undo, fresh(&pool, alice_id).await).await;

    assert!(matches!(undone, Ok(Outcome::Ignored(_))), "{undone:?}");
    let stored = follow::get(&pool, alice_id, bob.id)
        .await
        .expect("the pair reads")
        .expect("the follow still stands");
    assert_eq!(stored.state, FollowState::Accepted);
}
