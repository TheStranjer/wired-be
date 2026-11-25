//! Following a relay, its shared fan-out, its announces and the unfollow.

use super::*;

/// Scenario: this instance follows the peer's relay over the real fetch and the
/// real delivery, and the relay's `Accept` — posted back through `POST /inbox`
/// — turns both of its rows accepted.
#[tokio::test]
async fn following_a_relay_runs_the_handshake_over_http() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let instance = instance_actor(&state).await;

    let subscription = follow_relay(&state, &peer.relay())
        .await
        .expect("the relay follow runs");
    assert_eq!(
        subscription.actor_ap_id,
        peer.relay(),
        "the subscription is keyed by the fetched relay actor"
    );
    assert_eq!(
        subscription.inbox,
        peer.shared_inbox(),
        "and carries the inbox that actor's document named"
    );
    let _drained = common::run_all_jobs(&state).await;

    let follows = peer.delivered("Follow");
    assert_eq!(follows.len(), 1, "the relay's inbox recorded the Follow");
    assert_outbound(&follows[0], &instance, remote::INBOX, "Follow");
    let body = follows[0].activity();
    let follow_id = body["id"].as_str().expect("the Follow carries its id");
    assert_eq!(
        body["object"],
        json!(peer.relay()),
        "the relay actor is its object"
    );
    assert_eq!(
        body["to"],
        json!([peer.shared_inbox()]),
        "and the relay's inbox is its only address"
    );

    let accept = peer.activity(
        "Accept",
        "ef-relay-accept",
        &peer.relay(),
        &json!(&follow_id),
    );
    peer_delivers(&app, &state, &peer, &peer.relay(), &accept).await;

    let mirror = relay_mirror(&state, &peer).await;
    assert_granted_relay(&pool, &mirror, &instance, &peer.relay()).await;
}

/// Scenario: with the relay accepted and the mock following a local account,
/// one public post fans out to a single inbox — the relay's address is the same
/// shared endpoint the follower expands to, so the per-inbox dedup keeps one
/// delivery — and the `Create` the peer records is this instance's own signed
/// document.
#[tokio::test]
async fn an_accepted_relay_shares_one_fan_out_with_the_followers_inbox() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ef-fanout-alice").await;
    let _subscription = relay_accepted(&app, &state, &peer).await;
    let _follow = peer_follows(&app, &state, &peer, &alice, "ef-fanout-follow").await;

    let posted = post_public(&app, &alice_token, "for my follower and the relay").await;
    let _drained = common::run_all_jobs(&state).await;

    let creates = peer.delivered("Create");
    assert_eq!(
        creates.len(),
        1,
        "the relay's inbox and the follower's shared inbox are one URL, so one delivery goes out"
    );
    assert_outbound(&creates[0], &alice, remote::INBOX, "Create");
    assert_eq!(
        creates[0].activity()["object"]["id"],
        json!(posted["uri"]),
        "the Create carries the status that was just posted"
    );
    assert_eq!(
        peer.delivered("Announce").len(),
        0,
        "the fan-out sends the Create, never a boost of it"
    );
}

/// Scenario: the accepted relay re-broadcasts a note of the peer's own. It
/// arrives as an ordinary public remote status on the public timeline — not as
/// a boost, and with nothing added to the relay's own post count.
#[tokio::test]
async fn a_relay_announce_lands_in_the_public_timeline_without_a_boost() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let _subscription = relay_accepted(&app, &state, &peer).await;
    let relay = relay_mirror(&state, &peer).await;

    let announce = json!({
        "id": peer.activity_id("ef-relay-announce"),
        "type": "Announce",
        "actor": peer.relay(),
        "object": peer.note("relayed"),
        "to": [remote::PUBLIC],
    });
    peer_delivers(&app, &state, &peer, &peer.relay(), &announce).await;

    let entry = timeline_entry(&app, &peer.note("relayed"))
        .await
        .expect("the relayed note was fetched and ingested");
    assert_eq!(
        entry["content"],
        json!("<p>a note relayed as relayed</p>"),
        "as the announced object's own content"
    );
    assert_eq!(
        entry["reblog"],
        Value::Null,
        "with no boost wrapped around it"
    );
    assert_eq!(
        entry["account"]["acct"],
        json!(peer.handle("author")),
        "and authored by the note's own account, not by the relay"
    );
    assert_eq!(
        account(&app, None, &relay.id.to_string()).await["statuses_count"],
        json!(0),
        "the relay holds no status: its Announce wrote no reblog row"
    );
}

/// Scenario: withdrawing the relay sends the `Undo` of the very `Follow` this
/// instance posted to its inbox, closes the fan-out, and clears the
/// subscription.
#[tokio::test]
async fn unfollowing_a_relay_sends_the_undo_and_stops_the_fan_out() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let instance = instance_actor(&state).await;
    let (_alice, alice_token) = local(&pool, "ef-relay-undo-alice").await;
    let follow_id = relay_accepted(&app, &state, &peer).await;

    let _first_post = post_public(&app, &alice_token, "one for the accepted relay").await;
    let _fan_out = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.delivered("Create").len(),
        1,
        "the accepted relay rides the fan-out"
    );

    unfollow_relay(&state, &peer.relay())
        .await
        .expect("the withdrawal runs");
    let _withdrawal = common::run_all_jobs(&state).await;

    let undos = peer.delivered("Undo");
    assert_eq!(undos.len(), 1, "the relay's inbox recorded the Undo");
    assert_outbound(&undos[0], &instance, remote::INBOX, "Undo");
    // Withdrawing the very Follow this instance sent, as its actor made it.
    let undo = undos[0].activity();
    assert_flags(
        &undo["object"],
        &[
            ("id", json!(follow_id)),
            ("type", json!("Follow")),
            ("actor", json!(instance.ap_id)),
        ],
    );

    let _second_post = post_public(&app, &alice_token, "the relay no longer subscribes").await;
    let _quiet = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.delivered("Create").len(),
        1,
        "and the fan-out no longer names the relay"
    );
    let ledger = relay::get(&pool, &peer.relay()).await;
    assert!(
        ledger.expect("the ledger reads").is_none(),
        "the subscription is cleared"
    );
}
