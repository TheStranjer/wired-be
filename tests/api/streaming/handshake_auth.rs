//! The upgrade and its authentication: the health probe and the plain
//! `GET` refusal on the streaming path, a public socket that connects with
//! no token at all and speaks Mastodon's envelopes, the private streams and
//! the bad token refused *before* the upgrade, and the two bearer carriers —
//! the `access_token` query string and the echoed `Sec-WebSocket-Protocol`
//! value — that open a user's own stream.

use axum::http::StatusCode;
use serde_json::Value;
use uuid::Uuid;
use wired_be::streaming::hub::{Audience, StreamEvent};

use super::harness::{
    PUBLIC, Server, account, assert_absent, event_on, next_text, open, open_with_protocol,
    public_update, read_until, refusal, token,
};
use crate::common;

/// `/api/v1/streaming/health`: `200` with the text body `OK`, the probe
/// Mastodon's clients poll before opening a socket.
#[tokio::test]
async fn health_probe_answers_ok() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let response = common::TestRequest::get("/api/v1/streaming/health")
        .send(&app)
        .await;

    common::assert_answer(&response, StatusCode::OK, "the streaming probe");
    assert_eq!(response.text(), "OK", "the probe's body is OK");
    assert_eq!(
        response.header("content-type"),
        Some("text/plain; charset=utf-8"),
        "as text, not as JSON"
    );
}

/// A plain `GET` on the streaming path is no upgrade, and is refused with the
/// JSON error every other failure of this API answers with.
#[tokio::test]
async fn a_plain_get_on_the_streaming_path_is_a_json_400() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let response = common::TestRequest::get("/api/v1/streaming")
        .send(&app)
        .await;

    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "a request without an upgrade is refused: {}",
        response.text()
    );
    assert_eq!(
        response.json_value().get("error").and_then(Value::as_str),
        Some("Expected a WebSocket upgrade"),
        "the refusal names what the request lacked"
    );
}

/// A public stream needs no token at all, and a published update arrives in
/// Mastodon's envelope: the streams array, the event name, and the entity as a
/// JSON *string*.
#[tokio::test]
async fn public_stream_connects_without_a_token() {
    let server = Server::start().await;
    let mut client = open(&server.url(PUBLIC)).await;

    server.publish(public_update("hello"));

    assert_eq!(
        next_text(&mut client).await,
        r#"{"stream":["public"],"event":"update","payload":"{\"id\":\"hello\"}"}"#,
        "the payload stays a JSON string inside the envelope, as Mastodon sends it"
    );
}

/// A `delete` event carries the deleted status's id as its payload, where an
/// `update` carries an entity.
#[tokio::test]
async fn a_delete_event_carries_the_status_id() {
    let server = Server::start().await;
    let id = Uuid::now_v7().to_string();
    let mut client = open(&server.url(PUBLIC)).await;

    server.publish(StreamEvent {
        event: "delete".to_owned(),
        payload: Some(id.clone()),
        ..public_update(&id)
    });

    assert_eq!(
        next_text(&mut client).await,
        format!(r#"{{"stream":["public"],"event":"delete","payload":"{id}"}}"#),
        "the envelope is Mastodon's, with the id as the payload string"
    );
}

/// Every private stream of the ticket is refused *before* the upgrade when the
/// socket presents no token: 401 with the pinned body.
#[tokio::test]
async fn private_streams_without_a_token_are_refused() {
    let server = Server::start().await;

    for stream in [
        "user",
        "user:notification",
        "user:pleroma_chat",
        "direct",
        "list",
    ] {
        let (status, body) = refusal(&server.url(&format!("stream={stream}"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "`{stream}` needs a token");
        assert_eq!(
            body.get("error").and_then(Value::as_str),
            Some("Invalid access token"),
            "the refusal wording for `{stream}`"
        );
    }
}

/// A token that names no live user is refused the same way, even on a stream the
/// socket could otherwise have watched anonymously.
#[tokio::test]
async fn an_invalid_token_rejects_the_upgrade() {
    let server = Server::start().await;

    let (status, body) = refusal(&server.url(&format!("{PUBLIC}&access_token=nope"))).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED, "a bad token never opens");
    assert_eq!(
        body.get("error").and_then(Value::as_str),
        Some("Invalid access token"),
        "with the ticket's body"
    );
}

/// A token in the query string authenticates the socket, its bare
/// `stream=user` subscription is held under the canonical `user:<reader uuid>`
/// hub name, and it receives the event whose streams name `user:<its uuid>`
/// while staying silent about one naming another user's `user:<uuid>`.
#[tokio::test]
async fn token_in_the_query_string_opens_the_user_stream() {
    let server = Server::start().await;
    let reader = account(server.pool(), "query-reader").await;
    let other = account(server.pool(), "query-other").await;
    let own_stream = format!("user:{}", reader.id);
    let other_stream = format!("user:{}", other.id);
    let secret = token(server.pool(), &reader, "read:statuses").await;
    let mut client = open(&server.url(&format!("stream=user&access_token={secret}"))).await;

    server.publish(event_on(
        &[other_stream.as_str()],
        "for-other",
        Audience::Users(vec![other.id]),
    ));
    server.publish(event_on(
        &[own_stream.as_str()],
        "for-me",
        Audience::Users(vec![reader.id]),
    ));

    let frames = read_until(&mut client, "for-me").await;
    assert_absent(&frames, &["for-other", &other_stream]);
    assert_eq!(
        frames.last().expect("the control frame"),
        r#"{"stream":["user"],"event":"update","payload":"{\"id\":\"for-me\"}"}"#,
        "the socket's own event arrives on its public `user` stream, not the uuid-named one"
    );
}

/// The Mastodon web client's handshake: the token is the first
/// `Sec-WebSocket-Protocol` value, and the server echoes that same value back as
/// the chosen subprotocol — a browser drops a handshake that answers with a
/// protocol it did not offer.
#[tokio::test]
async fn token_in_the_subprotocol_is_echoed_back() {
    let server = Server::start().await;
    let reader = account(server.pool(), "subprotocol-reader").await;
    let secret = token(server.pool(), &reader, "read").await;

    let (mut client, response) = open_with_protocol(&server.url("stream=user"), &secret).await;

    assert_eq!(
        response.status(),
        StatusCode::SWITCHING_PROTOCOLS,
        "the upgrade went through on the token alone"
    );
    assert_eq!(
        response.headers().get("sec-websocket-protocol"),
        Some(&secret.as_str().parse().expect("a token is a header value")),
        "the offered protocol comes back as the chosen one"
    );

    let own_stream = format!("user:{}", reader.id);
    server.publish(event_on(
        &[own_stream.as_str()],
        "via-subprotocol",
        Audience::Users(vec![reader.id]),
    ));
    assert!(
        next_text(&mut client).await.contains("via-subprotocol"),
        "and the socket streams for the user that token names"
    );
}
