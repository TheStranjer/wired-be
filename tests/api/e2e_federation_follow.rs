//! End-to-end federation of relationships (WBE-224 scenario 2 and the relay
//! half of scenario 7): follow handshakes, withdrawals and blocks over real
//! HTTP.
//!
//! Everything here runs against [`remote::MockPeer`] — a loopback instance
//! with its own keys, actor documents, `WebFinger` and capturing inboxes —
//! through this instance's own public surface: the peer posts signed
//! activities to `POST /inbox`, this instance's answers reach the peer's inbox
//! through the real federation worker, and every fact is read back through
//! `GET /api/v1/accounts/lookup`, `/api/v1/follow_requests`,
//! `/api/v1/accounts/relationships`, `/api/v1/accounts/{id}` and
//! `/api/v1/timelines/public`, or from the peer's capture. Nothing waits on a
//! clock: [`common::run_all_jobs`] drains each queue by hand.
//!
//! What the scenarios pin:
//!
//! - a peer's `Follow` of a local account grants on the spot, is visible as a
//!   follower everywhere the client API says so, and is answered by one signed
//!   `Accept` at the follower's own inbox, embedding the `Follow` and addressed
//!   to the follower alone;
//! - a locked account holds the same `Follow` in `GET /api/v1/follow_requests`
//!   and sends nothing until its owner authorizes it;
//! - `POST /api/v1/follows` of a handle resolves the peer, delivers a signed
//!   `Follow`, and the peer's `Accept` back through `POST /inbox` grants the
//!   relationship and steps both counters;
//! - `POST …/unfollow` sends the `Undo` of that very activity, and a peer's own
//!   `Undo` takes its follow — row and both counters — back;
//! - `POST …/block` publishes a signed `Block` and severs the pair, a peer's
//!   `Block` of a local account reads as `blocked_by` and stops carrying her
//!   posts to him, and his `Undo` of the block lets her be followed again;
//! - [`follow_relay`] against the peer's `Service` relay follows over the real
//!   fetch and delivery, the peer's `Accept` accepts, the accepted relay joins
//!   the fan-out of the next public `Create` at exactly one inbox — its own,
//!   which is the shared endpoint the follower expands to too — its `Announce`
//!   lands as an ordinary public remote status with no boost behind it, and
//!   [`unfollow_relay`] sends the `Undo` and closes the fan-out.
//!
//! The relay rows (the `relays` subscription and the instance actor's own
//! `follows` row) are read from their models: no client endpoint lists them,
//! which is what `admin_federation.rs` exists for.
//!
//! The per-endpoint refusals belong to the area files (`follow_federation.rs`,
//! `follow_federation_out.rs`, `follow_requests.rs`, `blocks.rs`, `relays.rs`);
//! this module owns the round trips.
//!
//! [`follow_relay`]: wired_be::services::relay::follow_relay
//! [`unfollow_relay`]: wired_be::services::relay::unfollow_relay

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, FollowState},
        relay::{self, RelayState},
        user::{Flag, NewUser, User, get_by_ap_id},
    },
    services::{
        instance_actor::ensure_instance_actor,
        relay::{follow_relay, unfollow_relay},
    },
    state::AppState,
};

use crate::{common, remote};

/// The mock's follower account, `bob@127.0.0.1:{port}`.
const BOB: &str = "bob";
/// The instance domain of [`common::test_config`], which decision 7 puts into
/// every handle the mock reads off this instance.
const DOMAIN: &str = "test.example";
/// The account routes the scenarios read and write.
const ACCOUNTS: &str = "/api/v1/accounts";
/// The caller's follow-request queue and its answer routes.
const FOLLOW_REQUESTS: &str = "/api/v1/follow_requests";
/// The legacy follow write keyed by handle or actor URL.
const FOLLOWS: &str = "/api/v1/follows";
/// The status write.
const STATUSES: &str = "/api/v1/statuses";
/// The public timeline read.
const PUBLIC_TIMELINE: &str = "/api/v1/timelines/public";
/// The scopes every local caller carries: `read write` covers each sub-scope
/// the relationship and status writes in these scenarios ask for.
const SCOPES: &str = "read write";

// --- the fixtures ----------------------------------------------------------------------

/// A local account named `nickname` that can sign its own deliveries, with a
/// bearer token granting [`SCOPES`].
async fn local(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = remote::keyed_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, SCOPES).await;
    (created, token)
}

/// A locked local account named `nickname` with a [`SCOPES`] token: the same
/// key-carrying fixture, holding its own answers.
async fn locked_local(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = remote::keyed_local_user_with(pool, nickname, |user| NewUser {
        locked: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, SCOPES).await;
    (created, token)
}

/// This instance's own actor: the signer of a relay subscription.
async fn instance_actor(state: &AppState) -> User {
    ensure_instance_actor(&state.db, &common::test_config())
        .await
        .expect("the instance actor stands")
}

/// The stored mirror of the peer's relay actor.
async fn relay_mirror(state: &AppState, peer: &remote::MockPeer) -> User {
    get_by_ap_id(&state.db, &peer.relay())
        .await
        .expect("the relay mirror reads")
        .expect("the relay follow mirrored the peer's actor")
}

// --- reading the public surface back ----------------------------------------------------

/// `GET /api/v1/accounts/lookup` for `acct`, answered as the `Account` entity:
/// the handle is what a peer, or a client, names an account by.
async fn lookup(app: &Router, acct: &str) -> Value {
    let response = common::TestRequest::get(&format!("{ACCOUNTS}/lookup?acct={acct}"))
        .send(app)
        .await;
    common::assert_answer(
        &response,
        StatusCode::OK,
        &format!("the lookup of `{acct}`"),
    );
    response.json_value()
}

/// The account id `acct` resolves to.
async fn account_id(app: &Router, acct: &str) -> String {
    let entity = lookup(app, acct).await;
    entity["id"]
        .as_str()
        .expect("the account entity carries its id")
        .to_owned()
}

/// `GET /api/v1/accounts/{id}` as the bearer of `token` — an anonymous read
/// when there is none — answered as the `Account` entity.
async fn account(app: &Router, token: Option<&str>, id: &str) -> Value {
    let request = common::TestRequest::get(&format!("{ACCOUNTS}/{id}"));
    let response = match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    };
    common::assert_answer(&response, StatusCode::OK, "the account read");
    response.json_value()
}

/// One denormalised counter off `GET /api/v1/accounts/{id}`: `followers_count`,
/// `following_count` or `statuses_count`, whichever the scenario is watching.
async fn counter(app: &Router, token: &str, id: &str, column: &str) -> Value {
    let entity = account(app, Some(token), id).await;
    entity.get(column).cloned().unwrap_or(Value::Null)
}

/// `GET /api/v1/accounts/relationships` for one `target`, as the caller of
/// `token`: the single relationship entity that viewer holds.
async fn relationship(app: &Router, token: &str, target: &str) -> Value {
    let response = common::TestRequest::get(&format!("{ACCOUNTS}/relationships?id%5B%5D={target}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the relationships read");
    let entities: Vec<Value> = response.json();
    let found = entities.first().cloned();
    found.expect("one relationship per requested id")
}

/// `GET /api/v1/follow_requests` as `token`: the accounts whose follow of the
/// caller still waits.
async fn follow_requests(app: &Router, token: &str) -> Vec<Value> {
    let response = common::TestRequest::get(FOLLOW_REQUESTS)
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the follow-request queue");
    response.json::<Vec<Value>>()
}

/// `POST /api/v1/follow_requests/{requester}/authorize` as `token`, answered
/// with the caller's relationship to the requester.
async fn authorize(app: &Router, token: &str, requester: &str) -> Value {
    let response = common::TestRequest::post(&format!("{FOLLOW_REQUESTS}/{requester}/authorize"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the authorize write");
    response.json_value()
}

/// `POST /api/v1/follows` of `uri`, asserted 200 and answered with the target's
/// `Account` entity the write returns.
async fn follow_by_uri(app: &Router, token: &str, uri: &str) -> Value {
    let response = common::TestRequest::post(FOLLOWS)
        .bearer(token)
        .json(&json!({"uri": uri}))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the follow write");
    response.json_value()
}

/// `POST /api/v1/accounts/{target}/{verb}` — the `unfollow` and `block` writes
/// — asserted 200 and answered as the entity it returns.
async fn account_action(app: &Router, token: &str, target: &str, verb: &str) -> Value {
    let response = common::TestRequest::post(&format!("{ACCOUNTS}/{target}/{verb}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, &format!("the {verb} write"));
    response.json_value()
}

/// `POST /api/v1/statuses` with `text`, asserted 200 and answered with the
/// status entity it wrote.
async fn post_public(app: &Router, token: &str, text: &str) -> Value {
    let response = common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&json!({"status": text}))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the public post");
    response.json_value()
}

/// The entry of `GET /api/v1/timelines/public` whose `uri` is `uri`, when the
/// page carries one.
async fn timeline_entry(app: &Router, uri: &str) -> Option<Value> {
    let response = common::TestRequest::get(PUBLIC_TIMELINE).send(app).await;
    common::assert_answer(&response, StatusCode::OK, "the public timeline");
    let entries: Vec<Value> = response.json();
    entries
        .into_iter()
        .find(|entry| entry["uri"].as_str() == Some(uri))
}

// --- driving the peer and the queues -----------------------------------------------------

/// Delivers the mock's signed `Follow` of `target` through this instance's
/// shared inbox and drains the fold, the answer it queued and the delivery it
/// enqueued; returns the `Follow` document the peer sent.
async fn peer_follows(
    app: &Router,
    state: &AppState,
    peer: &remote::MockPeer,
    target: &User,
    name: &str,
) -> Value {
    let follow = peer.follow_of(name, &peer.actor(BOB), &target.ap_id);
    let folded = peer
        .deliver(app, &peer.actor(BOB), remote::INBOX, &follow)
        .await;
    assert_eq!(
        folded.status,
        StatusCode::ACCEPTED,
        "the inbox folds the mock's Follow"
    );
    let _drained = common::run_all_jobs(state).await;
    follow
}

/// `POST`s `activity` to this instance's shared inbox as the peer's actor
/// `actor` and drains the fold plus everything it sent onward.
async fn peer_delivers(
    app: &Router,
    state: &AppState,
    peer: &remote::MockPeer,
    actor: &str,
    activity: &Value,
) {
    let folded = peer.deliver(app, actor, remote::INBOX, activity).await;
    let kind = activity["type"].as_str().unwrap_or("activity");
    assert_eq!(
        folded.status,
        StatusCode::ACCEPTED,
        "the inbox folds the mock's {kind}"
    );
    let _drained = common::run_all_jobs(state).await;
}

/// Follows the peer's `Service` relay and rides its `Accept` back through the
/// peer's own inbox route, so the subscription stands accepted: returns the id
/// of the `Follow` this instance sent.
async fn relay_accepted(app: &Router, state: &AppState, peer: &remote::MockPeer) -> String {
    let subscription = follow_relay(state, &peer.relay())
        .await
        .expect("the relay follow runs");
    assert_eq!(
        subscription.state,
        RelayState::Pending,
        "a fresh subscription waits for its answer"
    );
    let _drained = common::run_all_jobs(state).await;
    let follows = peer.delivered("Follow");
    assert_eq!(follows.len(), 1, "the relay's inbox recorded the Follow");
    let follow_id = follows[0].activity()["id"]
        .as_str()
        .expect("the Follow carries its own id")
        .to_owned();
    let accept = peer.activity("Accept", "relay-accept", &peer.relay(), &json!(&follow_id));
    peer_delivers(app, state, peer, &peer.relay(), &accept).await;
    follow_id
}

// --- asserting the peer's capture ---------------------------------------------------------

/// Asserts the entity carries each `(key, value)` pair of `expected` verbatim:
/// the shape a scenario reads off a relationship or an embedded activity, one
/// named fact per line.
fn assert_flags(entity: &Value, expected: &[(&str, Value)]) {
    for (key, value) in expected {
        assert_eq!(
            &entity[*key], value,
            "the entity's `{key}` reads as expected"
        );
    }
}

/// Asserts `delivery` is this instance's own signed `POST` of a `kind` activity
/// by `actor` that arrived at `path`: the `Digest` describes the bytes that
/// arrived, the `Signature` names `actor`'s key and verifies against the key
/// `actor` publishes, and the document names `actor` as its maker under a
/// decision 6 activity id.
fn assert_outbound(delivery: &remote::Delivery, actor: &User, path: &str, kind: &str) {
    let body = delivery.activity();
    assert_eq!(delivery.path(), path, "the {kind} arrived at one inbox");
    assert_eq!(body["type"], json!(kind), "the delivered activity type");
    assert_eq!(
        body["actor"],
        json!(actor.ap_id),
        "made by this instance's actor"
    );
    let id = body["id"].as_str().expect("the activity carries its id");
    assert!(
        id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
        "and its id is this instance's own: {id}"
    );
    assert_eq!(
        delivery.key_id().as_deref(),
        Some(format!("{}#main-key", actor.ap_id).as_str()),
        "the signer is the actor the activity names"
    );
    assert!(
        delivery.digest_matches(),
        "the digest describes the delivered bytes"
    );
    let pem = actor
        .public_key
        .as_deref()
        .expect("the fixture actor publishes its key");
    assert!(
        delivery.signature_verifies(pem),
        "and the signature verifies against that key"
    );
}

/// Asserts `delivery` is the `Accept` answering `follow`: one signed `POST` to
/// the follower's own inbox, made by the local `target`, addressed to the
/// follower alone, and embedding the `Follow` it answers.
fn assert_accept(
    delivery: &remote::Delivery,
    follow: &Value,
    target: &User,
    follower: &str,
    inbox_path: &str,
) {
    assert_outbound(delivery, target, inbox_path, "Accept");
    let body = delivery.activity();
    assert_eq!(
        body["to"],
        json!([follower]),
        "the follower is the answer's only address"
    );
    assert_eq!(
        body["object"],
        json!({
            "id": follow["id"],
            "type": "Follow",
            "actor": follow["actor"],
            "object": follow["object"],
        }),
        "the Accept embeds the Follow it answers"
    );
}

/// Both relay rows after the peer's `Accept`: the instance actor's `follows`
/// row and the `relays` subscription, each standing accepted. Neither has a
/// client endpoint of its own, which is why this — and only this — reads the
/// models.
async fn assert_granted_relay(pool: &PgPool, mirror: &User, instance: &User, relay_actor: &str) {
    let granted = follow::get(pool, instance.id, mirror.id)
        .await
        .expect("the follow reads")
        .expect("the instance actor follows the relay");
    assert_eq!(
        granted.state,
        FollowState::Accepted,
        "the relay's Accept grants the instance actor's follow"
    );
    let subscription = relay::get(pool, relay_actor)
        .await
        .expect("the ledger reads")
        .expect("the relay subscription stands");
    assert_eq!(
        subscription.state,
        RelayState::Accepted,
        "and the subscription with it"
    );
}

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

/// Scenario: this instance follows the peer's relay over the real fetch and the
/// real delivery, and the relay's `Accept` — posted back through `POST /inbox`
/// — turns both of its rows accepted.
#[tokio::test]
async fn following_a_relay_runs_the_handshake_over_http() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;

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
    let instance = instance_actor(&state).await;
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

    let instance = instance_actor(&state).await;
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
