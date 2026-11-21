//! Tests of the conditions that change a publish: federation switched off,
//! the relay hook, the unreachable-host hook and an actor's own
//! self-broadcasts.

use chrono::{Duration, Utc};

use super::test_support::*;
use super::*;
use crate::{
    config::{Config, Toggle},
    models::relay,
};

/// The personal inbox the `other.example` mirror publishes.
const ERIN_INBOX: &str = "https://other.example/users/erin/inbox";
/// The inbox the relay fixtures publish.
const RELAY_INBOX: &str = "https://relay.example/inbox";
/// The id of the second activity a test publishes, so a re-publish of the
/// first one's inbox cannot be swallowed by the delivery's deduplication.
const CORRECTION_ID: &str = "https://test.example/activities/2";
/// The grace window [`Config::for_tests`] carries: zero seconds, so the
/// publisher skips a host the moment the ledger marks it.
const NO_GRACE: Duration = Duration::zero();
/// How far in the past a test's mark goes, so the comparison with the
/// database's clock is settled rather than a race.
const ONE_MINUTE_AGO: Duration = Duration::minutes(1);

/// With federation disabled nothing is computed or queued.
#[tokio::test]
async fn a_disabled_federation_publishes_nothing() {
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
    let mut config = Config::for_tests();
    config.federation_enabled = Toggle::new(false);
    let state = state_with(&pool, config);

    let count = publish_ok(
        &state,
        &alice,
        &public_activity(&alice),
        PublishOpts::default(),
    )
    .await;

    assert_eq!(count, 0, "an instance that does not federate sends nothing");
    assert_eq!(job_count(&pool).await, 0, "and queues nothing");
}

/// The relay hook answers the `relays` table: an accepted relay's
/// inbox joins the fan-out of a public activity, a `pending`
/// subscription stays out of it, and a non-public activity of the
/// same actor carries the relay's inbox nowhere.
#[tokio::test]
async fn the_relay_hook_adds_the_accepted_inboxes_to_the_fan_out() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    assert!(
        instance::relay_inboxes(&pool).await.unwrap().is_empty(),
        "no relay is subscribed until one is followed and accepted"
    );

    let alice = alice(&pool).await;
    let erin = mirror(&pool, "erin", "other.example", ERIN_INBOX, None).await;
    followed(&pool, &erin, &alice).await;
    let _subscription = relay::upsert(&pool, "https://relay.example/relay", RELAY_INBOX)
        .await
        .unwrap();
    let state = test_state(&pool);

    // The relay stands subscribed but unaccepted: a public activity
    // reaches the follower alone until the relay's `Accept` folds.
    let pending_targets = publish_default(&state, &alice, &public_activity(&alice)).await;
    assert_eq!(pending_targets, 1, "the pending relay is not a target");
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![ERIN_INBOX],
        "and it is queued nothing"
    );

    let _granted = relay::mark_accepted(&pool, "https://relay.example/relay")
        .await
        .unwrap();
    let granted_targets = publish_default(&state, &alice, &public_activity(&alice)).await;
    assert_eq!(
        granted_targets, 2,
        "the accepted relay's inbox joins the public fan-out"
    );
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![ERIN_INBOX, RELAY_INBOX],
        "the queue holds the follower's delivery and the relay's"
    );

    // A non-public activity of the same actor — no public address, no
    // self-broadcast type — never carries the relay's inbox.
    let private = json!({
        "id": CORRECTION_ID,
        "type": "Create",
        "actor": alice.ap_id,
        "to": [erin.ap_id],
    });
    let quiet_targets = publish_default(&state, &alice, &private).await;
    assert_eq!(
        quiet_targets, 1,
        "the private activity fans out to the addressed recipient alone"
    );
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![ERIN_INBOX, ERIN_INBOX, RELAY_INBOX],
        "and the relay gets no copy of it"
    );
}

/// The unreachable hook answers the `instances` ledger: with no row
/// for a host its inboxes are targets, a mark under the default zero
/// grace drops them out, and clearing the mark targets them again.
#[tokio::test]
async fn the_unreachable_hook_drops_and_restores_a_marked_host() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    assert!(
        !instance::is_host_unreachable(&pool, "peer.example", NO_GRACE)
            .await
            .unwrap(),
        "a host with no instances row is reachable"
    );

    let alice = alice(&pool).await;
    let erin = mirror(&pool, "erin", "other.example", ERIN_INBOX, None).await;
    followed(&pool, &erin, &alice).await;
    let state = test_state(&pool);

    let first_targets = publish_default(&state, &alice, &public_activity(&alice)).await;
    assert_eq!(first_targets, 1, "the follower's host is a target at first");
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![ERIN_INBOX],
        "and it is queued"
    );

    // A second activity: the queue's per-(activity, inbox) deduplication
    // would otherwise hide whether the skipped fan-out found a target.
    let correction = json!({
        "id": CORRECTION_ID,
        "type": "Create",
        "actor": alice.ap_id,
        "to": [PUBLIC],
        "cc": [alice.followers_url],
    });
    instance::set_unreachable(&pool, "other.example", Utc::now() - ONE_MINUTE_AGO)
        .await
        .unwrap();
    let marked_targets = publish_default(&state, &alice, &correction).await;
    assert_eq!(
        marked_targets, 0,
        "the marked host drops out of the target set"
    );
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![ERIN_INBOX],
        "and nothing new is queued for it"
    );

    instance::set_reachable(&pool, "other.example")
        .await
        .unwrap();
    let recovered_targets = publish_default(&state, &alice, &correction).await;
    assert_eq!(
        recovered_targets, 1,
        "and the cleared host is a target again"
    );
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![ERIN_INBOX, ERIN_INBOX],
        "queued once more, for the activity the skip missed"
    );
}

/// The self-broadcast arm runs the relay hook without any subscription
/// standing: the actor's own `Update` consults the `relays` table, finds
/// no accepted relay, and reaches its followers only.
#[tokio::test]
async fn an_actor_self_broadcast_runs_the_relay_arm() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    let erin = mirror(&pool, "erin", "other.example", ERIN_INBOX, None).await;
    followed(&pool, &erin, &alice).await;
    let state = test_state(&pool);
    let update = json!({
        "id": ACTIVITY_ID,
        "type": "Update",
        "actor": alice.ap_id,
        "object": alice.ap_id,
        "to": [alice.followers_url],
    });

    assert_eq!(
        publish_default(&state, &alice, &update).await,
        1,
        "the actor's Update fans out to followers and consults the empty relay ledger"
    );
}
