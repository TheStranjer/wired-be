//! The note scenarios: the public status lifecycle of `Create`, `Update`
//! and `Delete`, the visibility matrix over a post and its edit, the
//! withdrawal that repeats each visibility's audience, the `local`-only
//! post that leaves nothing at all, and the direct note that reaches the
//! mentioned remote's personal inbox alone.

use serde_json::json;

use super::expectations::{
    Expected, assert_delivered, assert_lifecycle_create, assert_lifecycle_delete,
    assert_lifecycle_update, count_delivered, count_of, create_id_of, delivered_for, expected_case,
    id_of, note_cases, uri_of,
};
use super::support::{
    BOB, DAVE, destroy, edit, followers_of, local, peer_follows, personal, post, post_text,
};
use crate::{common, remote};

// --- the scenarios ------------------------------------------------------------------------

/// Scenario: the public status lifecycle. The `Create`, the `Update` of the
/// re-drafted note and the `Delete` of the `Tombstone` each arrive as one
/// signed `POST` at the follower's shared inbox, under decision 6's ids and
/// the same public addressing throughout.
#[tokio::test]
async fn a_public_status_lifecycle_travels_signed_to_the_follower_inbox() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;
    let host = peer.host();

    let posted = post(&app, &alice_token, json!({"status": "first draft"})).await;
    let id = id_of(&posted);
    let uri = uri_of(&posted);
    let create_id = create_id_of(&posted);
    let _create_delivered = common::run_all_jobs(&state).await;
    let _edited = edit(&app, &alice_token, &id, "second draft").await;
    let _update_delivered = common::run_all_jobs(&state).await;
    destroy(&app, &alice_token, &id).await;
    let _delete_delivered = common::run_all_jobs(&state).await;

    let posts = peer.received();
    assert_lifecycle_create(&posts, &alice, &host, &uri, &create_id);
    assert_lifecycle_update(&posts, &alice, &host, &uri, &create_id);
    assert_lifecycle_delete(&posts, &alice, &host, &uri, &id);
    assert_eq!(
        posts.len(),
        4,
        "the Accept that answered the follow, and the lifecycle — nothing else"
    );
    assert_eq!(posts[0].activity_type(), Some("Accept"));
    assert_eq!(posts[0].path(), personal(&peer, BOB));
}

/// Scenario: the visibility matrix over the post and the edit. Each of
/// `public`, `unlisted`, `private` and `direct` travels with its own `to`
/// and `cc` split to the inbox its audience expands to, and `local` never
/// arrives: the mock holds four notes up and four re-drafts, no fifth.
#[tokio::test]
async fn every_visibility_shapes_and_sends_the_note_its_own_way() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-matrix-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;
    let host = peer.host();
    let cases = note_cases(
        &peer.actor(BOB),
        &followers_of(&alice),
        &personal(&peer, BOB),
    );

    let specs = [
        ("to the world", None),
        ("off the timeline", Some("unlisted")),
        ("for followers", Some("private")),
        ("psst", Some("direct")),
        ("instance only", Some("local")),
    ];
    let mut posted = Vec::new();
    for (text, visibility) in specs {
        let body = if visibility == Some("direct") {
            json!({"status": format!("{text} @{}", peer.handle(BOB)), "visibility": "direct"})
        } else {
            json!({"status": text, "visibility": visibility.unwrap_or("public")})
        };
        posted.push(post(&app, &alice_token, body).await);
    }
    // The direct note must keep its mention when edited: the re-drafted
    // document is addressed from the live row's audience, and an edited DM
    // that names nobody travels nowhere.
    let drafts = [
        "edited world",
        "edited timeline",
        "edited followers",
        &format!("edited psst @{}", peer.handle(BOB)),
        "edited inside",
    ];
    for (entity, text) in posted.iter().zip(drafts) {
        let id = id_of(entity);
        let _edited = edit(&app, &alice_token, &id, text).await;
    }
    let _drained = common::run_all_jobs(&state).await;

    let posts = peer.received();
    for (index, case) in cases.iter().enumerate() {
        let uri = uri_of(&posted[index]);
        for kind in ["Create", "Update"] {
            assert_delivered(
                delivered_for(&posts, kind, &uri),
                &expected_case(case, kind, &alice),
                &host,
            );
        }
    }
    assert_eq!(
        count_delivered(&posts, remote::INBOX, "Create", &uri_of(&posted[4])),
        0,
        "the `local` post delivered nothing"
    );
    assert_eq!(
        count_of(&posts, "Create") + count_of(&posts, "Update"),
        8,
        "four notes up, four re-drafts, no fifth"
    );
}

/// Scenario: the withdrawal of every visibility repeats its audience. The
/// `Delete` of each post lands on exactly the path its `Create` arrived at,
/// with the same `to` and `cc` — the direct one's comes down at the
/// personal inbox alone.
#[tokio::test]
async fn each_delete_comes_down_exactly_where_its_post_went_up() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-down-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;
    let host = peer.host();
    let cases = note_cases(
        &peer.actor(BOB),
        &followers_of(&alice),
        &personal(&peer, BOB),
    );

    let mut posted = Vec::new();
    for (text, visibility) in [
        ("public down", None),
        ("unlisted down", Some("unlisted")),
        ("private down", Some("private")),
        ("direct down", Some("direct")),
    ] {
        let body = if visibility == Some("direct") {
            json!({"status": format!("{text} @{}", peer.handle(BOB)), "visibility": "direct"})
        } else {
            json!({"status": text, "visibility": visibility.unwrap_or("public")})
        };
        posted.push(post(&app, &alice_token, body).await);
    }
    let _up = common::run_all_jobs(&state).await;
    for entity in &posted {
        destroy(&app, &alice_token, &id_of(entity)).await;
    }
    let _down = common::run_all_jobs(&state).await;

    let posts = peer.received();
    for (case, entity) in cases.iter().zip(&posted) {
        let uri = uri_of(entity);
        assert_eq!(
            count_delivered(&posts, &case.path, "Delete", &uri),
            1,
            "the withdrawal of `{uri}` arrived once, at `{}`",
            case.path
        );
        assert_delivered(
            delivered_for(&posts, "Delete", &uri),
            &expected_case(case, "Delete", &alice),
            &host,
        );
    }
    assert_eq!(count_of(&posts, "Create"), 4, "one up for each down");
}

/// Scenario: `local` visibility keeps everything at home. The post, its edit
/// and its delete write their rows, change what the client reads, and leave
/// the mock with nothing but the `Accept` that answered its own follow.
#[tokio::test]
async fn a_local_only_status_leaves_nothing_at_all() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-home-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let posted = post_text(&app, &alice_token, "stays inside", Some("local")).await;
    let id = id_of(&posted);
    let _edited = edit(&app, &alice_token, &id, "still inside").await;
    let _first = common::run_all_jobs(&state).await;
    destroy(&app, &alice_token, &id).await;
    let _second = common::run_all_jobs(&state).await;

    assert_eq!(
        peer.delivery_count(),
        1,
        "nothing of the local post, its edit or its delete left the process"
    );
    assert_eq!(peer.received()[0].activity_type(), Some("Accept"));
}

/// Scenario: a direct post is for the mentionee alone. It reaches `dave`'s
/// personal inbox even though the mock publishes a shared endpoint for
/// every actor — the publisher puts a directly-addressed recipient at the
/// inbox that names them — and the follower `bob` never hears of it: no
/// shared-inbox copy and no follower fan-out.
#[tokio::test]
async fn a_direct_post_reaches_the_mentioned_inbox_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-dm-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let text = format!("dave, between us @{}", peer.handle(DAVE));
    let posted = post_text(&app, &alice_token, &text, Some("direct")).await;
    let _drained = common::run_all_jobs(&state).await;

    let posts = peer.received();
    let uri = uri_of(&posted);
    assert_delivered(
        delivered_for(&posts, "Create", &uri),
        &Expected {
            path: &personal(&peer, DAVE),
            kind: "Create",
            actor: &alice,
            to: Some(json!([peer.actor(DAVE)])),
            cc: Some(json!([])),
        },
        &peer.host(),
    );
    assert_eq!(
        count_delivered(&posts, remote::INBOX, "Create", &uri),
        0,
        "the shared inbox holds no copy of a direct"
    );
    assert_eq!(
        count_delivered(&posts, &personal(&peer, BOB), "Create", &uri),
        0,
        "the follower who was not mentioned holds none either"
    );
    assert_eq!(
        posts.len(),
        2,
        "bob's Accept and this DM: the direct fans out to nobody else"
    );
}
