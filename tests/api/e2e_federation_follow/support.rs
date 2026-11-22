//! The fixtures, readers and capture assertions the follow end-to-end tests
//! share. The scenario modules are its children, so they reach every helper
//! through `use super::*`.

mod blocks;
mod follows;
mod relays;

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
