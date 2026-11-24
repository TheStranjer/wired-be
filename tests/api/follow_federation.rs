//! Inbound follow handshake over federation (WBE-168): the dispatcher's own
//! entry point [`handle_incoming`] folding a peer's `Follow`, `Accept`,
//! `Reject` and `Undo`(of a `Follow`) onto this instance's `follows` rows
//! (`src/federation/handlers/follow.rs`).
//!
//! What this pins, per the ticket:
//!
//! - a delivered `Follow` of an unlocked local user grants on the spot —
//!   the row with the delivered id as its `ap_id`, the counters, the
//!   followee's `follow` telling, and an enqueued `Accept` delivery whose
//!   embedded object is the original `Follow` `{id, type, actor, object}`
//!   addressed to the follower's inbox alone;
//! - a locked target holds the request `pending` with a `follow_request`
//!   telling and sends nothing until the user authorizes;
//! - a blocked pair is answered with a `Reject` and stores nothing;
//! - an `Accept` from the followee flips our own pending follow (by bare
//!   activity id or by the embedded pair), a `Reject` removes it — with its
//!   counters when the relationship stood accepted — and a wrong actor folds
//!   nothing;
//! - an `Undo` by the follower takes the row and the pair's tellings back;
//! - a duplicate `Follow` restates one row, moves the stored id to the
//!   newest activity, and re-sends the `Accept` for a granted pair.
//!
//! [`handle_incoming`]: wired_be::federation::dispatch::handle_incoming

#![cfg(test)]

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        block, follow,
        user::{Flag, NewUser, User},
    },
};

use crate::common::{self, deliver, delivering_state, delivery_jobs, reload_user};

/// The remote actors the fixtures deliver as.
const BOB: &str = "https://remote.example/users/bob";
const MALLORY: &str = "https://remote.example/users/mallory";

/// A `Follow` by `actor` of `object`, named `n` on the actor's host.
fn follow_activity(n: &str, actor: &str, object: &str) -> Value {
    activity_of("Follow", n, actor, &json!(object))
}

/// The id [`follow_activity`] mints for `n` on `actor`'s host.
fn follow_id(n: &str, actor: &str) -> String {
    activity_id(n, actor)
}

/// An `Accept` / `Reject` / `Undo` by `actor`, named `n` on the actor's
/// host, answering `object` (an id or an embedded inner `Follow`).
fn answer(kind: &str, n: &str, actor: &str, object: &Value) -> Value {
    activity_of(kind, n, actor, object)
}

/// The shape every fixture activity shares.
fn activity_of(kind: &str, n: &str, actor: &str, object: &Value) -> Value {
    let host = actor.split('/').nth(2).expect("the fixture actor is a url");
    json!({
        "id": format!("https://{host}/activities/{n}"),
        "type": kind,
        "actor": actor,
        "object": object,
    })
}

/// The activity id the fixtures mint for `n` on `actor`'s host.
fn activity_id(n: &str, actor: &str) -> String {
    let host = actor.split('/').nth(2).expect("the fixture actor is a url");
    format!("https://{host}/activities/{n}")
}

// --- reading the state back -----------------------------------------------------

/// The stored row of the pair (follower `follower`, followed account
/// `target`).
async fn relation(pool: &PgPool, follower: &User, target: &User) -> follow::Follow {
    follow::get(pool, follower.id, target.id)
        .await
        .expect("the follow reads")
        .expect("the relationship exists")
}

/// `(type, from_account_id, status_id)` for every notification of `user_id`,
/// oldest first.
async fn told(pool: &PgPool, user_id: Uuid) -> Vec<(String, Uuid, Option<Uuid>)> {
    sqlx::query_as::<_, (String, Uuid, Option<Uuid>)>(
        "SELECT type, from_account_id, status_id FROM notifications \
          WHERE user_id = $1 ORDER BY id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("the notifications read")
}

/// How many follow rows the table holds.
async fn follow_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM follows")
        .fetch_one(pool)
        .await
        .expect("the table reads")
}

/// How many jobs of any kind the queue holds.
async fn job_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
        .fetch_one(pool)
        .await
        .expect("the job count")
}

/// The `{TEST_BASE_URL}/activities/{uuid}` id the answer carries: its shape
/// checked.
fn assert_own_activity_id(args: &Value) {
    let id = args["body"]["id"].as_str().expect("the answer has an id");
    let prefix = format!("{TEST_BASE_URL}/activities/");
    let tail = id
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("the answer is this instance's own: {id}"));
    assert!(
        Uuid::parse_str(tail).is_ok(),
        "the answer id is a uuid: {id}"
    );
}

// --- Follow -----------------------------------------------------------------------

#[tokio::test]
async fn a_follow_of_an_open_local_account_grants_and_answers_with_an_accept() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let alice = common::create_local_user(&pool, "ff-open-alice").await;

    deliver(
        &state,
        &follow_activity("ff-open-1", BOB, &alice.ap_id),
        &bob,
    )
    .await;

    let row = relation(&pool, &bob, &alice).await;
    assert_eq!(
        row.state,
        follow::FollowState::Accepted,
        "this instance owns the answer for an unlocked local account"
    );
    assert_eq!(
        row.ap_id.as_deref(),
        Some(follow_id("ff-open-1", BOB).as_str()),
        "the delivered activity id lands on the row (decision 6)"
    );
    assert_eq!(
        (
            reload_user(&pool, alice.id).await.followers_count,
            reload_user(&pool, bob.id).await.following_count
        ),
        (1, 1),
        "the counters step up with the grant"
    );
    assert_eq!(
        told(&pool, alice.id).await,
        vec![("follow".to_owned(), bob.id, None)],
        "the local followee is told of the new follower"
    );

    let jobs = delivery_jobs(&pool).await;
    assert_eq!(jobs.len(), 1, "the grant is answered with one delivery");
    let args = &jobs[0];
    assert_eq!(
        args["inbox"],
        json!("https://remote.example/users/bob/inbox"),
        "the answer goes to the follower's personal inbox"
    );
    assert_eq!(
        args["actor_id"],
        json!(alice.id.to_string()),
        "signed as the local followee"
    );
    assert_own_activity_id(args);
    let body = &args["body"];
    assert_eq!(body["type"], json!("Accept"));
    assert_eq!(body["actor"], json!(alice.ap_id));
    assert_eq!(body["to"], json!([BOB]), "the follower is the only address");
    assert_eq!(
        body["object"],
        json!({
            "id": follow_id("ff-open-1", BOB),
            "type": "Follow",
            "actor": BOB,
            "object": alice.ap_id,
        }),
        "the Accept embeds the original Follow, the shape Mastodon matches on"
    );
}

#[tokio::test]
async fn a_follow_of_a_locked_local_account_waits_with_a_request_and_sends_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let carol = common::create_local_user_with(&pool, "ff-locked-carol", |u| NewUser {
        locked: Flag::new(true),
        ..u
    })
    .await;

    deliver(
        &state,
        &follow_activity("ff-locked-1", BOB, &carol.ap_id),
        &bob,
    )
    .await;

    let row = relation(&pool, &bob, &carol).await;
    assert_eq!(
        row.state,
        follow::FollowState::Pending,
        "a locked account answers its own requests"
    );
    assert_eq!(
        row.ap_id.as_deref(),
        Some(follow_id("ff-locked-1", BOB).as_str()),
        "and the request still carries the delivered id"
    );
    assert_eq!(
        reload_user(&pool, carol.id).await.followers_count,
        0,
        "a waiting request counts nothing"
    );
    assert_eq!(
        told(&pool, carol.id).await,
        vec![("follow_request".to_owned(), bob.id, None)],
        "the followee is told of the request to answer"
    );
    assert_eq!(
        job_count(&pool).await,
        0,
        "and nothing leaves until she authorizes"
    );
}

#[tokio::test]
async fn a_follow_across_a_block_is_answered_with_a_reject_and_stores_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let alice = common::create_local_user(&pool, "ff-blocked-alice").await;
    let _blocked = block::block(&pool, TEST_BASE_URL, &alice, &bob)
        .await
        .expect("the local account blocks the remote one");

    deliver(
        &state,
        &follow_activity("ff-blocked-1", BOB, &alice.ap_id),
        &bob,
    )
    .await;

    assert_eq!(
        follow_count(&pool).await,
        0,
        "the blocked pair stores nothing"
    );
    assert!(
        told(&pool, alice.id).await.is_empty(),
        "and nobody is told of the refused request"
    );

    let jobs = delivery_jobs(&pool).await;
    assert_eq!(jobs.len(), 1, "the refusal answers with one delivery");
    assert_own_activity_id(&jobs[0]);
    let body = &jobs[0]["body"];
    assert_eq!(body["type"], json!("Reject"));
    assert_eq!(body["actor"], json!(alice.ap_id));
    assert_eq!(body["to"], json!([BOB]));
    assert_eq!(
        body["object"]["id"],
        json!(follow_id("ff-blocked-1", BOB)),
        "the Reject answers the delivered Follow by its own id"
    );
}

#[tokio::test]
async fn a_follow_of_anything_but_an_active_local_account_folds_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let dormant = common::create_local_user_with(&pool, "ff-dormant", |u| NewUser {
        is_active: Flag::new(false),
        ..u
    })
    .await;

    for (n, object) in [
        ("ff-unfit-1", mallory.ap_id.clone()),
        (
            "ff-unfit-2",
            "https://remote.example/users/never-stored".to_owned(),
        ),
        ("ff-unfit-3", dormant.ap_id.clone()),
    ] {
        deliver(&state, &follow_activity(n, BOB, &object), &bob).await;
    }
    // An embedded object that names no id at all has nothing to resolve.
    deliver(
        &state,
        &answer("Follow", "ff-unfit-4", BOB, &json!({"type": "Note"})),
        &bob,
    )
    .await;

    assert_eq!(
        follow_count(&pool).await,
        0,
        "none of the four unfit targets stored a follow"
    );
    assert_eq!(job_count(&pool).await, 0, "and none answered anything");
}

// --- Accept ------------------------------------------------------------------------

/// The fixture of the answer routes: `alice`'s own outgoing follow of the
/// remote `bob`, waiting in `pending` with its minted activity id.
async fn outgoing_follow(pool: &PgPool, alice: &User, bob: &User) -> follow::Follow {
    let row = follow::follow(pool, TEST_BASE_URL, alice, bob)
        .await
        .expect("the outgoing follow stores");
    assert_eq!(
        row.state,
        follow::FollowState::Pending,
        "a follow of a remote account waits for their answer"
    );
    row
}

#[tokio::test]
async fn an_accept_by_the_followee_grants_our_pending_follow() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let alice = common::create_local_user(&pool, "fa-alice").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let row = outgoing_follow(&pool, &alice, &bob).await;
    let ap_id = row.ap_id.clone().expect("our follow carries an id");

    deliver(
        &state,
        &answer("Accept", "fa-accept-1", BOB, &json!(&ap_id)),
        &bob,
    )
    .await;

    assert_eq!(
        relation(&pool, &alice, &bob).await.state,
        follow::FollowState::Accepted,
        "the bare activity id keys the follow and the answer grants it"
    );
    assert_eq!(
        (
            reload_user(&pool, alice.id).await.following_count,
            reload_user(&pool, bob.id).await.followers_count
        ),
        (1, 1),
        "the counters step up with the transition"
    );
    assert!(
        delivery_jobs(&pool).await.is_empty(),
        "an answered request sends nothing onward"
    );
}

#[tokio::test]
async fn an_accept_carrying_the_inner_follow_grants_by_the_pair() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let alice = common::create_local_user(&pool, "fp-alice").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let row = outgoing_follow(&pool, &alice, &bob).await;

    deliver(
        &state,
        &answer(
            "Accept",
            "fp-accept-1",
            BOB,
            &json!({
                "id": row.ap_id.clone().expect("our follow carries an id"),
                "type": "Follow",
                "actor": alice.ap_id,
                "object": BOB,
            }),
        ),
        &bob,
    )
    .await;

    assert_eq!(
        relation(&pool, &alice, &bob).await.state,
        follow::FollowState::Accepted,
        "the embedded actor/object pair keys the follow just as well"
    );
}

#[tokio::test]
async fn an_accept_from_another_actor_grants_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let alice = common::create_local_user(&pool, "fw-alice").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let row = outgoing_follow(&pool, &alice, &bob).await;
    let ap_id = row.ap_id.clone().expect("our follow carries an id");

    // Mallory signs an Accept of the follow of bob: the followee answers,
    // nobody else — by the bare id, and by an inner pair naming her.
    deliver(
        &state,
        &answer("Accept", "fw-accept-1", MALLORY, &json!(&ap_id)),
        &mallory,
    )
    .await;
    deliver(
        &state,
        &answer(
            "Accept",
            "fw-accept-2",
            MALLORY,
            &json!({"id": &ap_id, "type": "Follow", "actor": alice.ap_id, "object": MALLORY}),
        ),
        &mallory,
    )
    .await;

    assert_eq!(
        relation(&pool, &alice, &bob).await.state,
        follow::FollowState::Pending,
        "the foreign answer moved nothing"
    );
    assert_eq!(
        reload_user(&pool, alice.id).await.following_count,
        0,
        "and counted nothing"
    );
}

#[tokio::test]
async fn an_accept_naming_nothing_we_store_is_ignored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;

    deliver(
        &state,
        &answer(
            "Accept",
            "fu-accept-1",
            BOB,
            &json!("https://remote.example/activities/never-followed"),
        ),
        &bob,
    )
    .await;

    assert_eq!(
        follow_count(&pool).await,
        0,
        "an unknown follow stays unknown"
    );
    assert_eq!(job_count(&pool).await, 0, "and nothing else happened");
}

// --- Reject -------------------------------------------------------------------------

#[tokio::test]
async fn a_reject_by_the_followee_removes_our_pending_follow() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let alice = common::create_local_user(&pool, "fr-pending-alice").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let row = outgoing_follow(&pool, &alice, &bob).await;
    let ap_id = row.ap_id.clone().expect("our follow carries an id");

    deliver(
        &state,
        &answer("Reject", "fr-reject-1", BOB, &json!(&ap_id)),
        &bob,
    )
    .await;

    assert!(
        follow::get(&pool, alice.id, bob.id)
            .await
            .expect("the follow reads")
            .is_none(),
        "the refused request is gone"
    );
    assert_eq!(
        reload_user(&pool, alice.id).await.following_count,
        0,
        "a request that never counted leaves nothing to uncount"
    );
}

#[tokio::test]
async fn a_reject_of_an_accepted_follow_steps_the_counters_back() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let alice = common::create_local_user(&pool, "fr-revoked-alice").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let row = outgoing_follow(&pool, &alice, &bob).await;
    let granted = follow::accept(&pool, row.id)
        .await
        .expect("the fixture grants");
    assert_eq!(
        (
            reload_user(&pool, alice.id).await.following_count,
            reload_user(&pool, bob.id).await.followers_count
        ),
        (1, 1),
        "the grant counted"
    );

    deliver(
        &state,
        &answer(
            "Reject",
            "fr-revoked-1",
            BOB,
            &json!({"id": granted.ap_id.expect("an id"), "type": "Follow", "actor": alice.ap_id, "object": BOB}),
        ),
        &bob,
    )
    .await;

    assert!(
        follow::get(&pool, alice.id, bob.id)
            .await
            .expect("the follow reads")
            .is_none(),
        "the remote revoked the relationship"
    );
    assert_eq!(
        (
            reload_user(&pool, alice.id).await.following_count,
            reload_user(&pool, bob.id).await.followers_count
        ),
        (0, 0),
        "and its counters step down with it"
    );
}

// --- Undo of Follow -------------------------------------------------------------------

#[tokio::test]
async fn an_undo_takes_back_the_row_the_counters_and_the_telling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let alice = common::create_local_user(&pool, "fu-undo-alice").await;
    deliver(
        &state,
        &follow_activity("fu-undo-1", BOB, &alice.ap_id),
        &bob,
    )
    .await;
    assert_eq!(
        told(&pool, alice.id).await,
        vec![("follow".to_owned(), bob.id, None)],
        "the grant told the followee"
    );

    deliver(
        &state,
        &answer(
            "Undo",
            "fu-undo-2",
            BOB,
            &json!(follow_id("fu-undo-1", BOB)),
        ),
        &bob,
    )
    .await;

    assert_eq!(follow_count(&pool).await, 0, "the follow row went");
    assert_eq!(
        (
            reload_user(&pool, alice.id).await.followers_count,
            reload_user(&pool, bob.id).await.following_count
        ),
        (0, 0),
        "its counters stepped down"
    );
    assert!(
        told(&pool, alice.id).await.is_empty(),
        "and the `follow` telling is gone with it"
    );
    assert_eq!(
        delivery_jobs(&pool).await.len(),
        1,
        "the withdrawal itself sends nothing back"
    );
}

#[tokio::test]
async fn an_undo_carrying_the_inner_follow_withdraws_by_the_pair() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let carol = common::create_local_user_with(&pool, "fu-locked-carol", |u| NewUser {
        locked: Flag::new(true),
        ..u
    })
    .await;
    deliver(
        &state,
        &follow_activity("fu-pair-1", BOB, &carol.ap_id),
        &bob,
    )
    .await;
    assert_eq!(
        told(&pool, carol.id).await,
        vec![("follow_request".to_owned(), bob.id, None)],
        "the request told the locked account"
    );

    deliver(
        &state,
        &answer(
            "Undo",
            "fu-pair-2",
            BOB,
            &json!({
                "id": follow_id("fu-pair-1", BOB),
                "type": "Follow",
                "actor": BOB,
                "object": carol.ap_id,
            }),
        ),
        &bob,
    )
    .await;

    assert_eq!(
        follow_count(&pool).await,
        0,
        "the withdrawn request is gone, id and pair both key it"
    );
    assert!(
        told(&pool, carol.id).await.is_empty(),
        "and the `follow_request` telling goes with it"
    );
}

#[tokio::test]
async fn an_undo_of_another_actors_follow_withdraws_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let alice = common::create_local_user(&pool, "fw-undo-alice").await;
    deliver(
        &state,
        &follow_activity("fw-follower-1", BOB, &alice.ap_id),
        &bob,
    )
    .await;

    // Mallory names bob's Follow by its id, and again with an inner pair:
    // neither is hers to withdraw.
    deliver(
        &state,
        &answer(
            "Undo",
            "fw-follower-2",
            MALLORY,
            &json!(follow_id("fw-follower-1", BOB)),
        ),
        &mallory,
    )
    .await;
    deliver(
        &state,
        &answer(
            "Undo",
            "fw-follower-3",
            MALLORY,
            &json!({"type": "Follow", "object": alice.ap_id}),
        ),
        &mallory,
    )
    .await;

    assert_eq!(
        relation(&pool, &bob, &alice).await.state,
        follow::FollowState::Accepted,
        "bob's follow stands through both"
    );
    assert_eq!(
        reload_user(&pool, alice.id).await.followers_count,
        1,
        "and so does its count"
    );
}

// --- the duplicate ----------------------------------------------------------------------

#[tokio::test]
async fn a_duplicate_follow_updates_the_id_and_resends_the_answer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let alice = common::create_local_user(&pool, "fd-alice").await;

    deliver(&state, &follow_activity("fd-1", BOB, &alice.ap_id), &bob).await;
    deliver(&state, &follow_activity("fd-2", BOB, &alice.ap_id), &bob).await;

    let row = relation(&pool, &bob, &alice).await;
    assert_eq!(follow_count(&pool).await, 1, "the pair keeps one row");
    assert_eq!(
        row.ap_id.as_deref(),
        Some(follow_id("fd-2", BOB).as_str()),
        "the stored id moves to the newest Follow"
    );
    assert_eq!(
        reload_user(&pool, alice.id).await.followers_count,
        1,
        "the restated grant counts once"
    );
    assert_eq!(
        told(&pool, alice.id).await,
        vec![("follow".to_owned(), bob.id, None)],
        "and tells once"
    );

    let jobs = delivery_jobs(&pool).await;
    assert_eq!(jobs.len(), 2, "the accepted pair is answered again");
    assert_eq!(
        jobs[0]["body"]["object"]["id"],
        json!(follow_id("fd-1", BOB)),
        "the first Accept answered the first Follow"
    );
    assert_eq!(
        jobs[1]["body"]["object"]["id"],
        json!(follow_id("fd-2", BOB)),
        "and the second answers the newest"
    );
    assert_ne!(
        jobs[0]["body"]["id"], jobs[1]["body"]["id"],
        "each Accept is its own activity"
    );
}

#[tokio::test]
async fn a_duplicate_request_updates_the_id_and_stays_quiet() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = delivering_state(&pool);
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let carol = common::create_local_user_with(&pool, "fd-locked-carol", |u| NewUser {
        locked: Flag::new(true),
        ..u
    })
    .await;

    deliver(
        &state,
        &follow_activity("fd-locked-1", BOB, &carol.ap_id),
        &bob,
    )
    .await;
    deliver(
        &state,
        &follow_activity("fd-locked-2", BOB, &carol.ap_id),
        &bob,
    )
    .await;

    let row = relation(&pool, &bob, &carol).await;
    assert_eq!(
        row.state,
        follow::FollowState::Pending,
        "the re-send does not grant itself"
    );
    assert_eq!(
        row.ap_id.as_deref(),
        Some(follow_id("fd-locked-2", BOB).as_str()),
        "the stored id still moves to the newest Follow"
    );
    assert_eq!(
        told(&pool, carol.id).await,
        vec![("follow_request".to_owned(), bob.id, None)],
        "one request, one telling"
    );
    assert_eq!(job_count(&pool).await, 0, "and still nothing sent");
}
