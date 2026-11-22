//! Follow, request and mutual-follow flags, and batch ordering.

use uuid::Uuid;

use super::test_support::{
    assert_placeholder_flags, flipped, local_user, locked_user, one_relationship,
    rejected_relation, set_toggles,
};
use super::*;
use crate::{
    config::TEST_BASE_URL,
    models::follow::{self, FollowState},
    util::id::new_id,
};

#[tokio::test]
async fn nothing_recorded_answers_the_default_flags() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;

    let relationship = one_relationship(&pool, &alice, &bob).await;
    assert!(!relationship.following.0, "no row, no follow");
    assert!(!relationship.requested.0, "no row, no request");
    assert!(!relationship.followed_by.0, "no row, no follower");
    assert!(
        !relationship.notifying.0,
        "notifying defaults to false with no row"
    );
    assert!(
        !relationship.subscribing.0,
        "the alias carries the same default"
    );
    assert!(
        relationship.showing_reblogs.0,
        "with no row nothing was ever hidden"
    );
    assert_placeholder_flags(&relationship);
}

#[tokio::test]
async fn an_accepted_follow_computes_following_and_the_toggles() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    let granted = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
        .await
        .unwrap();
    assert_eq!(
        granted.state,
        FollowState::Accepted,
        "an open local account answers on the spot"
    );

    let defaults = one_relationship(&pool, &alice, &bob).await;
    assert!(defaults.following.0, "the accepted row follows");
    assert!(!defaults.requested.0, "and nothing is being asked");
    assert!(
        defaults.showing_reblogs.0,
        "the row's stored show_reblogs default"
    );
    assert!(!defaults.notifying.0, "the row's stored notify default");

    set_toggles(&pool, &alice, &bob, flipped()).await;
    let toggled = one_relationship(&pool, &alice, &bob).await;
    assert!(toggled.following.0, "toggling does not unfollow");
    assert!(
        !toggled.showing_reblogs.0,
        "show_reblogs rides on the viewer's own row"
    );
    assert!(toggled.notifying.0, "so does notify");
    assert!(
        toggled.subscribing.0,
        "and Pleroma's alias carries the same value"
    );
    assert!(
        !toggled.followed_by.0,
        "following one way does not compute the other"
    );
    assert_placeholder_flags(&toggled);
}

#[tokio::test]
async fn a_pending_request_is_requested_but_not_following() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let carol = locked_user(&pool, "carol").await;
    let waiting = follow::follow(&pool, TEST_BASE_URL, &alice, &carol)
        .await
        .unwrap();
    assert_eq!(waiting.state, FollowState::Pending, "the fixture waits");

    let relationship = one_relationship(&pool, &alice, &carol).await;
    assert!(relationship.requested.0, "the ask is visible");
    assert!(!relationship.following.0, "but not granted");
    assert!(!relationship.followed_by.0, "and not in reverse");
    assert!(
        relationship.showing_reblogs.0,
        "a waiting row still carries the show_reblogs default"
    );
    assert!(!relationship.notifying.0, "and the notify default");
    assert_placeholder_flags(&relationship);
}

#[tokio::test]
async fn an_accepted_incoming_follow_computes_followed_by() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    let incoming = follow::follow(&pool, TEST_BASE_URL, &bob, &alice)
        .await
        .unwrap();
    assert_eq!(
        incoming.state,
        FollowState::Accepted,
        "bob's follow of alice is granted"
    );

    let relationship = one_relationship(&pool, &alice, &bob).await;
    assert!(relationship.followed_by.0, "bob follows alice");
    assert!(
        !relationship.following.0,
        "and alice does not follow bob back from that"
    );
    assert!(
        !relationship.requested.0,
        "an incoming follow is not an outgoing request"
    );
    assert!(
        relationship.showing_reblogs.0,
        "the incoming row cannot set alice's own toggles"
    );
    assert!(!relationship.notifying.0, "nor her notify");
    assert_placeholder_flags(&relationship);
}

#[tokio::test]
async fn a_mutual_follow_computes_both_directions() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    let outgoing = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
        .await
        .unwrap();
    assert_eq!(
        outgoing.state,
        FollowState::Accepted,
        "the outgoing leg is granted"
    );
    let incoming = follow::follow(&pool, TEST_BASE_URL, &bob, &alice)
        .await
        .unwrap();
    assert_eq!(
        incoming.state,
        FollowState::Accepted,
        "and so is the incoming one"
    );
    set_toggles(&pool, &alice, &bob, flipped()).await;

    let relationship = one_relationship(&pool, &alice, &bob).await;
    assert!(relationship.following.0, "alice follows bob");
    assert!(relationship.followed_by.0, "and is followed back");
    assert!(!relationship.requested.0, "nothing waits");
    assert!(
        !relationship.showing_reblogs.0,
        "the viewer's own row sets the toggles"
    );
    assert!(relationship.notifying.0, "on both aliases");
    assert!(relationship.subscribing.0, "on both aliases");
    assert_placeholder_flags(&relationship);
}

#[tokio::test]
async fn rejected_rows_compute_no_following_flags() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    rejected_relation(&pool, &alice, &bob).await;
    rejected_relation(&pool, &bob, &alice).await;

    let relationship = one_relationship(&pool, &alice, &bob).await;
    assert!(
        !relationship.following.0,
        "a rejected row is not an accepted follow"
    );
    assert!(!relationship.requested.0, "nor a pending request");
    assert!(
        !relationship.followed_by.0,
        "and the incoming side is not accepted either"
    );
    assert!(
        relationship.showing_reblogs.0,
        "the rejected row only carries column defaults"
    );
    assert_placeholder_flags(&relationship);
}

#[tokio::test]
async fn results_keep_the_requested_order_in_one_query_set() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = local_user(&pool, "bob").await;
    let carol = locked_user(&pool, "carol").await;
    let dave = local_user(&pool, "dave").await;
    let accepted = follow::follow(&pool, TEST_BASE_URL, &alice, &bob)
        .await
        .unwrap();
    assert_eq!(accepted.state, FollowState::Accepted, "alice → bob");
    let pending = follow::follow(&pool, TEST_BASE_URL, &alice, &carol)
        .await
        .unwrap();
    assert_eq!(pending.state, FollowState::Pending, "alice → carol waits");
    let incoming = follow::follow(&pool, TEST_BASE_URL, &dave, &alice)
        .await
        .unwrap();
    assert_eq!(incoming.state, FollowState::Accepted, "dave → alice");
    // An id no user holds: the loader is not the one deciding to skip
    // it (that is the endpoint's job), it just answers the defaults.
    let unknown = new_id();

    // The batch of mixed states below — outgoing, pending, incoming,
    // unknown, and a repeated id — is answered by the fixed query set:
    // `flag_sources` runs one statement per source exactly once per
    // call, and this asserts every row came back right from those two
    // fetches. There is no per-target query loop in the module to
    // count: one call, one `FlagSources`, N answers.
    let targets = [bob.id, carol.id, unknown, dave.id, bob.id];
    let loaded = Relationship::load_many(&pool, alice.id, &targets)
        .await
        .unwrap();
    assert_eq!(loaded.len(), targets.len(), "one answer per requested id");
    let order: Vec<String> = loaded.iter().map(|row| row.id.clone()).collect();
    let wanted: Vec<String> = targets.iter().map(Uuid::to_string).collect();
    assert_eq!(order, wanted, "the requested order, repeats included");

    let flags = |target: Uuid| {
        let row = loaded
            .iter()
            .find(|relationship| relationship.id == target.to_string())
            .expect("the batch answered the target");
        (row.following.0, row.requested.0, row.followed_by.0)
    };
    assert_eq!(flags(bob.id), (true, false, false), "the accepted follow");
    assert_eq!(flags(carol.id), (false, true, false), "the pending request");
    assert_eq!(flags(dave.id), (false, false, true), "the incoming follow");
    assert_eq!(flags(unknown), (false, false, false), "an unknown target");
}

#[tokio::test]
async fn an_empty_batch_touches_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let loaded = Relationship::load_many(&pool, alice.id, &[]).await.unwrap();
    assert!(loaded.is_empty(), "no targets, no relationships");
}
