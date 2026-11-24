//! Streaming scenarios over the recipient-scoped streams: notification,
//! direct-conversation, chat and filter tellings, and the two token
//! carriers a handshake accepts.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::server::{SCOPES, Server, Verb, action_ok, write_ok};
use super::sockets::{
    account, assert_absent, direct_stream, favourite, follow, frame_payload, notification_stream,
    open, open_with_protocol, post_status, read_until, read_until_json, settled, streams_of,
    user_stream,
};
use crate::common;

/// Real interactions land real `notification` frames: Bob's favourite of
/// Alice's post over HTTP renders Mastodon's notification on Alice's
/// `user:notification` socket with the like as its entity, and Bob's
/// follow of Alice — granted on the spot — follows it with the `follow`
/// telling. A bystander's socket on the same client stream stays silent
/// through both: the audience names its recipient alone.
#[tokio::test]
async fn favourites_and_follows_stream_notification_frames() {
    let srv = Server::start().await;
    let (alice, alice_token) = account(&srv, "rt226-notif-alice", SCOPES).await;
    let (bob, bob_token) = account(&srv, "rt226-notif-bob", SCOPES).await;
    let (bystander, bystander_token) = account(&srv, "rt226-notif-outsider", SCOPES).await;
    let status_id = common::id_of(&post_status(&srv, &alice_token, "to be liked", "public").await);

    let alice_stream = notification_stream(alice.id);
    let other_stream = notification_stream(bystander.id);
    let mut notified = open(&srv.ws_url(&format!(
        "stream=user:notification&access_token={alice_token}"
    )))
    .await;
    let mut blind = open(&srv.ws_url(&format!(
        "stream=user:notification&access_token={bystander_token}"
    )))
    .await;
    settled(&mut notified).await;
    settled(&mut blind).await;

    favourite(&srv, &bob_token, &status_id).await;
    follow(&srv, &bob_token, alice.id).await;

    let like = read_until_json(&mut notified, r#"\"type\":\"favourite\""#).await;
    assert_eq!(
        like["event"], "notification",
        "the favourite arrives on the recipient's client stream"
    );
    assert_eq!(
        streams_of(&like),
        ["user:notification"],
        "named by the recipient's notification stream: {like}"
    );
    assert!(
        !like.to_string().contains(alice_stream.as_str()),
        "not by its internal uuid-named form: {like}"
    );
    let like_payload = frame_payload(&like);
    assert_eq!(like_payload["type"], "favourite", "the like renders");
    assert_eq!(
        like_payload["account"]["id"],
        json!(bob.id.to_string()),
        "with the actor"
    );
    assert_eq!(
        like_payload["status"]["id"],
        json!(status_id),
        "and the liked post"
    );

    let arrival = read_until_json(&mut notified, r#"\"type\":\"follow\""#).await;
    let arrival_payload = frame_payload(&arrival);
    assert_eq!(arrival_payload["type"], "follow", "the follow lands too");
    assert_eq!(
        arrival_payload["account"]["id"],
        json!(bob.id.to_string()),
        "from the same actor"
    );

    srv.publish_control(&other_stream, bystander.id, "rt226-bystander-control");
    let frames = read_until(&mut blind, "rt226-bystander-control").await;
    assert_absent(&frames, &[r#""event":"notification""#, &status_id]);
    assert!(
        frames
            .iter()
            .any(|frame| frame.contains("rt226-bystander-control")),
        "the bystander's socket is open and streaming, just not Alice's: {frames:?}"
    );
}

/// The `direct` DM journey, streamed: Dave's `direct` post @-mentioning
/// Alice over HTTP renders for Alice on both hub names her socket holds —
/// the `update` of the message itself on `user` and `direct`, and
/// Mastodon's `conversation` with her own side of the thread as the hinted
/// entity (the message as `last_status`, Dave among the `accounts`).
#[tokio::test]
async fn a_direct_post_streams_update_and_conversation_frames() {
    let srv = Server::start().await;
    let (alice, alice_token) = account(&srv, "rt226-dm-alice", SCOPES).await;
    let (dave, dave_token) = account(&srv, "rt226-dm-dave", SCOPES).await;
    let own_home = user_stream(alice.id);
    let own_direct = direct_stream(alice.id);
    let mut client = open(&srv.ws_url(&format!("stream=direct&access_token={alice_token}"))).await;
    settled(&mut client).await;

    let message = post_status(
        &srv,
        &dave_token,
        "@rt226-dm-alice a whispered word",
        "direct",
    )
    .await;
    let message_id = common::id_of(&message);
    let frames = read_until(&mut client, r#""event":"conversation""#).await;
    let update: Value = serde_json::from_str(
        frames
            .iter()
            .find(|frame| frame.contains(r#""event":"update""#))
            .expect("the message itself streamed before the conversation"),
    )
    .expect("frames are JSON envelopes");
    assert_eq!(
        streams_of(&update),
        ["direct"],
        "the DM rides the one stream the socket subscribed to, `direct`: {update}"
    );
    assert!(
        !update.to_string().contains(own_home.as_str())
            && !update.to_string().contains(own_direct.as_str()),
        "and never the internal uuid-named ones: {update}"
    );
    let conversation: Value = serde_json::from_str(
        frames
            .iter()
            .find(|frame| frame.contains(r#""event":"conversation""#))
            .expect("the conversation frame arrived"),
    )
    .expect("frames are JSON envelopes");
    assert_eq!(
        streams_of(&conversation),
        ["direct"],
        "it rides the direct stream: {conversation}"
    );
    let payload = frame_payload(&conversation);
    assert_eq!(
        payload["last_status"]["id"],
        json!(message_id),
        "the thread's last status is the message"
    );
    assert!(
        payload["accounts"]
            .as_array()
            .is_some_and(|accounts| accounts
                .iter()
                .any(|account| account["id"] == json!(dave.id.to_string()))),
        "the other side names Dave: {payload}"
    );
}

/// Pleroma's chat and Mastodon's filter tellings, streamed from the real
/// writes: Bob's chat message over HTTP lands `pleroma:chat_update` on
/// Alice's `user:pleroma_chat` socket with her own chat row (her chat id,
/// Bob's message as `last_message`), and Alice's filter write answers her
/// own `user` stream with the bare `filters_changed` frame — an envelope
/// with no payload at all.
#[tokio::test]
async fn chat_messages_and_filter_writes_stream_their_tellings() {
    let srv = Server::start().await;
    let (alice, alice_token) = account(&srv, "rt226-chat-alice", SCOPES).await;
    let (bob, bob_token) = account(&srv, "rt226-chat-bob", SCOPES).await;

    let alice_chat = action_ok(
        &srv,
        &format!("/api/v1/pleroma/chats/by-account-id/{}", bob.id),
        &alice_token,
    )
    .await;
    let alice_chat_id = common::id_of(&alice_chat);
    let bob_chat = action_ok(
        &srv,
        &format!("/api/v1/pleroma/chats/by-account-id/{}", alice.id),
        &bob_token,
    )
    .await;
    let bob_chat_id = common::id_of(&bob_chat);

    let mut chat_socket = open(&srv.ws_url(&format!(
        "stream=user:pleroma_chat&access_token={alice_token}"
    )))
    .await;
    let mut home_socket =
        open(&srv.ws_url(&format!("stream=user&access_token={alice_token}"))).await;
    settled(&mut chat_socket).await;
    settled(&mut home_socket).await;

    let _message = write_ok(
        &srv,
        Verb::Post,
        &format!("/api/v1/pleroma/chats/{bob_chat_id}/messages"),
        &bob_token,
        &json!({ "content": "streamed through the chat" }),
    )
    .await;
    let update = read_until_json(&mut chat_socket, &alice_chat_id).await;
    assert_eq!(
        update["event"], "pleroma:chat_update",
        "the chat change rides its own event"
    );
    assert_eq!(
        streams_of(&update),
        ["user:pleroma_chat"],
        "on Alice's own chat stream: {update}"
    );
    let chat_payload = frame_payload(&update);
    assert_eq!(
        chat_payload["id"],
        json!(alice_chat_id),
        "rendered as Alice's OWN chat row"
    );
    assert!(
        chat_payload["last_message"]["content"]
            .as_str()
            .is_some_and(|content| content.contains("streamed through the chat")),
        "with Bob's message on it: {chat_payload}"
    );

    let _filter = write_ok(
        &srv,
        Verb::Post,
        "/api/v1/filters",
        &alice_token,
        &json!({ "phrase": "hush now", "context": ["home"], "irreversible": true }),
    )
    .await;
    let changed = read_until_json(&mut home_socket, r#""event":"filters_changed""#).await;
    assert_eq!(
        changed["event"], "filters_changed",
        "the filter write tells"
    );
    assert!(
        changed.get("payload").is_none(),
        "Mastodon sends it bare: {changed}"
    );
    assert_eq!(
        streams_of(&changed),
        ["user"],
        "on the owner's own user stream: {changed}"
    );
}

/// Both token carriers the Mastodon clients use, end to end: the query
/// parameter authenticates the handshake and the socket streams its user's
/// own timeline, while the web client's `Sec-WebSocket-Protocol` smuggle
/// is echoed back as the chosen subprotocol (a browser drops anything
/// else) and carries the same identity for its own reader.
#[tokio::test]
async fn tokens_in_the_query_and_in_the_subprotocol_open_their_sockets() {
    let srv = Server::start().await;
    let (alice, alice_token) = account(&srv, "rt226-tok-alice", SCOPES).await;
    let (bob, bob_token) = account(&srv, "rt226-tok-bob", SCOPES).await;
    follow(&srv, &bob_token, alice.id).await;

    let mut query_socket =
        open(&srv.ws_url(&format!("stream=user&access_token={alice_token}"))).await;
    let (mut protocol_socket, response) =
        open_with_protocol(&srv.ws_url("stream=user"), &bob_token).await;
    assert_eq!(
        response.status(),
        StatusCode::SWITCHING_PROTOCOLS,
        "the upgrade went through on the subprotocol token alone"
    );
    assert_eq!(
        response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|value| value.to_str().ok()),
        Some(bob_token.as_str()),
        "the offered protocol comes back as the chosen one"
    );
    settled(&mut query_socket).await;
    settled(&mut protocol_socket).await;

    let id = common::id_of(&post_status(&srv, &alice_token, "query and protocol", "public").await);
    let alice_own = user_stream(alice.id);
    let bob_own = user_stream(bob.id);

    // The home rule names every target stream on the event, so what tells
    // the two sockets apart is not which stream names ride the envelope —
    // both carry Alice's and Bob's — but whose VIEW the entity was
    // rendered in: the query socket sees Alice's own account with no
    // self-relationship, the subprotocol socket sees the same account as
    // the relationship Bob's token names.
    let query_frames = read_until(&mut query_socket, &id).await;
    let query_frame: Value = serde_json::from_str(
        query_frames
            .iter()
            .find(|frame| frame.contains(&id))
            .expect("the query socket got the post"),
    )
    .expect("frames are JSON envelopes");
    assert_eq!(
        streams_of(&query_frame),
        ["user"],
        "the query-token socket streams for its user: {query_frame}"
    );
    assert!(
        !query_frame.to_string().contains(alice_own.as_str()),
        "without leaking the internal uuid-named stream: {query_frame}"
    );
    let query_payload = frame_payload(&query_frame);
    assert_eq!(
        query_payload["id"],
        json!(id),
        "the query socket reads the very post the HTTP write made: {query_payload}"
    );
    let protocol_frames = read_until(&mut protocol_socket, &id).await;
    let protocol_frame: Value = serde_json::from_str(
        protocol_frames
            .iter()
            .find(|frame| frame.contains(&id))
            .expect("the subprotocol socket got the post"),
    )
    .expect("frames are JSON envelopes");
    assert_eq!(
        streams_of(&protocol_frame),
        ["user"],
        "the subprotocol socket streams for ITS user: {protocol_frame}"
    );
    assert!(
        !protocol_frame.to_string().contains(bob_own.as_str()),
        "without leaking the internal uuid-named stream: {protocol_frame}"
    );
    let protocol_payload = frame_payload(&protocol_frame);
    assert_eq!(
        protocol_payload["account"]["pleroma"]["relationship"]["following"],
        json!(true),
        "the same hub event rendered Bob's own view of Alice's account: {protocol_payload}"
    );
}
