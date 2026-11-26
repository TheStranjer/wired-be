//! Tests for the follow handshake: the fixtures every group shares, the
//! routing, guard, relay and retry tests, and the refusals as a child
//! module.

mod blocks;
mod refusals;

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::*;
use crate::{
    config::TEST_BASE_URL,
    models::{
        activitypub_actor::ActivityPubActorWrite,
        user::{NewUser, insert},
    },
    test_fixtures::{federating_user, local_user as local, write_actor},
    test_support::{app_state, broken_state},
};

/// The mirror every direct-call fixture here is sent by: alice on
/// `peer.example`.
const ALICE: &str = "https://peer.example/users/alice";
/// MALLORY, the other `peer.example` mirror — the wrong-actor fixture.
const MALLORY: &str = "https://peer.example/users/mallory";

/// The remote mirror fixture: `nickname` on `peer.example`.
async fn mirror(pool: &PgPool, nickname: &str, ap_id: &str) -> User {
    insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@peer.example"),
            ap_id: ap_id.to_owned(),
            domain: Some("peer.example".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the remote fixture inserts")
}

/// The row of user `id` as it stands now, for the next handler call
/// that needs its own `User` (they are moved into the handler).
async fn fresh(pool: &PgPool, id: Uuid) -> User {
    user::get_by_id(pool, id)
        .await
        .expect("the user reads")
        .expect("the user exists")
}

/// The normalized view of `kind`, named `n` on `peer.example`, sent by
/// `actor`, carrying `object`.
fn view(kind: &str, n: &str, actor: &str, object: &Value) -> ActivityView {
    ActivityView::parse(&json!({
        "id": format!("https://peer.example/activities/{n}"),
        "type": kind,
        "actor": actor,
        "object": object,
    }))
    .expect("the fixture activity parses")
}

/// `Follow`, `Accept` and `Reject` route by their own names, and the
/// `Undo` table claims `Follow` — and nothing else of the family.
#[test]
fn register_binds_the_follow_handshake_and_its_withdrawal() {
    let mut handlers = Handlers::new();
    register(&mut handlers);
    for kind in ["Follow", "Accept", "Reject"] {
        assert!(handlers.handles(kind), "a {kind} routes here");
    }
    assert!(
        handlers.handles_undo("Follow"),
        "the withdrawal of a Follow routes here"
    );
    assert!(
        !handlers.handles_undo("Block"),
        "the withdrawal of a Block stays the misc module's key"
    );
}

/// The belt on the embedded inner activity: an inner naming the
/// signer's own actor — or no actor at all — passes; an inner naming a
/// different actor does not. A bare-id delivery has no inner and
/// passes.
#[test]
fn an_inner_follow_passes_the_belt_for_its_own_maker_only() {
    let mine = view(
        "Undo",
        "undo-belt-1",
        ALICE,
        &json!({"type": "Follow", "actor": ALICE, "object": "https://test.example/users/bob"}),
    );
    assert!(
        inner_follow_is_the_signers(&mine, ALICE),
        "the signer's own inner passes"
    );
    let theirs = view(
        "Undo",
        "undo-belt-2",
        ALICE,
        &json!({"type": "Follow", "actor": MALLORY, "object": "https://test.example/users/bob"}),
    );
    assert!(
        !inner_follow_is_the_signers(&theirs, ALICE),
        "an inner made by somebody else does not"
    );
    let actorless = view(
        "Undo",
        "undo-belt-3",
        ALICE,
        &json!({"type": "Follow", "object": "https://test.example/users/bob"}),
    );
    assert!(
        inner_follow_is_the_signers(&actorless, ALICE),
        "an inner naming no actor is the dispatcher's own fallback"
    );
    let bare = view(
        "Undo",
        "undo-belt-4",
        ALICE,
        &json!("https://peer.example/activities/follow-1"),
    );
    assert!(
        inner_follow_is_the_signers(&bare, ALICE),
        "and a bare-id Undo has no inner to check"
    );
}

/// The local-signer guard of all four routes: an activity whose actor
/// this instance owns folds nothing — the federation door is for
/// remote peers, and every local write has its own endpoint.
#[tokio::test]
async fn every_route_refuses_a_locally_signed_activity() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = local(&pool, "follow-self-alice").await;
    let alice_id = alice.id;
    let alice_ap = alice.ap_id.clone();
    let bob = local(&pool, "follow-self-bob").await;

    let followed = handle_follow(
        state.clone(),
        view("Follow", "self-1", &alice_ap, &json!(bob.ap_id.clone())),
        alice,
    )
    .await;
    let accepted = handle_accept(
        state.clone(),
        view("Accept", "self-2", &alice_ap, &json!(bob.ap_id.clone())),
        fresh(&pool, alice_id).await,
    )
    .await;
    let rejected = handle_reject(
        state.clone(),
        view("Reject", "self-3", &alice_ap, &json!(bob.ap_id.clone())),
        fresh(&pool, alice_id).await,
    )
    .await;
    let undone = handle_undo_follow(
        state,
        view(
            "Undo",
            "self-4",
            &alice_ap,
            &json!({"type": "Follow", "actor": &alice_ap, "object": bob.ap_id.clone()}),
        ),
        fresh(&pool, alice_id).await,
    )
    .await;
    for outcome in [followed, accepted, rejected, undone] {
        assert!(
            matches!(&outcome, Ok(Outcome::Ignored(reason)) if reason.contains("this instance owns")),
            "the local signer is every route's own refusal: {outcome:?}"
        );
    }
    assert!(
        follow::get(&pool, alice_id, bob.id)
            .await
            .unwrap()
            .is_none(),
        "and nothing was folded"
    );
}

/// The relay actor every relay fixture here answers as.
const RELAY_AP: &str = "https://peer.example/relay";
/// Its inbox, the fan-out address the ledger carries.
const RELAY_INBOX: &str = "https://peer.example/relay/inbox";

/// The relay's `Accept` or `Reject` (`kind`, named `n`) of the follow
/// `follower_ap_id` made of the relay, the inner `Follow` embedded whole.
fn relay_answer(kind: &str, n: &str, follower_ap_id: &str) -> ActivityView {
    view(
        kind,
        n,
        RELAY_AP,
        &json!({"type": "Follow", "actor": follower_ap_id, "object": RELAY_AP}),
    )
}

/// A relay mirror whose subscription row stands `pending` in the
/// ledger, plus the instance actor that would follow it.
async fn relay_fixture(pool: &PgPool, state: &AppState) -> (User, User) {
    let relay_user = mirror(pool, "relay", RELAY_AP).await;
    let _row = relay::upsert(pool, RELAY_AP, RELAY_INBOX)
        .await
        .expect("the subscription row stores");
    let instance = crate::services::instance_actor::ensure_instance_actor(pool, &state.config)
        .await
        .expect("the instance actor provisions");
    (relay_user, instance)
}

/// A local user's follow of the relay actor, once the relay accepts it,
/// is a granted follow and nothing more: the instance's subscription
/// stays `pending` and the relay's inbox joins no fan-out.
#[tokio::test]
async fn an_accept_of_a_local_users_follow_of_a_relay_leaves_the_subscription_pending() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let (relay_user, _instance) = relay_fixture(&pool, &state).await;
    let alice = local(&pool, "relay-accept-alice").await;
    let relay_id = relay_user.id;
    let _row = follow::follow(&pool, TEST_BASE_URL, &alice, &relay_user)
        .await
        .expect("the local follow records");

    let outcome = handle_accept(
        state,
        relay_answer("Accept", "relay-accept-local", &alice.ap_id),
        relay_user,
    )
    .await;

    assert!(matches!(outcome, Ok(Outcome::Handled)), "{outcome:?}");
    let granted = follow::get(&pool, alice.id, relay_id)
        .await
        .unwrap()
        .expect("the follow stands");
    assert_eq!(
        granted.state,
        FollowState::Accepted,
        "the follow is granted"
    );
    let subscription = relay::get(&pool, RELAY_AP).await.unwrap().expect("a row");
    assert_eq!(
        subscription.state,
        relay::RelayState::Pending,
        "but the instance's subscription was never answered"
    );
    assert!(
        relay::accepted_inboxes(&pool).await.unwrap().is_empty(),
        "so the relay's inbox is no fan-out target"
    );
}

/// The relay's `Accept` of the instance actor's own follow turns the
/// subscription accepted and opens the relay's inbox to the fan-out.
#[tokio::test]
async fn an_accept_of_the_instance_actors_follow_marks_the_relay_accepted() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let (relay_user, instance) = relay_fixture(&pool, &state).await;
    let relay_id = relay_user.id;
    let _row = follow::follow(&pool, TEST_BASE_URL, &instance, &relay_user)
        .await
        .expect("the instance actor's follow records");

    let outcome = handle_accept(
        state,
        relay_answer("Accept", "relay-accept-instance", &instance.ap_id),
        relay_user,
    )
    .await;

    assert!(matches!(outcome, Ok(Outcome::Handled)), "{outcome:?}");
    let granted = follow::get(&pool, instance.id, relay_id)
        .await
        .unwrap()
        .expect("the follow stands");
    assert_eq!(
        granted.state,
        FollowState::Accepted,
        "the follow is granted"
    );
    assert!(
        relay::is_accepted(&pool, RELAY_AP).await.unwrap(),
        "the subscription stands accepted"
    );
    assert_eq!(
        relay::accepted_inboxes(&pool).await.unwrap(),
        vec![RELAY_INBOX.to_owned()],
        "and the relay's inbox is a fan-out target"
    );
}

/// A relay's `Reject` of the instance actor's follow revokes the
/// subscription: the follow is gone, the ledger row is `pending` again
/// and the relay's inbox leaves the fan-out.
#[tokio::test]
async fn a_reject_of_the_instance_actors_relay_follow_takes_the_relay_out_of_fan_out() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let (relay_user, instance) = relay_fixture(&pool, &state).await;
    let relay_id = relay_user.id;
    let _row = follow::follow(&pool, TEST_BASE_URL, &instance, &relay_user)
        .await
        .expect("the instance actor's follow records");
    let _granted = relay::mark_accepted(&pool, RELAY_AP).await.unwrap();
    assert_eq!(
        relay::accepted_inboxes(&pool).await.unwrap().len(),
        1,
        "the relay starts as a fan-out target"
    );

    let outcome = handle_reject(
        state,
        relay_answer("Reject", "relay-reject-instance", &instance.ap_id),
        relay_user,
    )
    .await;

    assert!(matches!(outcome, Ok(Outcome::Handled)), "{outcome:?}");
    assert!(
        follow::get(&pool, instance.id, relay_id)
            .await
            .unwrap()
            .is_none(),
        "the follow row is removed"
    );
    assert!(
        relay::accepted_inboxes(&pool).await.unwrap().is_empty(),
        "the relay's inbox leaves the fan-out"
    );
    assert!(
        !relay::is_accepted(&pool, RELAY_AP).await.unwrap(),
        "and its Announces no longer fold"
    );
    let subscription = relay::get(&pool, RELAY_AP).await.unwrap().expect("a row");
    assert_eq!(subscription.state, relay::RelayState::Pending);
}

/// A relay's `Reject` of a *local user's* follow of the relay actor is
/// no revocation of the instance's subscription: the accepted relay
/// keeps its place in the fan-out.
#[tokio::test]
async fn a_reject_of_a_local_users_follow_of_a_relay_leaves_the_subscription_alone() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let (relay_user, _instance) = relay_fixture(&pool, &state).await;
    let alice = local(&pool, "relay-reject-alice").await;
    let _row = follow::follow(&pool, TEST_BASE_URL, &alice, &relay_user)
        .await
        .expect("the local follow records");
    let _granted = relay::mark_accepted(&pool, RELAY_AP).await.unwrap();

    let outcome = handle_reject(
        state,
        relay_answer("Reject", "relay-reject-local", &alice.ap_id),
        relay_user,
    )
    .await;

    assert!(matches!(outcome, Ok(Outcome::Handled)), "{outcome:?}");
    assert_eq!(
        relay::accepted_inboxes(&pool).await.unwrap(),
        vec![RELAY_INBOX.to_owned()],
        "the instance's accepted subscription stands"
    );
}

/// Every route against a database that cannot answer: the first read
/// fails, the failure is transient, and the queue's retry finds the
/// same idempotent writes waiting for it.
#[tokio::test]
async fn a_dead_database_sends_every_route_back_for_a_retry() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = mirror(&pool, "dead-alice", ALICE).await;
    let alice_id = alice.id;
    let bob = local(&pool, "dead-bob").await;
    let state = broken_state().await;

    let outcomes = [
        handle_follow(
            state.clone(),
            view("Follow", "dead-1", ALICE, &json!(bob.ap_id.clone())),
            alice,
        )
        .await,
        handle_accept(
            state.clone(),
            view(
                "Accept",
                "dead-2",
                ALICE,
                &json!("https://peer.example/activities/none"),
            ),
            fresh(&pool, alice_id).await,
        )
        .await,
        handle_reject(
            state.clone(),
            view(
                "Reject",
                "dead-3",
                ALICE,
                &json!("https://peer.example/activities/none"),
            ),
            fresh(&pool, alice_id).await,
        )
        .await,
        handle_undo_follow(
            state,
            view(
                "Undo",
                "dead-4",
                ALICE,
                &json!("https://peer.example/activities/none"),
            ),
            fresh(&pool, alice_id).await,
        )
        .await,
    ];
    for outcome in outcomes {
        assert!(
            matches!(outcome, Err(FederationError::Transient(_))),
            "the dead database is a retry, not a verdict: {outcome:?}"
        );
    }
}
