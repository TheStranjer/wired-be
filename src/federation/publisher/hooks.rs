//! Tests of the conditions that change a publish: federation switched off,
//! the relay hook, the unreachable-host hook and which of an actor's own
//! self-broadcasts reach relays (the public ones only).

use chrono::{Duration, Utc};
use sqlx::PgPool;

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
    let state = app_state(&pool);

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

    // A non-public activity of the same actor — no public address —
    // never carries the relay's inbox.
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
    let state = app_state(&pool);

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
        "cc": [followers_of(&alice)],
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

/// Subscribes to and accepts the fixture relay, so its inbox is one the
/// publisher adds to a relay-eligible (public) fan-out.
async fn accepted_relay(pool: &PgPool) {
    let _subscription = relay::upsert(pool, "https://relay.example/relay", RELAY_INBOX)
        .await
        .unwrap();
    let _granted = relay::mark_accepted(pool, "https://relay.example/relay")
        .await
        .unwrap();
}

/// An actor's own `Update` addressed to its followers (an edit of a
/// followers-only note) reaches the followers and never the accepted
/// relay: the body of a private post must not leave through a relay.
#[tokio::test]
async fn a_followers_only_update_stays_off_the_relay() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    let erin = mirror(&pool, "erin", "other.example", ERIN_INBOX, None).await;
    followed(&pool, &erin, &alice).await;
    accepted_relay(&pool).await;
    let state = app_state(&pool);
    let update = json!({
        "id": ACTIVITY_ID,
        "type": "Update",
        "actor": alice.ap_id,
        "object": {"type": "Note", "id": "https://test.example/objects/1", "content": "secret"},
        "to": [followers_of(&alice)],
    });

    assert_eq!(
        publish_default(&state, &alice, &update).await,
        1,
        "the Update fans out to the follower alone"
    );
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![ERIN_INBOX],
        "and no delivery job names the relay's inbox"
    );
}

/// An `Undo` of a block, addressed to the one remote actor it concerns,
/// reaches that actor and not the relay.
#[tokio::test]
async fn an_undo_block_stays_off_the_relay() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    let erin = mirror(&pool, "erin", "other.example", ERIN_INBOX, None).await;
    accepted_relay(&pool).await;
    let state = app_state(&pool);
    let undo = json!({
        "id": ACTIVITY_ID,
        "type": "Undo",
        "actor": alice.ap_id,
        "object": {"type": "Block", "actor": alice.ap_id, "object": erin.ap_id},
        "to": [erin.ap_id],
    });

    assert_eq!(
        publish_default(&state, &alice, &undo).await,
        1,
        "the Undo goes to the unblocked actor alone"
    );
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![ERIN_INBOX],
        "the target's inbox gets its job and the relay's inbox gets none"
    );
}

/// An `Undo` of a follow published with `only_to` reaches the named
/// actor and never the relay, whatever the document's own addressing.
#[tokio::test]
async fn an_undo_follow_with_only_to_stays_off_the_relay() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    let erin = mirror(&pool, "erin", "other.example", ERIN_INBOX, None).await;
    accepted_relay(&pool).await;
    let state = app_state(&pool);
    let undo = json!({
        "id": ACTIVITY_ID,
        "type": "Undo",
        "actor": alice.ap_id,
        "object": {"type": "Follow", "actor": alice.ap_id, "object": erin.ap_id},
        "to": [erin.ap_id],
    });
    let opts = PublishOpts {
        only_to: Some(vec![erin.ap_id.clone()]),
        ..PublishOpts::default()
    };

    assert_eq!(
        publish_ok(&state, &alice, &undo, opts).await,
        1,
        "the Undo goes to the unfollowed actor alone"
    );
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![ERIN_INBOX],
        "and no job names the relay's inbox"
    );
}

/// `only_to` closes the relay arm even when the list names the public
/// address: the caller narrowed the audience.
#[tokio::test]
async fn only_to_naming_the_public_still_skips_the_relay() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    let erin = mirror(&pool, "erin", "other.example", ERIN_INBOX, None).await;
    accepted_relay(&pool).await;
    let state = app_state(&pool);
    let opts = PublishOpts {
        only_to: Some(vec![PUBLIC.to_owned(), erin.ap_id.clone()]),
        ..PublishOpts::default()
    };

    assert_eq!(
        publish_ok(&state, &alice, &public_activity(&alice), opts).await,
        1,
        "only the named actor is a target"
    );
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![ERIN_INBOX],
        "the relay's inbox is left out"
    );
}

/// Regression: the `Delete` of a public note carries the public address,
/// so the accepted relay still gets it beside the follower.
#[tokio::test]
async fn a_public_delete_still_reaches_the_relay() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    let erin = mirror(&pool, "erin", "other.example", ERIN_INBOX, None).await;
    followed(&pool, &erin, &alice).await;
    accepted_relay(&pool).await;
    let state = app_state(&pool);
    let delete = json!({
        "id": ACTIVITY_ID,
        "type": "Delete",
        "actor": alice.ap_id,
        "object": "https://test.example/objects/1",
        "to": [PUBLIC],
        "cc": [followers_of(&alice)],
    });

    assert_eq!(
        publish_default(&state, &alice, &delete).await,
        2,
        "the follower and the relay are both targets"
    );
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![ERIN_INBOX, RELAY_INBOX],
        "the relay's inbox is queued"
    );
}

/// Regression: the profile `Update` is addressed to the public, so the
/// accepted relay still gets it.
#[tokio::test]
async fn a_public_profile_update_still_reaches_the_relay() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    accepted_relay(&pool).await;
    let state = app_state(&pool);
    let update = json!({
        "id": ACTIVITY_ID,
        "type": "Update",
        "actor": alice.ap_id,
        "object": alice.ap_id,
        "to": [PUBLIC],
    });

    assert_eq!(
        publish_default(&state, &alice, &update).await,
        1,
        "the relay is the one target"
    );
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![RELAY_INBOX],
        "and its inbox is queued"
    );
}

/// An unlisted `Create` (`to` the followers, the public address only in
/// `cc`) reaches the follower and never the accepted relay: unlisted means
/// off the federated timelines a relay would feed.
#[tokio::test]
async fn an_unlisted_create_stays_off_the_relay() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    let erin = mirror(&pool, "erin", "other.example", ERIN_INBOX, None).await;
    followed(&pool, &erin, &alice).await;
    accepted_relay(&pool).await;
    let state = app_state(&pool);
    let create = json!({
        "id": ACTIVITY_ID,
        "type": "Create",
        "actor": alice.ap_id,
        "object": {"type": "Note", "id": "https://test.example/objects/1", "content": "hi"},
        "to": [followers_of(&alice)],
        "cc": [PUBLIC],
    });

    assert_eq!(
        publish_default(&state, &alice, &create).await,
        1,
        "the unlisted Create fans out to the follower alone"
    );
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![ERIN_INBOX],
        "and no delivery job names the relay's inbox"
    );
}

/// A public `Create` (`to` the public address) reaches the accepted relay.
#[tokio::test]
async fn a_public_create_reaches_the_relay() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    accepted_relay(&pool).await;
    let state = app_state(&pool);
    let create = json!({
        "id": ACTIVITY_ID,
        "type": "Create",
        "actor": alice.ap_id,
        "object": {"type": "Note", "id": "https://test.example/objects/1", "content": "hi"},
        "to": [PUBLIC],
    });

    assert_eq!(
        publish_default(&state, &alice, &create).await,
        1,
        "the relay is the one target"
    );
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![RELAY_INBOX],
        "and its inbox is queued"
    );
}

/// A public `Like` (`cc` the followers and the public address) reaches the
/// author and the follower but never the relay: relays do not carry likes.
#[tokio::test]
async fn a_public_like_stays_off_the_relay() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    let erin = mirror(&pool, "erin", "other.example", ERIN_INBOX, None).await;
    accepted_relay(&pool).await;
    let state = app_state(&pool);
    let like = json!({
        "id": ACTIVITY_ID,
        "type": "Like",
        "actor": alice.ap_id,
        "object": "https://other.example/objects/1",
        "to": [erin.ap_id],
        "cc": [followers_of(&alice), PUBLIC],
    });

    assert_eq!(
        publish_default(&state, &alice, &like).await,
        1,
        "the Like goes to the author alone"
    );
    assert_eq!(
        delivered_inboxes(&pool).await,
        vec![ERIN_INBOX],
        "and no delivery job names the relay's inbox"
    );
}

/// A `Like` that even names the public address in `to` stays off the
/// relay: the type, not only the addressing, decides.
#[tokio::test]
async fn a_like_to_the_public_stays_off_the_relay() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = alice(&pool).await;
    accepted_relay(&pool).await;
    let state = app_state(&pool);
    let like = json!({
        "id": ACTIVITY_ID,
        "type": "Like",
        "actor": alice.ap_id,
        "object": "https://other.example/objects/1",
        "to": [PUBLIC],
    });

    assert_eq!(
        publish_default(&state, &alice, &like).await,
        0,
        "nobody is a target"
    );
    assert_eq!(
        delivered_inboxes(&pool).await,
        Vec::<String>::new(),
        "and no delivery job is queued"
    );
}
