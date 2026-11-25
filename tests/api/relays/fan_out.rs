//! The publisher's fan-out over the accepted relay subscription — public
//! activities reach the relay's inbox, non-public ones leave it out, and
//! so does the actor's own followers-only `Update` — and the relay's
//! `Announce` fold: an accepted relay ingests without boosting, a
//! non-relay still boosts, and a `pending` relay folds like a stranger's.

use serde_json::{Value, json};
use wired_be::{
    federation::publisher::{self, PublishOpts},
    models::{
        relay,
        status::{self, Visibility},
        user::get_by_ap_id,
    },
};

use super::support::{BASE, MockRelay, PUBLIC, followed_and_accepted, queued_inboxes, relay_state};
use crate::common::{self, deliver};

// --- the publisher's fan-out ----------------------------------------------------------

/// While the subscription is `pending`, no activity fans out to it; once
/// the relay's `Accept` folds, its inbox joins the fan-out of every
/// public `Create`, `Announce`, `Update` and `Delete` activity.
#[tokio::test]
async fn public_activities_reach_the_accepted_relay_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let mock = MockRelay::start().await;

    let alice = common::create_local_user(&pool, "alice").await;
    let activity = |n: u32, kind: &str, to: Value| {
        json!({
            "id": format!("{BASE}/activities/relay-fanout-{n}"),
            "type": kind,
            "actor": alice.ap_id,
            "object": format!("{BASE}/objects/fanout-{n}"),
            "to": to,
        })
    };

    // Pending: the subscription is not a target yet.
    let _subscription = relay::upsert(&pool, &mock.actor_ap_id(), &mock.inbox_url())
        .await
        .unwrap();
    let pending = publisher::publish(
        &state,
        &alice,
        &activity(1, "Create", json!([PUBLIC])),
        PublishOpts::default(),
    )
    .await
    .expect("the publish runs");
    assert_eq!(pending, 0, "a pending relay is nobody's fan-out target");

    // Accepted: every public activity reaches it — the four types the
    // ticket names, each as one delivery to the relay's inbox alone.
    let _granted = relay::mark_accepted(&pool, &mock.actor_ap_id())
        .await
        .unwrap();
    for (n, kind) in ["Create", "Announce", "Update", "Delete"]
        .into_iter()
        .enumerate()
    {
        let n = u32::try_from(n + 2).unwrap_or(2);
        let count = publisher::publish(
            &state,
            &alice,
            &activity(n, kind, json!([PUBLIC])),
            PublishOpts::default(),
        )
        .await
        .expect("the publish runs");
        assert_eq!(
            count, 1,
            "a public {kind} fans out to the relay's inbox alone"
        );
    }

    let inboxes = queued_inboxes(&pool).await;
    assert_eq!(
        inboxes,
        vec![mock.inbox_url(); 4],
        "every queued delivery asks for the accepted relay's inbox"
    );
}

/// The accepted relay stays out of the fan-out of activities that name
/// nobody public — including the actor's own `Update`, which relays see
/// only when it carries the public address.
#[tokio::test]
async fn non_public_activities_leave_the_accepted_relay_out() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let mock = MockRelay::start().await;
    let alice = common::create_local_user(&pool, "alice").await;
    let strangers_followers = "https://remote.example/users/erin/followers";
    let _subscription = relay::upsert(&pool, &mock.actor_ap_id(), &mock.inbox_url())
        .await
        .unwrap();
    let _granted = relay::mark_accepted(&pool, &mock.actor_ap_id())
        .await
        .unwrap();
    let activity = |n: u32, kind: &str, to: Value| {
        json!({
            "id": format!("{BASE}/activities/relay-fanout-{n}"),
            "type": kind,
            "actor": alice.ap_id,
            "object": format!("{BASE}/objects/fanout-{n}"),
            "to": to,
        })
    };

    let followers_only = publisher::publish(
        &state,
        &alice,
        &activity(6, "Create", json!([strangers_followers])),
        PublishOpts::default(),
    )
    .await
    .expect("the publish runs");
    assert_eq!(
        followers_only, 0,
        "a followers-addressed post reaches no relay"
    );
    let direct = publisher::publish(
        &state,
        &alice,
        &activity(7, "Announce", json!([alice.ap_id])),
        PublishOpts::default(),
    )
    .await
    .expect("the publish runs");
    assert_eq!(direct, 0, "a non-public Announce is nobody's relay feed");

    // The actor's own Update, addressed only to a followers collection:
    // it names no public address, so the relay stays out of it too.
    let own_update = json!({
        "id": format!("{BASE}/activities/relay-fanout-8"),
        "type": "Update",
        "actor": alice.ap_id,
        "object": alice.ap_id,
        "to": [strangers_followers],
    });
    let self_broadcast = publisher::publish(&state, &alice, &own_update, PublishOpts::default())
        .await
        .expect("the publish runs");
    assert_eq!(
        self_broadcast, 0,
        "a non-public Update of the actor's own reaches no relay"
    );

    let inboxes = queued_inboxes(&pool).await;
    assert!(
        inboxes.is_empty(),
        "and the queue holds no delivery at all, got {inboxes:?}"
    );
}

// --- the Announce fold ----------------------------------------------------------------

/// An `Announce` from an accepted relay — whom no user of this instance
/// follows — is accepted: the announced object is fetched through
/// `resolve_status` and ingested as an ordinary public remote status, and
/// no reblog row names the relay.
#[tokio::test]
async fn a_relay_announce_ingests_without_boosting() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let mock = MockRelay::start().await;
    let (_row, relay_user) = followed_and_accepted(&state, &mock).await;

    let alice = common::create_local_user(&pool, "alice").await;
    let local_post = common::create_status(&pool, &alice, "boostable", Visibility::Public).await;

    let announce_remote = json!({
        "id": mock.activity_uri("announce-remote"),
        "type": "Announce",
        "actor": mock.actor_ap_id(),
        "object": mock.object_uri("relayed"),
        "to": [PUBLIC],
    });
    let announce_local = json!({
        "id": mock.activity_uri("announce-local"),
        "type": "Announce",
        "actor": mock.actor_ap_id(),
        "object": local_post.uri,
        "to": [PUBLIC],
    });
    for activity in [&announce_remote, &announce_local] {
        deliver(&state, activity, &relay_user).await;
    }

    let ingested = status::get_by_uri(&pool, &mock.object_uri("relayed"))
        .await
        .expect("the status reads")
        .expect("the announced object was fetched and ingested");
    assert_eq!(
        ingested.visibility,
        Visibility::Public,
        "as an ordinary public remote status"
    );
    assert!(!ingested.local, "and it stays a mirror, not a local post");
    let author = get_by_ap_id(&pool, &format!("{}/users/author", mock.mock.base))
        .await
        .expect("the author reads")
        .expect("the ingest mirrored the author");
    assert_eq!(
        ingested.user_id, author.id,
        "authored by the object's own account"
    );

    for announce in [&announce_remote, &announce_local] {
        let boost = status::get_by_uri(&pool, announce["id"].as_str().expect("an id"))
            .await
            .expect("the status reads");
        assert!(
            boost.is_none(),
            "the relay's Announce {} creates no reblog row",
            announce["id"]
        );
    }
    let relayed_boosts = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM statuses WHERE user_id = $1 AND reblog_of_id IS NOT NULL",
    )
    .bind(relay_user.id)
    .fetch_one(&pool)
    .await
    .expect("the count reads");
    assert_eq!(
        relayed_boosts, 0,
        "and the relay holds no boost of anything"
    );
}

/// The control beside the relay fold: an `Announce` of a local public
/// post from a non-relay actor still makes its reblog row.
#[tokio::test]
async fn a_non_relay_announce_still_boosts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let alice = common::create_local_user(&pool, "alice").await;
    let local_post = common::create_status(&pool, &alice, "boostable", Visibility::Public).await;
    let bobo = common::create_remote_user(&pool, "bobo", "remote.example").await;

    let plain = json!({
        "id": "https://remote.example/activities/boost-1",
        "type": "Announce",
        "actor": bobo.ap_id,
        "object": local_post.uri,
        "to": [PUBLIC],
    });
    deliver(&state, &plain, &bobo).await;
    let boost = status::get_by_uri(&pool, "https://remote.example/activities/boost-1")
        .await
        .expect("the status reads")
        .expect("a non-relay Announce still makes its reblog row");
    assert_eq!(
        boost.reblog_of_id,
        Some(local_post.id),
        "boosting the very post the relay only carries"
    );
}

/// A pending subscription opens no relay arm: until the `Accept` folds,
/// the actor's `Announce` folds like any other remote account's — with
/// its reblog row.
#[tokio::test]
async fn a_pending_relay_announce_folds_like_a_strangers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);

    let relay_user = common::create_remote_user(&pool, "relay", "relay.mock").await;
    let relay_inbox = common::actor_of(&pool, &relay_user)
        .await
        .inbox
        .expect("a fixture inbox");
    let _subscription = relay::upsert(&pool, &relay_user.ap_id, &relay_inbox)
        .await
        .unwrap();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &alice, "pending-target", Visibility::Public).await;

    let announce = json!({
        "id": "https://relay.mock/activities/pending-announce",
        "type": "Announce",
        "actor": relay_user.ap_id,
        "object": post.uri,
        "to": [PUBLIC],
    });
    deliver(&state, &announce, &relay_user).await;
    let boost = status::get_by_uri(&pool, "https://relay.mock/activities/pending-announce")
        .await
        .expect("the status reads")
        .expect("an unaccepted relay is still an ordinary booster");
    assert_eq!(boost.user_id, relay_user.id, "the row names its actor");
    assert_eq!(
        boost.reblog_of_id,
        Some(post.id),
        "and folds the boost the accepted relay would skip"
    );
}
