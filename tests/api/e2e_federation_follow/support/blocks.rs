//! Local and remote blocks and the undo of a block.

use super::*;

/// Scenario: a local account blocks the mock. The follow between them is
/// severed here, the relationship reads `blocking`, and the peer's inbox holds
/// the signed `Block`.
#[tokio::test]
async fn blocking_a_remote_account_severs_the_follow_and_publishes_the_block() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ef-block-alice").await;
    let _follow = peer_follows(&app, &state, &peer, &alice, "ef-block-follow").await;
    let bob = account_id(&app, &peer.handle(BOB)).await;

    let relation = account_action(&app, &alice_token, &bob, "block").await;
    let _drained = common::run_all_jobs(&state).await;

    assert_eq!(
        relation["blocking"],
        json!(true),
        "the write answers the caller's own block"
    );
    // And the follower row is gone with the block.
    let after = relationship(&app, &alice_token, &bob).await;
    assert_flags(
        &after,
        &[("blocking", json!(true)), ("followed_by", json!(false))],
    );
    assert_eq!(
        counter(&app, &alice_token, &alice.id.to_string(), "followers_count").await,
        json!(0),
        "the counter that follow fed stepped back"
    );

    let blocks = peer.delivered("Block");
    assert_eq!(blocks.len(), 1, "the peer was told of the block");
    assert_outbound(
        &blocks[0],
        &alice,
        &peer.inbox_path(&peer.actor(BOB)),
        "Block",
    );
    assert_eq!(
        blocks[0].activity()["object"],
        json!(peer.actor(BOB)),
        "naming the blocked account as its object"
    );
}

/// Scenario: the mock blocks a local account. Her relationship read says
/// `blocked_by` — with nothing about domains behind it — his follow of her is
/// severed, and from that moment her public posts stop reaching him.
#[tokio::test]
async fn a_remote_block_hides_the_local_author_from_the_blocker() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ef-blocked-alice").await;
    let _follow = peer_follows(&app, &state, &peer, &alice, "ef-blocked-follow").await;
    let _before = post_public(&app, &alice_token, "seen while he only followed her").await;
    let _delivered_before = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.delivered("Create").len(),
        1,
        "a follower on the shared inbox receives her post"
    );

    let block = peer.activity(
        "Block",
        "ef-blocked-block",
        &peer.actor(BOB),
        &json!(alice.ap_id),
    );
    peer_delivers(&app, &state, &peer, &peer.actor(BOB), &block).await;

    // His block reads as `blocked_by`, with no follow and no domain rule
    // beside it.
    let bob = account_id(&app, &peer.handle(BOB)).await;
    let relation = relationship(&app, &alice_token, &bob).await;
    assert_flags(
        &relation,
        &[
            ("blocked_by", json!(true)),
            ("blocking", json!(false)),
            ("domain_blocking", json!(false)),
            ("followed_by", json!(false)),
        ],
    );
    assert_eq!(
        lookup(&app, &peer.handle(BOB)).await["following_count"],
        json!(0),
        "and his own follow of her went with the block"
    );

    let _after = post_public(&app, &alice_token, "posted after he blocked her").await;
    let _delivered_after = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.delivered("Create").len(),
        1,
        "the blocked pair carries nothing further to him"
    );
}

/// Scenario: the mock lifts the block it sent. `blocked_by` is gone from the
/// relationship read, and the very next `Follow` from him grants.
#[tokio::test]
async fn an_undo_of_a_remote_block_lifts_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ef-unblock-alice").await;

    let block = peer.activity(
        "Block",
        "ef-unblock-block",
        &peer.actor(BOB),
        &json!(alice.ap_id),
    );
    peer_delivers(&app, &state, &peer, &peer.actor(BOB), &block).await;
    let bob = account_id(&app, &peer.handle(BOB)).await;
    assert_eq!(
        relationship(&app, &alice_token, &bob).await["blocked_by"],
        json!(true),
        "the block stands while it is unrevoked"
    );

    let undo = peer.activity("Undo", "ef-unblock-undo", &peer.actor(BOB), &block["id"]);
    peer_delivers(&app, &state, &peer, &peer.actor(BOB), &undo).await;

    // The Undo lifted the block in both readings.
    let relation = relationship(&app, &alice_token, &bob).await;
    assert_flags(
        &relation,
        &[("blocked_by", json!(false)), ("blocking", json!(false))],
    );

    let _follow = peer_follows(&app, &state, &peer, &alice, "ef-unblock-follow").await;
    assert_eq!(
        relationship(&app, &alice_token, &bob).await["followed_by"],
        json!(true),
        "and the pair holds a follow again"
    );
}
