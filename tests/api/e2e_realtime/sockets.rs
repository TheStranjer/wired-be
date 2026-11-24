//! The WebSocket client and the world builders: opening and reading
//! streaming sockets, the hub stream names, and the accounts and writes the
//! scenarios seed their worlds with.

use std::time::Duration;

use axum::response::Response;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::{
    net::TcpStream,
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message as Frame, client::IntoClientRequest as _},
};
use uuid::Uuid;
use wired_be::models::user::{Flag, NewUser, User};

use super::server::{Server, Verb, action_ok, write_ok};
use crate::common;

/// How long one frame may take to arrive before a test gives up: every
/// event a scenario waits for is produced by an HTTP call that has already
/// returned — the hub holds the event before the response does — so a frame
/// that is late at all is late by microseconds.
const READ_WAIT: Duration = Duration::from_secs(20);

// ───────────────────────  The WebSocket client  ───────────────────────

/// A connected streaming client. Every test ends it by dropping, which
/// closes the connection out from under the server's loop.
pub(super) type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Opens `url`, panicking with the handshake failure if the server refused
/// the upgrade.
pub(super) async fn open(url: &str) -> Client {
    let (client, _response) = connect_async(url)
        .await
        .unwrap_or_else(|err| panic!("the upgrade of {url} succeeds: {err}"));
    client
}

/// Opens `url` offering `protocol` as its `Sec-WebSocket-Protocol`, and
/// returns the socket beside the handshake answer.
pub(super) async fn open_with_protocol(
    url: &str,
    protocol: &str,
) -> (Client, Response<Option<Vec<u8>>>) {
    let mut request = url
        .into_client_request()
        .expect("the test url is a valid request");
    let _offered = request.headers_mut().insert(
        "sec-websocket-protocol",
        protocol.parse().expect("a bearer token is a header value"),
    );
    connect_async(request)
        .await
        .unwrap_or_else(|err| panic!("the upgrade of {url} succeeds: {err}"))
}

/// Writes one text frame to the socket.
pub(super) async fn send(client: &mut Client, text: &str) {
    client
        .send(Frame::Text(text.to_owned().into()))
        .await
        .expect("the socket takes a text frame");
}

/// The next text frame, skipping the protocol frames a heartbeat produces.
async fn next_text(client: &mut Client) -> String {
    let deadline = Instant::now() + READ_WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "no text frame arrived within {READ_WAIT:?}"
        );
        match timeout(remaining, client.next()).await {
            Err(elapsed) => panic!("no frame arrived within {READ_WAIT:?}: {elapsed}"),
            Ok(None) => panic!("the server closed the socket"),
            Ok(Some(Err(err))) => panic!("the socket failed: {err}"),
            Ok(Some(Ok(Frame::Text(text)))) => return text.as_str().to_owned(),
            Ok(Some(Ok(_protocol))) => {}
        }
    }
}

/// The next text frame, parsed as Mastodon's JSON envelope.
pub(super) async fn next_json(client: &mut Client) -> Value {
    serde_json::from_str(&next_text(client).await).expect("every streaming frame is JSON")
}

/// Blocks until the server has taken every frame this client sent: a
/// `subscribe` of a stream that does not exist is always answered with the
/// refusal frame, and the socket loop handles client frames in arrival
/// order — so that answer is the barrier.
pub(super) async fn settled(client: &mut Client) {
    send(
        client,
        r#"{"type":"subscribe","stream":"this-stream-does-not-exist"}"#,
    )
    .await;
    assert_eq!(
        next_text(client).await,
        r#"{"error":"bad_request","error_description":"unknown stream"}"#,
        "the barrier frame is the refusal itself"
    );
}

/// Every text frame up to and including the first that carries `until`:
/// the control fact a scenario produces last bounds the wait for
/// everything before it, because the HTTP write that published the earlier
/// event returned before the one that published the control began.
pub(super) async fn read_until(client: &mut Client, until: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    loop {
        let frame = next_text(client).await;
        let arrived = frame.contains(until);
        seen.push(frame);
        if arrived {
            return seen;
        }
    }
}

/// Reads frames until one carries `marker` and returns that frame parsed;
/// a test that needs what came before calls [`read_until`] instead.
pub(super) async fn read_until_json(client: &mut Client, marker: &str) -> Value {
    let frames = read_until(client, marker).await;
    let frame = frames
        .iter()
        .rfind(|frame| frame.contains(marker))
        .expect("the control frame arrived");
    serde_json::from_str(frame).expect("every streaming frame is JSON")
}

/// The frame's `stream` array, as strings.
pub(super) fn streams_of(frame: &Value) -> Vec<&str> {
    frame["stream"]
        .as_array()
        .expect("the envelope names its streams")
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<&str>>>()
        .expect("every stream name is a string")
}

/// Whether `frames` carries none of `markers`, with the frames themselves
/// in the failure message.
pub(super) fn assert_absent(frames: &[String], markers: &[&str]) {
    for marker in markers {
        assert!(
            !frames.iter().any(|frame| frame.contains(marker)),
            "`{marker}` was delivered: {frames:?}"
        );
    }
}

/// The JSON payload the frame's envelope carries, parsed.
pub(super) fn frame_payload(frame: &Value) -> Value {
    serde_json::from_str(
        frame["payload"]
            .as_str()
            .expect("the envelope carries its entity as a payload string"),
    )
    .expect("the payload is the entity JSON")
}

/// The `user:{id}` hub stream name.
pub(super) fn user_stream(id: Uuid) -> String {
    format!("user:{id}")
}

/// The `user:notification:{id}` hub stream name.
pub(super) fn notification_stream(id: Uuid) -> String {
    format!("user:notification:{id}")
}

/// The `direct:{id}` hub stream name.
pub(super) fn direct_stream(id: Uuid) -> String {
    format!("direct:{id}")
}

/// The `list:{id}` hub stream name.
pub(super) fn list_stream(id: Uuid) -> String {
    format!("list:{id}")
}

// ─────────────────────────  The world builders  ─────────────────────────

/// A fresh local account named `nickname` beside a bearer token granting
/// `scopes`.
pub(super) async fn account(srv: &Server, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(srv.pool(), nickname).await;
    let token = common::create_app_and_token(srv.pool(), &created, scopes).await;
    (created, token)
}

/// A fresh local account displaying as `name`, with a `scopes` token: the
/// shape a profile with a display name has (the push title reads it).
pub(super) async fn named_account(
    srv: &Server,
    nickname: &str,
    name: &str,
    scopes: &str,
) -> (User, String) {
    let created = common::create_local_user_with(srv.pool(), nickname, |user| NewUser {
        name: name.to_owned(),
        ..user
    })
    .await;
    let token = common::create_app_and_token(srv.pool(), &created, scopes).await;
    (created, token)
}

/// A fresh local admin named `nickname` with a `scopes` token.
pub(super) async fn admin(srv: &Server, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(srv.pool(), nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(srv.pool(), &created, scopes).await;
    (created, token)
}

/// The `POST /api/v1/statuses` of `text` at `visibility`, answered entity.
pub(super) async fn post_status(srv: &Server, token: &str, text: &str, visibility: &str) -> Value {
    write_ok(
        srv,
        Verb::Post,
        "/api/v1/statuses",
        token,
        &json!({ "status": text, "visibility": visibility }),
    )
    .await
}

/// `token`'s user follows the account `target`, asserted granted.
pub(super) async fn follow(srv: &Server, token: &str, target: Uuid) {
    let relationship = action_ok(srv, &format!("/api/v1/accounts/{target}/follow"), token).await;
    assert_eq!(
        relationship["following"],
        json!(true),
        "the unlocked local target grants the follow on the spot"
    );
}

/// `token`'s user favourites the status `status`.
pub(super) async fn favourite(srv: &Server, token: &str, status: &str) {
    let body = action_ok(srv, &format!("/api/v1/statuses/{status}/favourite"), token).await;
    assert_eq!(body["favourited"], json!(true), "the like lands");
}
