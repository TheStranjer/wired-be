//! The interaction scenarios: the boost and its withdrawal, the favourite
//! and its withdrawal, the emoji reaction and its withdrawal, and the
//! shared-inbox dedupe two followers on one host collapse into — each
//! asserted from the mock's capture of the bytes that arrived.

use serde_json::json;
use wired_be::config::TEST_BASE_URL;

use super::expectations::{
    Expected, assert_delivered, assert_flags, count_delivered, create_id_of, delivered_for, uri_of,
};
use super::support::{
    AS_PUBLIC, BOB, DAVE, PARTY, delivery_job, followers_of, local, peer_follows, personal, post,
    reaction_write, status_action,
};
use crate::{common, remote};

/// Scenario: the boost and its withdrawal. A public `Announce` names the
/// world in `to` and the followers with the original author in `cc`, and
/// travels under the reblog row's own `uri`; the `Undo` repeats the
/// addressing and embeds that very document. An unlisted `Announce` names
/// the followers in `to` and the original author with the public address
/// in `cc`.
#[tokio::test]
async fn a_boost_and_its_withdrawal_travel_with_the_original_author_in_cc() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-boost-alice").await;
    let (carol, carol_token) = local(&pool, "eo-boost-carol").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let original = post(&app, &carol_token, json!({"status": "boost me"})).await;
    let boost = status_action(
        &app,
        &alice_token,
        &common::id_of(&original),
        "reblog",
        None,
    )
    .await;
    let quiet_source = post(&app, &carol_token, json!({"status": "quietly boost me"})).await;
    let _quiet = status_action(
        &app,
        &alice_token,
        &common::id_of(&quiet_source),
        "reblog",
        Some(json!({"visibility": "unlisted"})),
    )
    .await;
    let _drained = common::run_all_jobs(&state).await;

    let posts = peer.received();
    let with_author = json!([followers_of(&alice), carol.ap_id]);
    let announce = delivered_for(&posts, "Announce", &uri_of(&original));
    assert_delivered(
        announce,
        &Expected {
            path: remote::INBOX,
            kind: "Announce",
            actor: &alice,
            to: Some(json!([AS_PUBLIC])),
            cc: Some(with_author.clone()),
        },
        &peer.host(),
    );
    assert_eq!(
        announce.activity()["id"],
        json!(uri_of(&boost)),
        "the Announce travels under the reblog row's own uri (decision 6)"
    );
    let quiet_announce = delivered_for(&posts, "Announce", &uri_of(&quiet_source));
    assert_eq!(
        quiet_announce.activity()["to"],
        json!([followers_of(&alice)]),
        "an unlisted boost names the followers collection in `to`"
    );
    assert_eq!(
        quiet_announce.activity()["cc"],
        json!([carol.ap_id, AS_PUBLIC]),
        "and carries the original author with the public address in `cc`"
    );

    let _withdrawn =
        status_action(&app, &alice_token, &common::id_of(&boost), "unreblog", None).await;
    let _withdrawal_delivered = common::run_all_jobs(&state).await;
    let undos = peer.delivered("Undo");
    assert_eq!(undos.len(), 1, "the withdrawal is announced once");
    assert_delivered(
        &undos[0],
        &Expected {
            path: remote::INBOX,
            kind: "Undo",
            actor: &alice,
            to: Some(json!([AS_PUBLIC])),
            cc: Some(with_author),
        },
        &peer.host(),
    );
    assert_flags(
        &undos[0].activity()["object"],
        &[
            ("id", json!(uri_of(&boost))),
            ("type", json!("Announce")),
            ("actor", json!(alice.ap_id)),
        ],
    );
}

/// Scenario: the favourite and its withdrawal. A `Like` of a public post
/// names the acted-on author in `to` and the followers with the public
/// address in `cc`, under the favourite row's stored activity id; the
/// unfavourite's `Undo` repeats those lines and embeds that very `Like`. A
/// `Like` of an unlisted post stays with its (local) author: nothing leaves
/// the process for it.
#[tokio::test]
async fn a_favourite_and_its_withdrawal_name_the_author_and_the_followers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-like-alice").await;
    let (carol, carol_token) = local(&pool, "eo-like-carol").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let open = post(&app, &carol_token, json!({"status": "favourite me"})).await;
    let _liked = status_action(&app, &alice_token, &common::id_of(&open), "favourite", None).await;
    let _drained = common::run_all_jobs(&state).await;

    let posts = peer.received();
    let author_to = json!([carol.ap_id]);
    let crowd_cc = json!([followers_of(&alice), AS_PUBLIC]);
    let like = delivered_for(&posts, "Like", &uri_of(&open));
    assert_delivered(
        like,
        &Expected {
            path: remote::INBOX,
            kind: "Like",
            actor: &alice,
            to: Some(author_to.clone()),
            cc: Some(crowd_cc.clone()),
        },
        &peer.host(),
    );
    let like_id = like.activity()["id"].as_str().expect("the Like id");
    assert!(
        like_id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
        "the favourite row's stored activity id: {like_id}"
    );

    let hidden = post(
        &app,
        &carol_token,
        json!({"status": "quietly yours", "visibility": "unlisted"}),
    )
    .await;
    let _quiet_like = status_action(
        &app,
        &alice_token,
        &common::id_of(&hidden),
        "favourite",
        None,
    )
    .await;
    let _drained_again = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.received().len(),
        posts.len(),
        "a like of an unlisted post with a local author announces nothing new"
    );

    let _unliked = status_action(
        &app,
        &alice_token,
        &common::id_of(&open),
        "unfavourite",
        None,
    )
    .await;
    let _drained_thrice = common::run_all_jobs(&state).await;
    let undos = peer.delivered("Undo");
    assert_eq!(undos.len(), 1, "the unfavourite is announced once");
    assert_delivered(
        &undos[0],
        &Expected {
            path: remote::INBOX,
            kind: "Undo",
            actor: &alice,
            to: Some(author_to),
            cc: Some(crowd_cc),
        },
        &peer.host(),
    );
    assert_flags(
        &undos[0].activity()["object"],
        &[("id", json!(like_id)), ("type", json!("Like"))],
    );
}

/// Scenario: the emoji reaction and its withdrawal. An `EmojiReact` shares
/// the `Like`'s author-in-`to`, followers-in-`cc` shape but never the public
/// address, carries the emoji as `content` and the acted-on uri as its
/// object; the `Undo` repeats the lines and embeds the reaction's stored id.
#[tokio::test]
async fn an_emoji_reaction_and_its_withdrawal_federate_without_the_public_address() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-react-alice").await;
    let (carol, carol_token) = local(&pool, "eo-react-carol").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;

    let open = post(&app, &carol_token, json!({"status": "react to me"})).await;
    reaction_write(&app, &alice_token, &common::id_of(&open), "PUT", PARTY).await;
    let _drained = common::run_all_jobs(&state).await;

    let posts = peer.received();
    let author_to = json!([carol.ap_id]);
    let followers_cc = json!([followers_of(&alice)]);
    let react = delivered_for(&posts, "EmojiReact", &uri_of(&open));
    assert_delivered(
        react,
        &Expected {
            path: remote::INBOX,
            kind: "EmojiReact",
            actor: &alice,
            to: Some(author_to.clone()),
            cc: Some(followers_cc.clone()),
        },
        &peer.host(),
    );
    assert_eq!(
        react.activity()["content"],
        "🎉",
        "the emoji is the activity's own content"
    );
    let react_id = react.activity()["id"].as_str().expect("the stored id");

    reaction_write(&app, &alice_token, &common::id_of(&open), "DELETE", PARTY).await;
    let _drained_again = common::run_all_jobs(&state).await;
    let undos = peer.delivered("Undo");
    assert_eq!(undos.len(), 1, "the withdrawal is announced once");
    assert_delivered(
        &undos[0],
        &Expected {
            path: remote::INBOX,
            kind: "Undo",
            actor: &alice,
            to: Some(author_to),
            cc: Some(followers_cc),
        },
        &peer.host(),
    );
    assert_flags(
        &undos[0].activity()["object"],
        &[
            ("id", json!(react_id)),
            ("type", json!("EmojiReact")),
            ("content", json!("🎉")),
        ],
    );
}

/// Scenario: shared-inbox dedupe. `bob` and `dave` — two followers on one
/// instance — cost one `Create` between them, because the publisher expands
/// the followers collection through the shared endpoint and dedupes the
/// candidate set by URL; the delivery row carries the key
/// `deliver:{activity id}:{inbox}` that swallows a second queueing of the
/// same pair while it is live. The per-actor inboxes stay reachable, and a
/// reaction restated before its delivery has run never sends twice.
#[tokio::test]
async fn followers_sharing_one_inbox_cost_one_delivery_per_activity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "eo-dedupe-alice").await;
    peer_follows(&app, &state, &peer, BOB, &alice).await;
    peer_follows(&app, &state, &peer, DAVE, &alice).await;

    let posted = post(&app, &alice_token, json!({"status": "two followers"})).await;
    let _drained = common::run_all_jobs(&state).await;

    let posts = peer.received();
    let uri = uri_of(&posted);
    assert_eq!(
        count_delivered(&posts, remote::INBOX, "Create", &uri),
        1,
        "two followers, one shared inbox, one delivery"
    );
    let create_id = create_id_of(&posted);
    let job = delivery_job(&pool, &create_id).await;
    assert_eq!(
        job.unique_key.as_deref(),
        Some(format!("deliver:{create_id}:{}", peer.shared_inbox()).as_str()),
        "the dedup key names exactly this activity-inbox pair"
    );
    assert_eq!(job.state, "completed", "and it delivered once, cleanly");
    assert!(
        posts.iter().any(|p| p.path() == personal(&peer, BOB)),
        "the per-actor inbox path stayed reachable throughout"
    );

    let restated = post(&app, &alice_token, json!({"status": "restated fact"})).await;
    reaction_write(&app, &alice_token, &common::id_of(&restated), "PUT", PARTY).await;
    reaction_write(&app, &alice_token, &common::id_of(&restated), "PUT", PARTY).await;
    let _drained_again = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.delivered("EmojiReact").len(),
        1,
        "the repeat write queues a second fan-out, the live delivery's key swallows it"
    );
}
