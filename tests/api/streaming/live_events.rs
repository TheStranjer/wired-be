//! Real service events over a live socket, end to end: the status lifecycle
//! [`create_status`] and [`delete_status`] publish to the public and home
//! streams, the send-time render each connection performs for its own
//! viewer, the `notification` frame [`notify::on_favourite`] pushes to the
//! recipient's socket, and the roster-wide `announcement` the real
//! [`announcement_published`] emitter puts on the bus.
//!
//! [`create_status`]: wired_be::services::create_status::create_status
//! [`delete_status`]: wired_be::services::delete_status::delete_status
//! [`notify::on_favourite`]: wired_be::services::notify::on_favourite
//! [`announcement_published`]: wired_be::services::streaming::announcement_published

use serde_json::{Value, json};
use uuid::Uuid;
use wired_be::{
    models::{block, favourite, follow, status},
    services::{create_status, delete_status, notify, streaming},
    streaming::hub::Audience,
};

use super::harness::{
    LIFECYCLE_TEXT, PUBLIC, Server, account, assert_absent, assert_update_carries, create_body,
    event_on, next_json, open, parse_event, public_update, read_until, settled, streams_of, token,
};
use crate::common;

/// The whole lifecycle over a real socket: creating a public status through
/// [`create_status`] publishes `update` with the rendered entity, and an
/// anonymous socket on `public` and a follower's socket on `user` both
/// receive it; deleting it publishes `delete` and both sockets see the
/// withdrawal.
#[tokio::test]
async fn a_created_public_status_reaches_public_and_home_then_deletes() {
    let server = Server::start().await;
    let author = account(server.pool(), "lifecycle-author").await;
    let follower = account(server.pool(), "lifecycle-follower").await;
    let relationship = follow::follow(
        server.pool(),
        &server.state.config.base_url,
        &follower,
        &author,
    )
    .await
    .expect("the follow fixture writes");
    assert_eq!(
        relationship.state,
        follow::FollowState::Accepted,
        "the unlocked local target grants the follow on the spot"
    );
    let secret = token(server.pool(), &follower, "read").await;

    let mut anon = open(&server.url(PUBLIC)).await;
    let mut home = open(&server.url(&format!("stream=user&access_token={secret}"))).await;
    settled(&mut anon).await;
    settled(&mut home).await;

    let entity =
        create_status::create_status(&server.state, &author, None, create_body(LIFECYCLE_TEXT))
            .await
            .expect("the create service runs");

    let anon_frame = next_json(&mut anon).await;
    assert_update_carries(&anon_frame, &entity.id);
    assert!(
        streams_of(&anon_frame).contains(&"public"),
        "the anonymous socket's public stream carries it: {anon_frame}"
    );

    let home_stream = format!("user:{}", follower.id);
    let home_frame = next_json(&mut home).await;
    assert_update_carries(&home_frame, &entity.id);
    assert!(
        streams_of(&home_frame).contains(&home_stream.as_str()),
        "the follower's home stream carries it: {home_frame}"
    );

    let id = Uuid::parse_str(&entity.id).expect("the entity id is a uuid");
    let row = status::get_by_id(server.pool(), id)
        .await
        .expect("the row reads")
        .expect("the create stored it");
    assert!(
        delete_status::delete_status(&server.state, &row)
            .await
            .expect("the delete service runs"),
        "the row was live"
    );

    for (frame, who) in [
        (next_json(&mut anon).await, "anonymous"),
        (next_json(&mut home).await, "follower"),
    ] {
        assert_eq!(frame["event"], "delete", "the {who} socket sees the delete");
        assert_eq!(
            frame["payload"].as_str(),
            Some(entity.id.as_str()),
            "the {who} socket's delete payload is the status id"
        );
    }
}

/// The send-time render over a real socket: creating a public status
/// publishes its `RenderSpec::Status` hint, and each connection renders
/// the entity for its OWN viewer at send time. A plain subscriber on
/// `public` receives the full entity JSON in the frame; a subscriber who
/// blocked the author receives nothing of it — only the control event
/// published after, which proves its socket is open and streaming.
#[tokio::test]
async fn a_created_status_renders_for_one_subscriber_and_skips_a_blocker() {
    let server = Server::start().await;
    let author = account(server.pool(), "render-author").await;
    let subscriber = account(server.pool(), "render-subscriber").await;
    let blocker = account(server.pool(), "render-blocker").await;
    let _blocked = block::block(
        server.pool(),
        &server.state.config.base_url,
        &blocker,
        &author,
    )
    .await
    .expect("the block fixture writes");
    let seen_token = token(server.pool(), &subscriber, "read").await;
    let blind_token = token(server.pool(), &blocker, "read").await;
    let mut watcher = open(&server.url(&format!("stream=public&access_token={seen_token}"))).await;
    let mut skipped = open(&server.url(&format!("stream=public&access_token={blind_token}"))).await;
    settled(&mut watcher).await;
    settled(&mut skipped).await;

    let entity =
        create_status::create_status(&server.state, &author, None, create_body(LIFECYCLE_TEXT))
            .await
            .expect("the create service runs");

    let frame = next_json(&mut watcher).await;
    assert_update_carries(&frame, &entity.id);
    let payload: Value = serde_json::from_str(
        frame["payload"]
            .as_str()
            .expect("the update carries a payload string"),
    )
    .expect("the payload is the entity JSON");
    assert_eq!(
        payload["account"]["id"],
        json!(author.id.to_string()),
        "the rendered entity carries the author's account"
    );

    server.publish(public_update("render-control"));
    let frames = read_until(&mut skipped, "render-control").await;
    assert_absent(&frames, &[&entity.id, LIFECYCLE_TEXT]);
    assert!(
        frames.iter().any(|line| line.contains("render-control")),
        "the blocker's socket goes on streaming after the dropped hint: {frames:?}"
    );
}

/// The notification push end to end: B favourites A's status, and
/// [`notify::on_favourite`] publishes Mastodon's `notification` for the
/// recipient with a [`RenderSpec::Notification`] hint; A's socket,
/// subscribed to `user:notification`, receives the frame with the like
/// rendered as its entity. A third user's socket on the same client
/// stream receives nothing of it: only its own control event arrives.
#[tokio::test]
async fn a_favourite_lands_a_notification_frame_on_the_recipient_socket() {
    let server = Server::start().await;
    let author = account(server.pool(), "notif-author").await;
    let favouriter = account(server.pool(), "notif-favouriter").await;
    let bystander = account(server.pool(), "notif-bystander").await;
    let entity =
        create_status::create_status(&server.state, &author, None, create_body(LIFECYCLE_TEXT))
            .await
            .expect("the create service runs");
    let status_id = Uuid::parse_str(&entity.id).expect("the entity id is a uuid");
    let notified_token = token(server.pool(), &author, "read").await;
    let blind_token = token(server.pool(), &bystander, "read").await;
    let mut notified = open(&server.url(&format!(
        "stream=user:notification&access_token={notified_token}"
    )))
    .await;
    let mut bystander_socket = open(&server.url(&format!(
        "stream=user:notification&access_token={blind_token}"
    )))
    .await;
    settled(&mut notified).await;
    settled(&mut bystander_socket).await;

    let like = favourite::favourite(
        server.pool(),
        &server.state.config.base_url,
        favouriter.id,
        status_id,
    )
    .await
    .expect("the favourite fixture writes")
    .expect("the pair was fresh");
    notify::on_favourite(&server.state, &like, true).await;

    let frame = next_json(&mut notified).await;
    assert_eq!(
        frame["event"], "notification",
        "the recipient sees the event"
    );
    let recipient_stream = format!("user:notification:{}", author.id);
    assert!(
        streams_of(&frame).contains(&recipient_stream.as_str()),
        "it rides the recipient's notification stream: {frame}"
    );
    let payload: Value = serde_json::from_str(
        frame["payload"]
            .as_str()
            .expect("the notification carries a payload string"),
    )
    .expect("the payload is the entity JSON");
    assert_eq!(payload["type"], "favourite", "the entity is the like");
    assert_eq!(
        payload["account"]["id"],
        json!(favouriter.id.to_string()),
        "the entity names the favouriter as its actor"
    );
    assert_eq!(
        payload["status"]["id"],
        json!(entity.id),
        "and the liked status"
    );
    assert_eq!(payload["pleroma"]["is_muted"], false, "A does not mute B");

    let own_stream = format!("user:notification:{}", bystander.id);
    server.publish(event_on(
        &[own_stream.as_str()],
        "notif-control",
        Audience::Users(vec![bystander.id]),
    ));
    let frames = read_until(&mut bystander_socket, "notif-control").await;
    assert_absent(&frames, &[r#""event":"notification""#, &entity.id]);
    assert!(
        frames.iter().any(|line| line.contains("notif-control")),
        "the bystander's socket is open and streaming, just not this: {frames:?}"
    );
}

/// `announcement_published` (WBE-191 slice 5), end to end: the emitter reads
/// the active-local roster and rides every one of those users' own
/// `user:<uuid>` stream, so each active local socket receives the
/// announcement and no remote or anonymous socket does. The event is put on
/// the bus by the real emitter, so streams, audience and the entity-as-string
/// payload are its production shape. Each socket is then bounded by its own
/// control event on its own subscription, published after the announcement:
/// what the announcement could not have reached before the control, it never
/// will.
#[tokio::test]
async fn announcement_published_reaches_every_active_local_socket() {
    let server = Server::start().await;
    let alice = account(server.pool(), "announce-alice").await;
    let bob = account(server.pool(), "announce-bob").await;
    let remote =
        common::create_remote_user(server.pool(), "announce-remote", "remote.example").await;
    let alice_stream = format!("user:{}", alice.id);
    let bob_stream = format!("user:{}", bob.id);
    let remote_stream = format!("user:{}", remote.id);
    let mut alice_socket = open(&server.url(&format!(
        "stream=user&access_token={}",
        token(server.pool(), &alice, "read").await
    )))
    .await;
    let mut bob_socket = open(&server.url(&format!(
        "stream=user&access_token={}",
        token(server.pool(), &bob, "read").await
    )))
    .await;
    let mut remote_socket = open(&server.url(&format!(
        "stream=user&access_token={}",
        token(server.pool(), &remote, "read").await
    )))
    .await;
    let mut anonymous = open(&server.url(PUBLIC)).await;

    let entity = json!({"id": "announcement-1", "content": "<p>Up!</p>"});
    streaming::announcement_published(&server.state, &entity).await;
    server.publish(event_on(
        &[alice_stream.as_str()],
        "alice-control",
        Audience::Users(vec![alice.id]),
    ));
    server.publish(event_on(
        &[bob_stream.as_str()],
        "bob-control",
        Audience::Users(vec![bob.id]),
    ));
    server.publish(event_on(
        &[remote_stream.as_str()],
        "remote-control",
        Audience::Users(vec![remote.id]),
    ));
    server.publish(public_update("anon-control"));

    let alice_frames = read_until(&mut alice_socket, "alice-control").await;
    let alice_announcement = parse_event(&alice_frames, "announcement");
    assert_eq!(
        alice_announcement["stream"],
        json!([alice_stream, bob_stream]),
        "the event rides exactly the roster's own streams, alice's first"
    );
    assert_eq!(
        alice_announcement["payload"],
        entity.to_string(),
        "Mastodon ships the entity as its JSON string"
    );

    let bob_frames = read_until(&mut bob_socket, "bob-control").await;
    let bob_announcement = parse_event(&bob_frames, "announcement");
    assert_eq!(
        bob_announcement["payload"],
        entity.to_string(),
        "every active local socket gets the same entity"
    );

    let remote_frames = read_until(&mut remote_socket, "remote-control").await;
    assert_absent(&remote_frames, &["announcement-1"]);

    let anonymous_frames = read_until(&mut anonymous, "anon-control").await;
    assert_absent(&anonymous_frames, &["announcement-1"]);
}
