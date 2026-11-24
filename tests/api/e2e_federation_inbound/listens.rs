//! Inbound `Listen` activities end to end: a remote account's scrobble read
//! back through `GET /api/v1/pleroma/accounts/{id}/scrobbles`.

use serde_json::{Value, json};

use super::support::{
    AS_PUBLIC, BOB, PUBLISHED, activity, deliver, follow_bob, list, local, lookup,
};
use crate::{common, remote};

/// The `Audio` object of the peer's `actor`, named `name`: the shape Pleroma
/// delivers inside a `Listen`.
fn audio(peer: &remote::MockPeer, actor: &str, name: &str, title: &str) -> Value {
    json!({
        "id": peer.note(name),
        "type": "Audio",
        "actor": actor,
        "title": title,
        "artist": "Lain Iwakura",
        "album": "Navi",
        "length": 200_000,
        "externalLink": "https://example.com/track/2",
        "published": PUBLISHED,
    })
}

/// A `Listen` of `object` by `actor` with the given addressing.
fn listen(
    peer: &remote::MockPeer,
    actor: &str,
    name: &str,
    object: Value,
    addressing: (Value, Value),
) -> Value {
    let mut document = activity(peer, "Listen", name, actor, object);
    document["to"] = addressing.0;
    document["cc"] = addressing.1;
    document
}

/// Scenario: a followed remote account scrobbles. The public `Listen` shows
/// in the listing of its account, a redelivery adds nothing, and a `private`
/// one is hidden from the follower and from anonymous readers alike.
#[tokio::test]
async fn a_remote_listen_is_listed_on_the_scrobbles_of_its_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "ei-listen-alice").await;
    follow_bob(&app, &state, &peer, &alice_token, "listen-accept").await;
    let actor = peer.actor(BOB);

    let public = listen(
        &peer,
        &actor,
        "listen-1",
        audio(&peer, &actor, "audio-1", "Pleroma Song"),
        (json!([AS_PUBLIC]), json!([format!("{actor}/followers")])),
    );
    deliver(&app, &state, &peer, BOB, &public).await;
    deliver(&app, &state, &peer, BOB, &public).await;
    let private = listen(
        &peer,
        &actor,
        "listen-2",
        audio(&peer, &actor, "audio-2", "Secret Song"),
        (json!([format!("{actor}/followers")]), json!([])),
    );
    deliver(&app, &state, &peer, BOB, &private).await;

    let bob_id = lookup(&app, &peer.handle(BOB)).await["id"]
        .as_str()
        .expect("the mirror's id")
        .to_owned();
    let path = format!("/api/v1/pleroma/accounts/{bob_id}/scrobbles");
    let listed = list(&app, Some(&alice_token), &path).await;
    let [entry] = listed.as_slice() else {
        panic!("exactly the public listen is listed, got {listed:?}");
    };
    assert_eq!(entry["title"], json!("Pleroma Song"), "the title");
    assert_eq!(entry["artist"], json!("Lain Iwakura"), "the artist");
    assert_eq!(entry["album"], json!("Navi"), "the album");
    assert_eq!(entry["length"], json!(200_000), "the length");
    assert_eq!(
        entry["externalLink"],
        json!("https://example.com/track/2"),
        "the link"
    );
    assert_eq!(entry["account"]["id"], json!(bob_id), "listened by bob");
    assert_eq!(
        list(&app, None, &path).await.len(),
        1,
        "an anonymous reader sees the same one"
    );
}
