//! Remote and local follows, locked accounts, unfollows and remote undos.

use super::*;

// --- the scenarios ------------------------------------------------------------------------

/// Scenario: the mock follows a local account. The fold grants on the spot,
/// every client-API read agrees she has a follower, and the peer's inbox holds
/// the one signed `Accept` that answers the `Follow`.
#[tokio::test]
async fn a_remote_follow_grants_locally_and_is_answered_with_a_signed_accept() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ef-in-alice").await;

    let follow = peer_follows(&app, &state, &peer, &alice, "ef-in-follow").await;

    let subject = lookup(&app, &format!("{}@{DOMAIN}", alice.nickname)).await;
    assert_eq!(
        subject["id"],
        json!(alice.id.to_string()),
        "the handle the mock's view of alice carries, `alice@test.example`, is decision 7's spelling of her"
    );
    assert_eq!(
        subject["followers_count"],
        json!(1),
        "and the entity already counts her follower"
    );
    // The fetched follower, as both APIs render the mirror the fold wrote.
    let mirror = lookup(&app, &peer.handle(BOB)).await;
    assert_flags(
        &mirror,
        &[
            ("acct", json!(peer.handle(BOB))),
            ("username", json!(BOB)),
            ("following_count", json!(1)),
            ("followers_count", json!(0)),
        ],
    );
    assert_eq!(
        counter(&app, &alice_token, &alice.id.to_string(), "followers_count").await,
        json!(1),
        "the profile read says the same"
    );
    assert!(
        follow_requests(&app, &alice_token).await.is_empty(),
        "an open account answers on the spot, so nothing waits in the queue"
    );
    // The viewer's own flags: a follower, not a request of hers.
    let bob = mirror["id"].as_str().expect("the mirror carries its id");
    let relation = relationship(&app, &alice_token, bob).await;
    assert_flags(
        &relation,
        &[
            ("followed_by", json!(true)),
            ("following", json!(false)),
            ("requested", json!(false)),
        ],
    );

    let accepts = peer.delivered("Accept");
    assert_eq!(accepts.len(), 1, "the grant is answered with one delivery");
    assert_eq!(
        peer.delivery_count(),
        1,
        "and nothing else left this instance"
    );
    assert_accept(
        &accepts[0],
        &follow,
        &alice,
        &peer.actor(BOB),
        &peer.inbox_path(&peer.actor(BOB)),
    );
}

/// Scenario: the same `Follow` of a locked account waits in the queue and sends
/// nothing; only the local `authorize` write puts the `Accept` on the wire, and
/// it does so after the listing showed the request.
#[tokio::test]
async fn a_locked_account_answers_its_own_requests() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (carol, carol_token) = locked_local(&pool, "ef-locked-carol").await;

    let follow = peer_follows(&app, &state, &peer, &carol, "ef-locked-follow").await;

    let waiting = follow_requests(&app, &carol_token).await;
    assert_eq!(waiting.len(), 1, "the request waits in the queue");
    assert_eq!(
        waiting[0]["acct"],
        json!(peer.handle(BOB)),
        "and names the mock's account"
    );
    let requester = waiting[0]["id"].as_str().expect("the account id");
    // A request to answer, not yet a follower, and none of hers.
    let relation = relationship(&app, &carol_token, requester).await;
    assert_flags(
        &relation,
        &[("followed_by", json!(false)), ("requested", json!(false))],
    );
    assert_eq!(
        counter(&app, &carol_token, &carol.id.to_string(), "followers_count").await,
        json!(0),
        "and it counts no follower"
    );
    assert_eq!(
        peer.delivery_count(),
        0,
        "nothing leaves a locked account before it answers"
    );

    let answered = authorize(&app, &carol_token, requester).await;
    assert_eq!(
        answered["followed_by"],
        json!(true),
        "authorizing makes the requester a follower"
    );
    let _drained = common::run_all_jobs(&state).await;

    let accepts = peer.delivered("Accept");
    assert_eq!(
        accepts.len(),
        1,
        "the Accept rides out only after the request was listed and answered"
    );
    assert_eq!(
        peer.delivery_count(),
        1,
        "the queue held exactly that one send"
    );
    assert_accept(
        &accepts[0],
        &follow,
        &carol,
        &peer.actor(BOB),
        &peer.inbox_path(&peer.actor(BOB)),
    );
    assert_eq!(
        counter(&app, &carol_token, &carol.id.to_string(), "followers_count").await,
        json!(1),
        "the counter steps up with the answer"
    );
}

/// Scenario: a local account follows the mock. The handle resolves over
/// `WebFinger`, the signed `Follow` reaches the peer's inbox, and the peer's
/// `Accept` back through `POST /inbox` grants the relationship and moves both
/// counters.
#[tokio::test]
async fn a_local_follow_reaches_the_peer_and_their_accept_grants_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ef-out-alice").await;

    let target = follow_by_uri(&app, &alice_token, &peer.handle(BOB)).await;
    let bob = target["id"].as_str().expect("the target carries its id");
    assert_eq!(
        target["pleroma"]["relationship"]["requested"],
        json!(true),
        "a follow of a remote account waits for their answer"
    );
    let _drained = common::run_all_jobs(&state).await;

    let follows = peer.delivered("Follow");
    assert_eq!(follows.len(), 1, "the peer's inbox recorded the Follow");
    assert_outbound(
        &follows[0],
        &alice,
        &peer.inbox_path(&peer.actor(BOB)),
        "Follow",
    );
    let body = follows[0].activity();
    let follow_id = body["id"].as_str().expect("the Follow carries its id");
    assert_eq!(
        body["object"],
        json!(peer.actor(BOB)),
        "the followee is the object"
    );
    assert_eq!(
        body["to"],
        json!([peer.actor(BOB)]),
        "and the followee's own address the only line"
    );

    let accept = peer.activity(
        "Accept",
        "ef-out-accept",
        &peer.actor(BOB),
        &json!(&follow_id),
    );
    peer_delivers(&app, &state, &peer, &peer.actor(BOB), &accept).await;

    // Their Accept flipped the request into a follow.
    let relation = relationship(&app, &alice_token, bob).await;
    assert_flags(
        &relation,
        &[("following", json!(true)), ("requested", json!(false))],
    );
    assert_eq!(
        counter(&app, &alice_token, &alice.id.to_string(), "following_count").await,
        json!(1),
        "our counter stepped up with it"
    );
    assert_eq!(
        lookup(&app, &peer.handle(BOB)).await["followers_count"],
        json!(1),
        "and so did theirs"
    );
    assert_eq!(
        peer.delivery_count(),
        1,
        "an answered request sends nothing onward to the peer"
    );
}

/// Scenario: the local account unfollows. The peer's inbox holds the `Undo` of
/// that very `Follow` — same id, same pair — and the relationship reads back
/// empty on both sides.
#[tokio::test]
async fn an_unfollow_withdraws_the_very_follow_the_peer_holds() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ef-undo-alice").await;

    let target = follow_by_uri(&app, &alice_token, &peer.actor(BOB)).await;
    let bob = target["id"].as_str().expect("the target carries its id");
    let _sent_follow = common::run_all_jobs(&state).await;
    let follows = peer.delivered("Follow");
    let follow_id = follows[0].activity()["id"]
        .as_str()
        .expect("the Follow carries its id")
        .to_owned();

    let _withdrawn = account_action(&app, &alice_token, bob, "unfollow").await;
    let _sent_undo = common::run_all_jobs(&state).await;

    let undos = peer.delivered("Undo");
    assert_eq!(undos.len(), 1, "the withdrawal travelled to the peer");
    assert_outbound(
        &undos[0],
        &alice,
        &peer.inbox_path(&peer.actor(BOB)),
        "Undo",
    );
    // The Undo embeds the Follow it withdraws: same id, same pair.
    let undo = undos[0].activity();
    assert_flags(
        &undo["object"],
        &[
            ("id", json!(follow_id)),
            ("type", json!("Follow")),
            ("actor", json!(alice.ap_id)),
            ("object", json!(peer.actor(BOB))),
        ],
    );
    assert_eq!(
        undo["to"],
        json!([peer.actor(BOB)]),
        "addressed to the followee alone"
    );
    // The pair holds nothing on our side either.
    let relation = relationship(&app, &alice_token, bob).await;
    assert_flags(
        &relation,
        &[("following", json!(false)), ("requested", json!(false))],
    );
    assert_eq!(
        counter(&app, &alice_token, &alice.id.to_string(), "following_count").await,
        json!(0),
        "and its counter stepped back"
    );
}

/// Scenario: the peer withdraws its own follow. The row, both counters and the
/// follower flag are gone from this instance, and the withdrawal is answered
/// with nothing at all.
#[tokio::test]
async fn a_remote_undo_takes_the_row_and_both_counters_back() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "ef-remote-undo-alice").await;

    let follow = peer_follows(&app, &state, &peer, &alice, "ef-remote-undo").await;
    assert_eq!(
        counter(&app, &alice_token, &alice.id.to_string(), "followers_count").await,
        json!(1),
        "the grant counted a follower"
    );

    let undo = peer.activity("Undo", "ef-remote-undo-2", &peer.actor(BOB), &follow["id"]);
    peer_delivers(&app, &state, &peer, &peer.actor(BOB), &undo).await;

    let follower = lookup(&app, &peer.handle(BOB)).await;
    let bob = follower["id"].as_str().expect("the mirror carries its id");
    assert_eq!(
        counter(&app, &alice_token, &alice.id.to_string(), "followers_count").await,
        json!(0),
        "her follower count stepped back with the row"
    );
    assert_eq!(
        follower["following_count"],
        json!(0),
        "and his own follow is uncounted on his side of the pair"
    );
    assert_eq!(
        relationship(&app, &alice_token, bob).await["followed_by"],
        json!(false),
        "and the viewer's flags lost the follower"
    );
    assert_eq!(
        peer.delivery_count(),
        1,
        "a withdrawal earns no answer: the Accept the grant sent is all"
    );
}
