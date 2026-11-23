//! Shared consts and helpers for every submodule of the `streaming` area:
//! the application served on a real port, the socket openers, readers and
//! writers, the `StreamEvent` makers the tests publish, the raw-handshake
//! helpers the heartbeat tests read frames with, and the envelope readers
//! the live-event tests assert through. Everything here is `pub(super)`,
//! and a submodule imports exactly the members it calls.

use std::{net::SocketAddr, time::Duration};

use axum::{http::StatusCode, response::Response};
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{
    net::TcpStream,
    task::JoinHandle,
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message as Frame, client::IntoClientRequest as _, error::Error as WsError},
};
use uuid::Uuid;
use wired_be::{
    config::Config,
    models::user::{NewUser, User, insert},
    services::create_status::CreateStatusParams,
    state::AppState,
    streaming::hub::{Audience, StreamEvent},
    test_db::TestDb,
};

use crate::common;

/// How long one frame may take to arrive before a test gives up: the hub
/// delivers in-process, so a frame that is late at all is late by microseconds.
const READ_WAIT: Duration = Duration::from_secs(20);
/// How long to wait for the heartbeat's first ping, on the tests' own one-second
/// interval: ten intervals of slack, none of which a healthy server needs.
pub(super) const PING_WAIT: Duration = Duration::from_secs(10);
/// The query string of a socket that watches the public timeline anonymously.
pub(super) const PUBLIC: &str = "stream=public";
/// The opcode of a WebSocket `Ping` frame, as it arrives on the raw socket.
const PING_OPCODE: u8 = 0x9;
/// The opcode of a WebSocket `Close` frame.
const CLOSE_OPCODE: u8 = 0x8;

/// The application on a real port: the address to dial, the hub to publish on,
/// and the lease that keeps its database alive.
#[derive(Debug)]
pub(super) struct Server {
    /// The loopback address the listener bound.
    addr: SocketAddr,
    /// The state the router runs on; its hub is the bus the tests publish on.
    pub(super) state: AppState,
    /// The fixture slot `state` reads, held for the server's whole life so the
    /// pool behind it stays leased (decision 13).
    _db: TestDb,
    /// The `axum::serve` task, aborted when the test ends.
    task: JoinHandle<()>,
}

impl Server {
    /// Serves the real application over a leased slot and the default test
    /// configuration.
    pub(super) async fn start() -> Self {
        Self::with_config(common::test_config()).await
    }

    /// Serves the real application with `config`, which is how the heartbeat
    /// tests get a one-second ping instead of the shipped
    /// [`PING_INTERVAL`](wired_be::api::streaming::PING_INTERVAL).
    pub(super) async fn with_config(config: Config) -> Self {
        let db = common::test_db().await;
        let state = common::test_app_state(db.pool().clone(), config);
        let app = wired_be::app(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a free loopback port");
        let addr = listener.local_addr().expect("the bound address");
        let task = tokio::spawn(async move {
            let _served = axum::serve(listener, app).await;
        });
        Self {
            addr,
            state,
            _db: db,
            task,
        }
    }

    /// The pool the served router reads.
    pub(super) fn pool(&self) -> &PgPool {
        &self.state.db
    }

    /// The streaming endpoint's url with `query` appended after a `?`.
    pub(super) fn url(&self, query: &str) -> String {
        if query.is_empty() {
            return format!("ws://{}/api/v1/streaming", self.addr);
        }
        format!("ws://{}/api/v1/streaming?{query}", self.addr)
    }

    /// Puts `event` on the hub the served router is reading.
    pub(super) fn publish(&self, event: StreamEvent) {
        self.state.hub.publish(event);
    }
}

/// A streaming socket ends with its server: the serve task goes when it does.
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A connected client. Every test ends it by dropping, which closes the
/// connection out from under the server's loop.
pub(super) type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A fresh local account named `nickname`.
///
/// # Panics
///
/// If the insert fails, e.g. because the nickname is taken.
pub(super) async fn account(pool: &PgPool, nickname: &str) -> User {
    let ap_id = format!("https://test.example/users/{nickname}");
    insert(pool, &NewUser::local(nickname, &ap_id))
        .await
        .expect("user fixture inserts")
}

/// A user-bound bearer token granting `scopes`, through the shared fixtures.
pub(super) async fn token(pool: &PgPool, user: &User, scopes: &str) -> String {
    common::create_app_and_token(pool, user, scopes).await
}

/// Opens `url`, panicking with the handshake failure if the server refused the
/// upgrade.
pub(super) async fn open(url: &str) -> Client {
    let (client, _response) = connect_async(url)
        .await
        .unwrap_or_else(|err| panic!("the upgrade of {url} succeeds: {err}"));
    client
}

/// Opens `url` offering `protocol` as its `Sec-WebSocket-Protocol`, and returns
/// the socket with the status and the echoed subprotocol of the handshake
/// response.
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
    let (client, response) = connect_async(request)
        .await
        .unwrap_or_else(|err| panic!("the upgrade of {url} succeeds: {err}"));
    let echoed = response
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    drop(echoed);
    (client, response)
}

/// The status and JSON body of a refused upgrade, for `url`.
///
/// # Panics
///
/// If the endpoint answers the upgrade at all, or refuses it with anything but
/// an HTTP status and a JSON body.
pub(super) async fn refusal(url: &str) -> (StatusCode, Value) {
    let refused = connect_async(url)
        .await
        .expect_err("the endpoint refuses the upgrade");
    let WsError::Http(response) = refused else {
        panic!("a refused upgrade is an HTTP answer, not {refused}");
    };
    let status = response.status();
    let text = String::from_utf8_lossy(&response.body().clone().unwrap_or_default()).into_owned();
    let json: Value = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("the refusal of {url} is JSON, not {text:?}: {err}"));
    (status, json)
}

/// Writes one text frame to the socket.
pub(super) async fn send(client: &mut Client, text: &str) {
    client
        .send(Frame::Text(text.to_owned().into()))
        .await
        .expect("the socket takes a text frame");
}

/// The next text frame, skipping the protocol frames a heartbeat produces.
///
/// # Panics
///
/// If nothing arrives within [`READ_WAIT`], or the socket is gone.
pub(super) async fn next_text(client: &mut Client) -> String {
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

/// Blocks until the server has taken every frame this client sent.
///
/// The socket loop handles a client's frames in the order they arrive, and a
/// `subscribe` of a stream that does not exist is always answered with the
/// ticket's refusal frame — so that answer is the barrier: everything sent
/// before it has been applied by the time it is read. Without it, a test that
/// sends a `subscribe` and immediately publishes would race the loop against
/// the events it has not subscribed to yet.
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

/// Every text frame up to and including the first that carries `until`: the
/// control event a test publishes last bounds the wait for everything before it.
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

/// Whether `frames` carries none of `markers`, with the frames themselves in the
/// failure message.
pub(super) fn assert_absent(frames: &[String], markers: &[&str]) {
    for marker in markers {
        assert!(
            !frames.iter().any(|frame| frame.contains(marker)),
            "`{marker}` was delivered: {frames:?}"
        );
    }
}

/// An `update` on `streams` whose entity carries `marker` as its id, for
/// `audience`.
pub(super) fn event_on(streams: &[&str], marker: &str, audience: Audience) -> StreamEvent {
    StreamEvent {
        streams: streams.iter().copied().map(String::from).collect(),
        event: "update".to_owned(),
        payload: Some(format!(r#"{{"id":"{marker}"}}"#)),
        audience,
        render: None,
        local_only: false,
    }
}

/// A public `update` whose entity carries `marker` as its id.
pub(super) fn public_update(marker: &str) -> StreamEvent {
    event_on(&["public"], marker, Audience::Public)
}

/// An `Everyone` `update` on `streams` flagged `local_only`, whose entity
/// carries `marker` as its id: for authenticated local viewers only.
pub(super) fn local_only_update(streams: &[&str], marker: &str) -> StreamEvent {
    StreamEvent {
        local_only: true,
        ..event_on(streams, marker, Audience::Everyone)
    }
}

/// A public `update` whose entity is authored by `author` (as `acct`, which
/// names a domain when it holds an `@`) in the thread `thread` — the payload a
/// connection's filters judge.
pub(super) fn authored_update(marker: &str, author: Uuid, acct: &str, thread: Uuid) -> StreamEvent {
    StreamEvent {
        payload: Some(format!(
            r#"{{"id":"{marker}","account":{{"id":"{author}","acct":"{acct}"}},"conversation_id":"{thread}"}}"#
        )),
        ..public_update(marker)
    }
}

/// The frame opcodes a raw client saw before its server hung up.
#[derive(Debug, Default)]
pub(super) struct Saw {
    /// A heartbeat ping arrived.
    pub(super) ping: bool,
    /// A close frame arrived.
    pub(super) close: bool,
}

/// Reads frames from `raw` until the server ends the connection, noting the
/// opcodes that arrived on the way.
pub(super) async fn watch_frames(raw: &mut TcpStream) -> Saw {
    let mut saw = Saw::default();
    while let Some(opcode) = read_raw_frame(raw).await {
        if opcode == PING_OPCODE {
            saw.ping = true;
        }
        if opcode == CLOSE_OPCODE {
            saw.close = true;
        }
    }
    saw
}

/// Performs the WebSocket handshake by hand, so that nothing answers on the
/// client's behalf — not even a pong. Returns the live TCP connection, past the
/// `101` answer.
pub(super) async fn handshake_by_hand(server: &Server, query: &str) -> TcpStream {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = TcpStream::connect(server.addr)
        .await
        .expect("the server accepts a connection");
    let request = format!(
        "GET /api/v1/streaming?{query} HTTP/1.1\r\nHost: {addr}\r\n\
         Upgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        addr = server.addr
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("the handshake goes out");
    let mut answer: Vec<u8> = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        let got = stream
            .read(&mut byte)
            .await
            .expect("the handshake answer comes back");
        assert_ne!(
            got, 0,
            "the server hung up on the hand-built handshake: {answer:?}"
        );
        answer.push(byte[0]);
        if answer.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&answer).into_owned();
    assert!(
        text.contains("101"),
        "the hand-built handshake upgraded: {text}"
    );
    stream
}

/// Reads one server frame and returns its opcode, `None` once the connection is
/// over. Server frames are never masked, and the only frames this test's socket
/// sees — a ping, a close and short text frames — fit in the 7-bit length, so
/// the payload is read and dropped.
async fn read_raw_frame(stream: &mut TcpStream) -> Option<u8> {
    use tokio::io::AsyncReadExt as _;

    let mut header = [0_u8; 2];
    if stream.read_exact(&mut header).await.is_err() {
        return None;
    }
    let opcode = header[0] & 0x0f;
    let length = header[1] & 0x7f;
    let body = usize::from(length);
    if body > 0 {
        let mut payload = vec![0_u8; body];
        if stream.read_exact(&mut payload).await.is_err() {
            return None;
        }
    }
    Some(opcode)
}

/// The next text frame, parsed as Mastodon's JSON envelope.
pub(super) async fn next_json(client: &mut Client) -> Value {
    serde_json::from_str(&next_text(client).await).expect("every streaming frame is JSON")
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

/// A create body for a public status of `text`, through the service's own
/// parameters type.
pub(super) fn create_body(text: &str) -> CreateStatusParams {
    serde_json::from_value(json!({ "status": text, "visibility": "public" }))
        .expect("the test body is a create body")
}

/// The post the lifecycle test creates, asserted in the streamed entity.
pub(super) const LIFECYCLE_TEXT: &str = "a status that streams live";

/// Asserts an `update` frame is Mastodon's arrival event for the status
/// `id`, its payload the rendered public-view entity with the lifecycle
/// text in its content.
pub(super) fn assert_update_carries(frame: &Value, id: &str) {
    assert_eq!(frame["event"], "update", "the socket sees an update");
    let payload = frame["payload"]
        .as_str()
        .expect("the update carries a payload string");
    let entity: Value = serde_json::from_str(payload).expect("the payload is the entity JSON");
    assert_eq!(
        entity["id"], id,
        "the payload is the created status's rendered entity"
    );
    assert_eq!(
        entity["visibility"], "public",
        "rendered as the socket's reader sees it"
    );
    assert!(
        entity["content"]
            .as_str()
            .is_some_and(|content| content.contains(LIFECYCLE_TEXT)),
        "the content rides the entity: {entity}"
    );
}

/// The frame among `frames` whose envelope names event `name`, parsed.
///
/// # Panics
///
/// If no frame carries that event or its envelope is not JSON.
pub(super) fn parse_event(frames: &[String], name: &str) -> Value {
    let frame = frames
        .iter()
        .find(|frame| frame.contains(&format!(r#""event":"{name}""#)))
        .unwrap_or_else(|| panic!("no `{name}` frame among {frames:?}"));
    serde_json::from_str(frame).expect("the frame is a JSON envelope")
}
