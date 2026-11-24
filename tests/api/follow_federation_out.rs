//! Outbound follow federation over HTTP (WBE-171): the delivery each of the
//! relationship writes queues through
//! [`wired_be::services::federation_out`].
//!
//! What this pins, per the ticket:
//!
//! - `POST /api/v1/accounts/{id}/follow` of a REMOTE account queues one
//!   `federator.deliver` job carrying the row's own `Follow` — the stored
//!   `follows.ap_id` as the activity id, the follower as actor, the followee
//!   as `object` and as the only address, delivered to the followee's
//!   personal inbox alone; a re-follow of the waiting request restates the
//!   same activity id and the queue's per-inbox dedup keeps one sending;
//! - `POST …/unfollow` queues the `Undo` wrapping that very document under a
//!   fresh `{BASE_URL}/activities/{uuid}` id, for a pending or an accepted
//!   relationship alike, and a pair with nothing to withdraw queues nothing;
//! - `POST /api/v1/follow_requests/{id}/authorize` answers a REMOTE
//!   requester with an `Accept` embedding their delivered `Follow` by id,
//!   `…/reject` with the same document as a `Reject` — both to the
//!   follower's inbox alone;
//! - `POST …/remove_from_followers` of a REMOTE follower answers with that
//!   `Reject` too;
//! - every local-to-local pair — follow, unfollow, authorize, removal —
//!   writes the relationship and queues no delivery at all: the timeline is
//!   that delivery.
//!
//! [`wired_be::services::federation_out`]: wired_be::services::federation_out

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, Follow, FollowState},
        user::{Flag, User},
    },
};

use crate::common::{self, delivery_jobs};

/// The scope all four writes ask.
const WRITE: &str = "write:follows";

/// The `POST /api/v1/accounts/{target}/{verb}` write, answered.
async fn action(app: &Router, token: &str, target: &Uuid, verb: &str) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/accounts/{target}/{verb}"))
        .bearer(token)
        .send(app)
        .await
}

/// The same write, asserted to answer 200: the relationship fact stands
/// whichever way it moved.
async fn action_ok(app: &Router, token: &str, target: &Uuid, verb: &str) {
    let response = action(app, token, target, verb).await;
    common::assert_answer(
        &response,
        StatusCode::OK,
        &format!("the {verb} write answers"),
    );
}

/// The `POST /api/v1/follow_requests/{requester}/{answer}` write, answered.
async fn answer(app: &Router, token: &str, requester: &Uuid, answer_verb: &str) -> Value {
    let response = common::TestRequest::post(&format!(
        "/api/v1/follow_requests/{requester}/{answer_verb}"
    ))
    .bearer(token)
    .send(app)
    .await;
    common::assert_answer(&response, StatusCode::OK, "the request is answered");
    response.json_value()
}

/// A local account with a bearer token for the follow writes.
async fn writer(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, WRITE).await;
    (created, token)
}

/// A local account with a bearer token and the account locked, so every
/// follow of it waits in its queue for an answer.
async fn locked_writer(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |mut new| {
        new.locked = Flag::new(true);
        new
    })
    .await;
    let token = common::create_app_and_token(pool, &created, WRITE).await;
    (created, token)
}

/// How many jobs of any kind the queue holds.
async fn job_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
        .fetch_one(pool)
        .await
        .expect("the job count")
}

/// The stored relationship of the pair, which the test expects to exist.
async fn relation(pool: &PgPool, follower: &User, target: &User) -> Follow {
    follow::get(pool, follower.id, target.id)
        .await
        .expect("the follow reads")
        .expect("the relationship exists")
}

/// The `{TEST_BASE_URL}/activities/{uuid}` id a withdrawal or an answer
/// carries as its own — this instance's fresh activity, checked as a shape.
fn assert_fresh_activity_id(value: &Value) {
    let id = value["id"].as_str().expect("the activity has an id");
    let prefix = format!("{TEST_BASE_URL}/activities/");
    let tail = id
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("the activity is this instance's own: {id}"));
    assert!(Uuid::parse_str(tail).is_ok(), "the id ends in a uuid: {id}");
}

/// The delivered `Follow` a remote request carries: the model write plus the
/// id landing on it the way the inbound fold stores it.
async fn remote_request(pool: &PgPool, follower: &User, target: &User, n: &str) -> Follow {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the request fixture writes");
    assert_eq!(
        row.state,
        FollowState::Pending,
        "a locked local account answers its own requests"
    );
    let delivered = format!("https://remote.example/activities/{n}");
    follow::set_ap_id(pool, row.id, &delivered)
        .await
        .expect("the delivered id lands");
    follow::get(pool, follower.id, target.id)
        .await
        .expect("the request reads")
        .expect("the request exists")
}

/// Following a REMOTE account queues the row's `Follow` to the followee's
/// personal inbox alone — the stored `follows.ap_id` as the id, the
/// follower as actor, the followee as `object` and as the only address —
/// and neither the follower's own remote followers nor any other host
/// receives a copy.
#[tokio::test]
async fn following_a_remote_account_delivers_the_stored_follow() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = writer(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bob", "peer.example").await;
    let erin = common::create_remote_user(&pool, "erin", "other.example").await;
    // Erin follows alice and stays accepted: the follow write's only_to must
    // not sweep her in.
    let _admirer = follow::follow(&pool, TEST_BASE_URL, &erin, &alice)
        .await
        .expect("the follower fixture grants");

    action_ok(&app, &token, &bob.id, "follow").await;

    let stored = relation(&pool, &alice, &bob).await;
    assert_eq!(
        stored.state,
        FollowState::Pending,
        "a remote followee's answer is theirs to give"
    );
    let jobs = delivery_jobs(&pool).await;
    assert_eq!(jobs.len(), 1, "the request is announced once");
    assert_eq!(
        jobs[0]["inbox"],
        json!("https://peer.example/users/bob/inbox"),
        "the followee's personal inbox is the sole target"
    );
    assert_eq!(
        jobs[0]["actor_id"],
        json!(alice.id.to_string()),
        "signed as the follower"
    );
    let body = &jobs[0]["body"];
    assert_eq!(body["type"], json!("Follow"));
    assert_eq!(
        body["id"],
        json!(stored.ap_id.expect("a local follower's row carries its id")),
        "the activity id is the stored follows.ap_id"
    );
    assert_eq!(body["actor"], json!(alice.ap_id));
    assert_eq!(body["object"], json!(bob.ap_id));
    assert_eq!(
        body["to"],
        json!([bob.ap_id]),
        "the followee is the only address"
    );
    assert_eq!(body["cc"], json!([]));
    assert_eq!(
        body["published"],
        json!(wired_be::util::time::mastodon_time(&stored.inserted_at))
    );
}

/// The idempotent repeat — a second follow of the waiting pair — re-sends
/// the same activity id, and the per-activity, per-inbox dedup of the
/// delivery queue is what keeps the restated request a single sending.
#[tokio::test]
async fn a_repeated_follow_of_the_pending_request_restates_one_delivery() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = writer(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bob", "peer.example").await;

    action_ok(&app, &token, &bob.id, "follow").await;
    let first = delivery_jobs(&pool).await;
    action_ok(&app, &token, &bob.id, "follow").await;
    let second = delivery_jobs(&pool).await;

    assert_eq!(first.len(), 1, "the first follow sends once");
    assert_eq!(second.len(), 1, "and the repeat restates the live delivery");
    assert_eq!(
        second[0]["body"]["id"], first[0]["body"]["id"],
        "the same activity id — the row's own — restated, not re-minted"
    );
    assert_eq!(
        relation(&pool, &alice, &bob).await.state,
        FollowState::Pending,
        "and the relationship is unchanged"
    );
}

/// Unfollowing a remote account queues the `Undo` of the `Follow` the row
/// stood for — its own fresh activity id, the stored document embedded with
/// the stored id inside, the same single addressee — and this holds both
/// for a request that never got its answer and for one that did.
#[tokio::test]
async fn unfollowing_a_remote_account_delivers_the_undo() {
    for (nickname, grant) in [("pending-alice", false), ("accepted-alice", true)] {
        let db = common::test_db().await;
        let pool = db.pool().clone();
        let app = common::test_app(pool.clone());
        let (alice, token) = writer(&pool, nickname).await;
        let bob = common::create_remote_user(&pool, "bob", "peer.example").await;

        action_ok(&app, &token, &bob.id, "follow").await;
        let stored = relation(&pool, &alice, &bob).await;
        let follow_id = stored.ap_id.clone().expect("a local row carries its id");
        if grant {
            let _accepted = follow::accept(&pool, stored.id)
                .await
                .expect("the fixture answers the request");
        }
        let before = delivery_jobs(&pool).await.len();

        action_ok(&app, &token, &bob.id, "unfollow").await;

        let jobs = delivery_jobs(&pool).await;
        assert_eq!(jobs.len(), before + 1, "the withdrawal is announced once");
        let undo = &jobs[jobs.len() - 1];
        assert_eq!(
            undo["inbox"],
            json!("https://peer.example/users/bob/inbox"),
            "the withdrawn follow's addressee hears the taking-back"
        );
        let body = &undo["body"];
        assert_eq!(body["type"], json!("Undo"), "grant = {grant}");
        assert_eq!(body["actor"], json!(alice.ap_id));
        assert_eq!(body["to"], json!([bob.ap_id]));
        assert_eq!(body["cc"], json!([]));
        assert_fresh_activity_id(body);
        assert_ne!(
            body["id"], body["object"]["id"],
            "the wrapper stands under its own id, grant = {grant}"
        );
        assert_eq!(body["object"]["type"], json!("Follow"));
        assert_eq!(
            body["object"]["id"],
            json!(follow_id),
            "the embedded follow keeps the row's stored id, grant = {grant}"
        );
        assert_eq!(body["object"]["actor"], json!(alice.ap_id));
        assert_eq!(body["object"]["object"], json!(bob.ap_id));
    }
}

/// Unfollowing a pair that never followed is the endpoint's idempotent
/// no-op — and it federates nothing: no row, no `Undo`.
#[tokio::test]
async fn unfollowing_a_never_followed_remote_account_delivers_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = writer(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bob", "peer.example").await;

    action_ok(&app, &token, &bob.id, "unfollow").await;

    assert_eq!(
        job_count(&pool).await,
        0,
        "nothing was withdrawn, nothing sent"
    );
}

/// Authorizing a REMOTE requester's waiting request answers it with the
/// `Accept` embedding their delivered `Follow` by id, to their personal
/// inbox alone; rejecting answers the same activity with a `Reject`. The
/// relationship answer itself belongs to `follow_requests.rs`; here it is
/// the delivery the endpoint owes the peer.
#[tokio::test]
async fn answering_a_remote_request_delivers_the_accept_or_the_reject() {
    for (verb, kind) in [("authorize", "Accept"), ("reject", "Reject")] {
        let db = common::test_db().await;
        let pool = db.pool().clone();
        let app = common::test_app(pool.clone());
        let (carol, token) = locked_writer(&pool, &format!("frq-{verb}")).await;
        let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
        let request = remote_request(&pool, &bob, &carol, &format!("frq-{verb}-1")).await;
        let delivered_id = request.ap_id.clone().expect("the delivered id stands");

        let relationship = answer(&app, &token, &bob.id, verb).await;
        assert_eq!(
            relationship["followed_by"],
            json!(verb == "authorize"),
            "the endpoint still answers the relationship itself"
        );

        let jobs = delivery_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the answer is delivered once, {verb}");
        assert_eq!(
            jobs[0]["inbox"],
            json!("https://remote.example/users/bob/inbox"),
            "the follower's personal inbox is the sole target, {verb}"
        );
        assert_eq!(
            jobs[0]["actor_id"],
            json!(carol.id.to_string()),
            "signed as the local followee, {verb}"
        );
        let body = &jobs[0]["body"];
        assert_eq!(body["type"], json!(kind));
        assert_eq!(body["actor"], json!(carol.ap_id));
        assert_eq!(body["to"], json!([bob.ap_id]));
        assert_eq!(body["cc"], json!([]));
        assert_fresh_activity_id(body);
        assert_eq!(
            body["object"],
            json!({
                "id": delivered_id,
                "type": "Follow",
                "actor": bob.ap_id,
                "object": carol.ap_id,
            }),
            "the answer embeds the delivered Follow, the shape peers match on"
        );
    }
}

/// Dropping a REMOTE follower answers their delivered `Follow` with a
/// `Reject` at their personal inbox — the removal the followee performed is
/// a refusal the follower must hear.
#[tokio::test]
async fn removing_a_remote_follower_delivers_the_reject() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = locked_writer(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let request = remote_request(&pool, &bob, &alice, "drop-1").await;
    let _accepted = follow::accept(&pool, request.id)
        .await
        .expect("the fixture grants the request");
    let delivered_id = request.ap_id.clone().expect("the delivered id stands");

    action_ok(&app, &token, &bob.id, "remove_from_followers").await;

    let jobs = delivery_jobs(&pool).await;
    assert_eq!(jobs.len(), 1, "the dropped follower is answered once");
    assert_eq!(
        jobs[0]["inbox"],
        json!("https://remote.example/users/bob/inbox"),
        "the follower's personal inbox is the sole target"
    );
    assert_eq!(
        jobs[0]["actor_id"],
        json!(alice.id.to_string()),
        "signed as the followee who removed them"
    );
    let body = &jobs[0]["body"];
    assert_eq!(body["type"], json!("Reject"));
    assert_eq!(body["actor"], json!(alice.ap_id));
    assert_eq!(body["to"], json!([bob.ap_id]));
    assert_fresh_activity_id(body);
    assert_eq!(
        body["object"]["id"],
        json!(delivered_id),
        "the Reject answers the very Follow the row stood for"
    );
}

/// Every local-to-local move of these four writes settles in the database:
/// follow, unfollow, authorize, remove — no delivery job, ever, in either
/// direction.
#[tokio::test]
async fn local_relationships_never_produce_deliveries() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = writer(&pool, "alice").await;
    let (carol, carol_token) = locked_writer(&pool, "carol").await;

    action_ok(&app, &alice_token, &carol.id, "follow").await;
    assert_eq!(
        relation(&pool, &alice, &carol).await.state,
        FollowState::Pending,
        "the locked local account holds the request"
    );
    let granted = answer(&app, &carol_token, &alice.id, "authorize").await;
    assert_eq!(
        granted["followed_by"],
        json!(true),
        "the local requester becomes a follower"
    );
    action_ok(&app, &alice_token, &carol.id, "unfollow").await;

    let (bob, bob_token) = writer(&pool, "bob").await;
    action_ok(&app, &bob_token, &alice.id, "follow").await;
    action_ok(&app, &alice_token, &bob.id, "remove_from_followers").await;

    assert_eq!(
        job_count(&pool).await,
        0,
        "a local pair produces no job of any kind"
    );
}
