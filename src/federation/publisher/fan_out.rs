//! Tests of [`publish`]'s fan-out through the database: who a public note,
//! a mention and a direct message reach, which rows and hosts are
//! excluded, and how repeated inboxes and repeated publishes collapse.

use super::test_support::*;
use super::*;
use crate::{
    config::{Config, TEST_BASE_URL},
    models::user::{NewUser, insert, local_ap_id},
    services::{instance_actor::ensure_instance_actor, runtime_config},
};

/// A public note fans out to the followers' remote inboxes, and two
/// followers sharing one instance share one delivery.
#[tokio::test]
async fn a_public_note_fans_out_to_the_remote_followers() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    let carol = mirror(
        &pool,
        "carol",
        "peer.example",
        "https://peer.example/users/carol/inbox",
        Some("https://peer.example/inbox"),
    )
    .await;
    let dave = mirror(
        &pool,
        "dave",
        "peer.example",
        "https://peer.example/users/dave/inbox",
        Some("https://peer.example/inbox"),
    )
    .await;
    let erin = mirror(
        &pool,
        "erin",
        "other.example",
        "https://other.example/users/erin/inbox",
        None,
    )
    .await;
    let frank = mirror(
        &pool,
        "frank",
        "third.example",
        "https://third.example/inbox",
        None,
    )
    .await;
    for follower in [&carol, &dave, &erin, &frank] {
        followed(&pool, follower, &alice).await;
    }
    let state = test_state(&pool);

    let count = publish_ok(
        &state,
        &alice,
        &public_activity(&alice),
        PublishOpts::default(),
    )
    .await;

    assert_eq!(count, 3, "three hosts, three deliveries");
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![
            "https://other.example/users/erin/inbox",
            "https://peer.example/inbox",
            "https://third.example/inbox",
        ],
        "the two peer.example followers share their instance's POST"
    );
}

/// A directly addressed remote user is reached at their personal
/// inbox even when they publish a shared one.
#[tokio::test]
async fn a_mentioned_remote_user_gets_their_personal_inbox() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    let bob = mirror(
        &pool,
        "bob",
        "peer.example",
        "https://peer.example/users/bob/inbox",
        Some("https://peer.example/inbox"),
    )
    .await;
    let state = test_state(&pool);
    let activity = json!({
        "id": ACTIVITY_ID,
        "type": "Create",
        "actor": alice.ap_id,
        "to": [bob.ap_id, PUBLIC],
        "cc": [alice.followers_url],
    });

    let count = publish_ok(&state, &alice, &activity, PublishOpts::default()).await;

    assert_eq!(count, 1, "the mention is the only resolved recipient");
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec!["https://peer.example/users/bob/inbox"],
        "a direct address delivers to the personal inbox, not the shared one"
    );
}

/// With `only_to`, followers expansion does not happen: the named
/// recipients alone receive the activity.
#[tokio::test]
async fn a_direct_message_goes_only_to_the_mentioned() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    let carol = mirror(
        &pool,
        "carol",
        "peer.example",
        "https://peer.example/users/carol/inbox",
        Some("https://peer.example/inbox"),
    )
    .await;
    followed(&pool, &carol, &alice).await;
    let bob = mirror(
        &pool,
        "bob",
        "other.example",
        "https://other.example/users/bob/inbox",
        None,
    )
    .await;
    let state = test_state(&pool);
    let opts = PublishOpts {
        extra_inboxes: Vec::new(),
        only_to: Some(vec![bob.ap_id.clone()]),
    };

    let count = publish_ok(&state, &alice, &public_activity(&alice), opts).await;

    assert_eq!(count, 1, "the addressed follower set is replaced wholesale");
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec!["https://other.example/users/bob/inbox"],
        "only the mentioned actor receives the direct message"
    );
}

/// Local followers are never delivery targets — their timelines are
/// the delivery.
#[tokio::test]
async fn local_followers_never_receive_a_delivery() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    let local_bob = insert(
        &pool,
        &NewUser {
            inbox: Some(format!("{TEST_BASE_URL}/users/bob/inbox")),
            followers_url: Some(format!("{TEST_BASE_URL}/users/bob/followers")),
            ..NewUser::local("bob", &local_ap_id(TEST_BASE_URL, "bob"))
        },
    )
    .await
    .unwrap();
    followed(&pool, &local_bob, &alice).await;
    let remote = mirror(
        &pool,
        "carol",
        "peer.example",
        "https://peer.example/users/carol/inbox",
        None,
    )
    .await;
    followed(&pool, &remote, &alice).await;
    let state = test_state(&pool);

    let count = publish_ok(
        &state,
        &alice,
        &public_activity(&alice),
        PublishOpts::default(),
    )
    .await;

    assert_eq!(count, 1, "only the remote follower is a target");
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec!["https://peer.example/users/carol/inbox"],
        "no delivery is queued to this instance's own origin"
    );
}

/// A follower on a policy-rejected domain drops out of the set.
#[tokio::test]
async fn a_rejected_domain_is_excluded() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    policy_reject(&pool, "spam.example").await;
    let spammer = mirror(
        &pool,
        "spammer",
        "spam.example",
        "https://spam.example/inbox",
        None,
    )
    .await;
    followed(&pool, &spammer, &alice).await;
    let clean = mirror(
        &pool,
        "erin",
        "other.example",
        "https://other.example/users/erin/inbox",
        None,
    )
    .await;
    followed(&pool, &clean, &alice).await;
    let state = test_state(&pool);

    let count = publish_ok(
        &state,
        &alice,
        &public_activity(&alice),
        PublishOpts::default(),
    )
    .await;

    assert_eq!(count, 1, "the rejected host is dropped from the set");
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec!["https://other.example/users/erin/inbox"],
        "only the reachable follower receives a job"
    );
}

/// The per-inbox enqueue repeats no policy check, so the publish-level filter
/// is the only thing keeping a rejected host out: an extra inbox on a
/// rejected host (no follower row involved) must still queue nothing.
#[tokio::test]
async fn a_rejected_extra_inbox_is_never_enqueued() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    policy_reject(&pool, "spam.example").await;
    let state = test_state(&pool);

    let count = publish_ok(
        &state,
        &alice,
        &public_activity(&alice),
        PublishOpts {
            extra_inboxes: vec![
                "https://spam.example/inbox".to_owned(),
                "https://other.example/inbox".to_owned(),
            ],
            only_to: None,
        },
    )
    .await;

    assert_eq!(count, 1, "only the unrejected extra inbox is a target");
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec!["https://other.example/inbox"],
        "the rejected host never gets a job"
    );
}

/// A follower whose host the MRF domain lists reject is dropped the
/// same way: allowlist mode means this instance fans out only to the
/// hosts it chose, and the enqueue would refuse the others anyway.
#[tokio::test]
async fn a_domain_the_mrf_lists_reject_is_excluded() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    runtime_config::write(
        &pool,
        ":pleroma",
        ":mrf",
        &json!({
            "policies": ["simple"],
            "simple": {"accept": [["other.example", "the one we talk to"]]},
        }),
    )
    .await
    .unwrap();
    let alice = alice(&pool).await;
    let stranger = mirror(
        &pool,
        "stranger",
        "spam.example",
        "https://spam.example/inbox",
        None,
    )
    .await;
    followed(&pool, &stranger, &alice).await;
    let erin = mirror(
        &pool,
        "erin",
        "other.example",
        "https://other.example/users/erin/inbox",
        None,
    )
    .await;
    followed(&pool, &erin, &alice).await;
    let state = test_state(&pool);

    let count = publish_ok(
        &state,
        &alice,
        &public_activity(&alice),
        PublishOpts::default(),
    )
    .await;

    assert_eq!(count, 1, "the allowlisted host is the only target");
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec!["https://other.example/users/erin/inbox"],
        "the local publisher itself still federates: the chain exempts its own domain"
    );
}

/// The same inbox reached by three different routes — as a follower's
/// inbox, as a mention's inbox and as an extra — costs one delivery.
#[tokio::test]
async fn one_inbox_is_enqueued_once() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    let personal = "https://peer.example/users/erin/inbox";
    let erin = mirror(&pool, "erin", "peer.example", personal, None).await;
    followed(&pool, &erin, &alice).await;
    let state = test_state(&pool);
    let activity = json!({
        "id": ACTIVITY_ID,
        "type": "Create",
        "actor": alice.ap_id,
        "to": [erin.ap_id, PUBLIC],
        "cc": [alice.followers_url],
    });
    let opts = PublishOpts {
        extra_inboxes: vec![personal.to_owned(), personal.to_owned()],
        only_to: None,
    };

    let count = publish_ok(&state, &alice, &activity, opts).await;

    assert_eq!(count, 1, "mention, follower and extra collapse to one");
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![personal],
        "and the queue holds one job for it"
    );
}

/// The instance actor and the publishing actor are never recipients,
/// and neither is a deactivated mirror.
#[tokio::test]
async fn the_instance_actor_the_actor_and_the_suspended_are_excluded() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let config = Config::for_tests();
    let instance_actor = ensure_instance_actor(&pool, &config).await.unwrap();
    let alice = alice(&pool).await;
    let gone = mirror(
        &pool,
        "gone",
        "other.example",
        "https://other.example/users/gone/inbox",
        None,
    )
    .await;
    followed(&pool, &gone, &alice).await;
    deactivate(&pool, gone.id).await;
    let live = mirror(
        &pool,
        "erin",
        "other.example",
        "https://other.example/users/erin/inbox",
        None,
    )
    .await;
    let state = test_state(&pool);
    let activity = json!({
        "id": ACTIVITY_ID,
        "type": "Create",
        "actor": alice.ap_id,
        "to": [
            instance_actor.ap_id,
            alice.ap_id,
            live.ap_id,
            PUBLIC,
        ],
        "cc": [alice.followers_url],
    });

    let count = publish_ok(&state, &alice, &activity, PublishOpts::default()).await;

    assert_eq!(count, 1, "only the live remote mention remains");
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec!["https://other.example/users/erin/inbox"],
        "the instance actor, the actor and the deactivated mirror are skipped"
    );
}

/// Unknown addresses contribute nothing; an activity addressed only to
/// the public, from an actor with no followers, enqueues nothing.
#[tokio::test]
async fn unknown_recipients_are_skipped() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    let state = test_state(&pool);
    let activity = json!({
        "id": ACTIVITY_ID,
        "type": "Create",
        "actor": alice.ap_id,
        "to": [PUBLIC, "https://ghost.example/users/nobody"],
        "cc": [alice.followers_url],
    });

    let count = publish_ok(&state, &alice, &activity, PublishOpts::default()).await;

    assert_eq!(count, 0, "a public activity with nobody behind it");
    assert_eq!(job_count(&pool).await, 0, "and no job for anyone");
}

/// A second publish of the same activity finds the queue's per-pair
/// deduplication: the job count stays at one per inbox.
#[tokio::test]
async fn a_repeated_publish_reuses_the_queued_delivery() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    let erin = mirror(
        &pool,
        "erin",
        "other.example",
        "https://other.example/users/erin/inbox",
        None,
    )
    .await;
    followed(&pool, &erin, &alice).await;
    let state = test_state(&pool);
    let activity = public_activity(&alice);

    let first = publish_ok(&state, &alice, &activity, PublishOpts::default()).await;
    let second = publish_ok(&state, &alice, &activity, PublishOpts::default()).await;

    assert_eq!((first, second), (1, 1), "both calls compute one target");
    assert_eq!(
        job_count(&pool).await,
        1,
        "the delivery's unique key swallows the repeat"
    );
}
