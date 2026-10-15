#![cfg(test)]

//! The streaming WebSocket over a real socket (`src/api/streaming.rs`).
//!
//! These are the suite's only tests that are not `oneshot` requests: a
//! WebSocket is a connection, not a request, so each test binds a listener on
//! `127.0.0.1:0`, serves the real application on it, and drives it with
//! `tokio-tungstenite`. The database is still a leased slot of the fixture
//! (decision 13), and the events under test are published on the hub the served
//! router reads — the same bus the endpoints will publish on.
//!
//! Delivery is ordered but not synchronous, so the negative assertions never
//! "wait and see": each publishes the event that must be skipped *before* a
//! control event on the same subscription, then reads until the control
//! arrives. The hub offers every event to every subscriber in order, so an
//! event that had no effect before the control never will.

use std::{net::SocketAddr, time::Duration};

use axum::{http::StatusCode, response::Response};
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::Value;
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
    api::streaming::MAX_SUBSCRIPTIONS,
    config::Config,
    models::{
        block, conversation_mute, domain_block, list,
        mute::{self, MuteOptions},
        user::{NewUser, User, insert},
    },
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
const PING_WAIT: Duration = Duration::from_secs(10);
/// The query string of a socket that watches the public timeline anonymously.
const PUBLIC: &str = "stream=public";
/// The opcode of a WebSocket `Ping` frame, as it arrives on the raw socket.
const PING_OPCODE: u8 = 0x9;
/// The opcode of a WebSocket `Close` frame.
const CLOSE_OPCODE: u8 = 0x8;

/// The application on a real port: the address to dial, the hub to publish on,
/// and the lease that keeps its database alive.
#[derive(Debug)]
struct Server {
    /// The loopback address the listener bound.
    addr: SocketAddr,
    /// The state the router runs on; its hub is the bus the tests publish on.
    state: AppState,
    /// The fixture slot `state` reads, held for the server's whole life so the
    /// pool behind it stays leased (decision 13).
    _db: TestDb,
    /// The `axum::serve` task, aborted when the test ends.
    task: JoinHandle<()>,
}

impl Server {
    /// Serves the real application over a leased slot and the default test
    /// configuration.
    async fn start() -> Self {
        Self::with_config(common::test_config()).await
    }

    /// Serves the real application with `config`, which is how the heartbeat
    /// tests get a one-second ping instead of the shipped
    /// [`PING_INTERVAL`](wired_be::api::streaming::PING_INTERVAL).
    async fn with_config(config: Config) -> Self {
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
    fn pool(&self) -> &PgPool {
        &self.state.db
    }

    /// The streaming endpoint's url with `query` appended after a `?`.
    fn url(&self, query: &str) -> String {
        if query.is_empty() {
            return format!("ws://{}/api/v1/streaming", self.addr);
        }
        format!("ws://{}/api/v1/streaming?{query}", self.addr)
    }

    /// Puts `event` on the hub the served router is reading.
    fn publish(&self, event: StreamEvent) {
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
type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A fresh local account named `nickname`.
///
/// # Panics
///
/// If the insert fails, e.g. because the nickname is taken.
async fn account(pool: &PgPool, nickname: &str) -> User {
    let ap_id = format!("https://test.example/users/{nickname}");
    insert(pool, &NewUser::local(nickname, &ap_id))
        .await
        .expect("user fixture inserts")
}

/// A user-bound bearer token granting `scopes`, through the shared fixtures.
async fn token(pool: &PgPool, user: &User, scopes: &str) -> String {
    common::create_app_and_token(pool, user, scopes).await
}

/// Opens `url`, panicking with the handshake failure if the server refused the
/// upgrade.
async fn open(url: &str) -> Client {
    let (client, _response) = connect_async(url)
        .await
        .unwrap_or_else(|err| panic!("the upgrade of {url} succeeds: {err}"));
    client
}

/// Opens `url` offering `protocol` as its `Sec-WebSocket-Protocol`, and returns
/// the socket with the status and the echoed subprotocol of the handshake
/// response.
async fn open_with_protocol(url: &str, protocol: &str) -> (Client, Response<Option<Vec<u8>>>) {
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
async fn refusal(url: &str) -> (StatusCode, Value) {
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
async fn send(client: &mut Client, text: &str) {
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

/// Blocks until the server has taken every frame this client sent.
///
/// The socket loop handles a client's frames in the order they arrive, and a
/// `subscribe` of a stream that does not exist is always answered with the
/// ticket's refusal frame — so that answer is the barrier: everything sent
/// before it has been applied by the time it is read. Without it, a test that
/// sends a `subscribe` and immediately publishes would race the loop against
/// the events it has not subscribed to yet.
async fn settled(client: &mut Client) {
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
async fn read_until(client: &mut Client, until: &str) -> Vec<String> {
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
fn assert_absent(frames: &[String], markers: &[&str]) {
    for marker in markers {
        assert!(
            !frames.iter().any(|frame| frame.contains(marker)),
            "`{marker}` was delivered: {frames:?}"
        );
    }
}

/// An `update` on `streams` whose entity carries `marker` as its id, for
/// `audience`.
fn event_on(streams: &[&str], marker: &str, audience: Audience) -> StreamEvent {
    StreamEvent {
        streams: streams.iter().copied().map(String::from).collect(),
        event: "update".to_owned(),
        payload: Some(format!(r#"{{"id":"{marker}"}}"#)),
        audience,
    }
}

/// A public `update` whose entity carries `marker` as its id.
fn public_update(marker: &str) -> StreamEvent {
    event_on(&["public"], marker, Audience::Public)
}

/// A public `update` whose entity is authored by `author` (as `acct`, which
/// names a domain when it holds an `@`) in the thread `thread` — the payload a
/// connection's filters judge.
fn authored_update(marker: &str, author: Uuid, acct: &str, thread: Uuid) -> StreamEvent {
    StreamEvent {
        payload: Some(format!(
            r#"{{"id":"{marker}","account":{{"id":"{author}","acct":"{acct}"}},"conversation_id":"{thread}"}}"#
        )),
        ..public_update(marker)
    }
}

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

/// A token in the query string authenticates the socket, and its `user` stream
/// carries only the events addressed to that user.
#[tokio::test]
async fn token_in_the_query_string_opens_the_user_stream() {
    let server = Server::start().await;
    let reader = account(server.pool(), "query-reader").await;
    let other = account(server.pool(), "query-other").await;
    let secret = token(server.pool(), &reader, "read:statuses").await;
    let mut client = open(&server.url(&format!("stream=user&access_token={secret}"))).await;

    server.publish(event_on(
        &["user"],
        "for-other",
        Audience::Users(vec![other.id]),
    ));
    server.publish(event_on(
        &["user"],
        "for-me",
        Audience::Users(vec![reader.id]),
    ));

    let frames = read_until(&mut client, "for-me").await;
    assert_absent(&frames, &["for-other"]);
    assert_eq!(
        frames.last().expect("the control frame"),
        r#"{"stream":["user"],"event":"update","payload":"{\"id\":\"for-me\"}"}"#,
        "the socket's own event arrives on its own stream"
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

    server.publish(event_on(
        &["user"],
        "via-subprotocol",
        Audience::Users(vec![reader.id]),
    ));
    assert!(
        next_text(&mut client).await.contains("via-subprotocol"),
        "and the socket streams for the user that token names"
    );
}

/// A `subscribe` frame opens a stream on a live socket and an `unsubscribe`
/// frame closes it again, with the events to prove both.
#[tokio::test]
async fn subscribe_and_unsubscribe_frames_move_a_stream_in_and_out() {
    let server = Server::start().await;
    let mut client = open(&server.url(PUBLIC)).await;

    send(
        &mut client,
        r#"{"type":"subscribe","stream":"hashtag","tag":"Rust"}"#,
    )
    .await;
    settled(&mut client).await;
    server.publish(event_on(&["hashtag:rust"], "tagged", Audience::Public));
    server.publish(public_update("public-control"));
    let tagged = read_until(&mut client, "public-control").await;
    assert_eq!(
        tagged.first().expect("the hashtag event"),
        r#"{"stream":["hashtag:rust"],"event":"update","payload":"{\"id\":\"tagged\"}"}"#,
        "the hashtag stream the frame opened, delivered — and the tag was normalized"
    );

    send(
        &mut client,
        r#"{"type":"unsubscribe","stream":"hashtag","tag":"rust"}"#,
    )
    .await;
    settled(&mut client).await;
    server.publish(event_on(&["hashtag:rust"], "dropped", Audience::Public));
    server.publish(public_update("public-after"));
    let after = read_until(&mut client, "public-after").await;
    assert_absent(&after, &["dropped"]);
}

/// A list stream is a topic of its own: the socket holding `list:<own id>` is
/// silent about the events of another list.
#[tokio::test]
async fn a_list_stream_delivers_its_own_list_only() {
    let server = Server::start().await;
    let reader = account(server.pool(), "list-reader").await;
    let mine = list::create(
        server.pool(),
        reader.id,
        &list::NewList {
            title: "Reading".to_owned(),
            ..list::NewList::default()
        },
    )
    .await
    .expect("the list fixture writes");
    let secret = token(server.pool(), &reader, "read:lists").await;
    let mut client = open(&server.url(&format!(
        "stream=list&list={}&access_token={secret}",
        mine.id
    )))
    .await;

    server.publish(event_on(&["list:1"], "other-list", Audience::Public));
    let mine_stream = format!("list:{}", mine.id);
    server.publish(event_on(
        &[mine_stream.as_str()],
        "my-list",
        Audience::Public,
    ));

    let frames = read_until(&mut client, "my-list").await;
    assert_absent(&frames, &["other-list"]);
}

/// A stream this endpoint does not know is refused with the ticket's frame, and
/// a private stream on an anonymous socket with the ticket's other one; in both
/// cases the connection stays open and keeps what it had.
#[tokio::test]
async fn a_refused_subscription_answers_the_pinned_frame() {
    let server = Server::start().await;
    let mut client = open(&server.url(PUBLIC)).await;

    send(&mut client, r#"{"type":"subscribe","stream":"nonsense"}"#).await;
    let unknown = next_text(&mut client).await;
    send(&mut client, r#"{"type":"subscribe","stream":"user"}"#).await;
    let unauthorized = next_text(&mut client).await;

    assert_eq!(
        unknown, r#"{"error":"bad_request","error_description":"unknown stream"}"#,
        "an unknown stream is the ticket's bad_request frame"
    );
    assert_eq!(
        unauthorized, r#"{"error":"unauthorized"}"#,
        "a private stream without a user is the ticket's unauthorized frame"
    );

    server.publish(public_update("still-streaming"));
    assert!(
        next_text(&mut client).await.contains("still-streaming"),
        "and the socket is alive with the stream it came in on"
    );
}

/// The subscription past [`MAX_SUBSCRIPTIONS`] is refused and delivers nothing,
/// while the streams the socket already holds keep working — the handshake's own
/// `stream=public` counting against the cap like every other.
#[tokio::test]
async fn the_subscription_past_the_cap_is_refused() {
    let server = Server::start().await;
    let mut client = open(&server.url(PUBLIC)).await;

    for index in 0..MAX_SUBSCRIPTIONS - 1 {
        send(
            &mut client,
            &format!(r#"{{"type":"subscribe","stream":"hashtag","tag":"cap{index}"}}"#),
        )
        .await;
    }
    send(
        &mut client,
        r#"{"type":"subscribe","stream":"hashtag","tag":"one-too-many"}"#,
    )
    .await;

    assert_eq!(
        next_text(&mut client).await,
        r#"{"error":"bad_request","error_description":"too many subscriptions"}"#,
        "the cap is answered the way an unknown stream is"
    );
    settled(&mut client).await;

    server.publish(event_on(&["hashtag:cap0"], "held", Audience::Public));
    server.publish(event_on(
        &["hashtag:one-too-many"],
        "refused",
        Audience::Public,
    ));
    let frames = read_until(&mut client, "held").await;
    assert_absent(&frames, &["refused"]);
}

/// `pleroma:authenticate` gives an anonymous socket a user without a reconnect,
/// and the private streams open for it from then on.
#[tokio::test]
async fn an_anonymous_socket_authenticates_without_a_reconnect() {
    let server = Server::start().await;
    let reader = account(server.pool(), "late-auth").await;
    let other = account(server.pool(), "late-other").await;
    let secret = token(server.pool(), &reader, "read").await;
    let mut client = open(&server.url(PUBLIC)).await;

    send(
        &mut client,
        &format!(r#"{{"type":"pleroma:authenticate","token":"{secret}"}}"#),
    )
    .await;
    send(&mut client, r#"{"type":"subscribe","stream":"user"}"#).await;
    settled(&mut client).await;
    server.publish(event_on(
        &["user"],
        "for-other",
        Audience::Users(vec![other.id]),
    ));
    server.publish(event_on(
        &["user"],
        "after-auth",
        Audience::Users(vec![reader.id]),
    ));

    let frames = read_until(&mut client, "after-auth").await;
    assert_absent(&frames, &["for-other"]);
}

/// A `pleroma:authenticate` whose token names nobody is refused, and the socket
/// stays as anonymous as it connected.
#[tokio::test]
async fn authenticating_with_a_bad_token_is_refused() {
    let server = Server::start().await;
    let mut client = open(&server.url(PUBLIC)).await;

    send(
        &mut client,
        r#"{"type":"pleroma:authenticate","token":"nope"}"#,
    )
    .await;

    assert_eq!(
        next_text(&mut client).await,
        r#"{"error":"unauthorized"}"#,
        "a bad token authenticates nothing"
    );
    send(&mut client, r#"{"type":"subscribe","stream":"user"}"#).await;
    assert_eq!(
        next_text(&mut client).await,
        r#"{"error":"unauthorized"}"#,
        "and the socket is still anonymous"
    );
}

/// A reader never sees an event authored by an account it muted or blocked,
/// posted from a domain it blocked, or sitting in a thread it muted — the
/// per-connection filters, read once when the socket opened.
#[tokio::test]
async fn a_readers_own_filters_suppress_events() {
    let server = Server::start().await;
    let reader = account(server.pool(), "filtering-reader").await;
    let muted = account(server.pool(), "filter-muted").await;
    let blocked = account(server.pool(), "filter-blocked").await;
    let muted_thread = Uuid::now_v7();
    let secret = token(server.pool(), &reader, "read").await;
    let _muted = mute::mute(server.pool(), &reader, &muted, MuteOptions::default())
        .await
        .expect("the mute fixture writes");
    let _blocked = block::block(
        server.pool(),
        &server.state.config.base_url,
        &reader,
        &blocked,
    )
    .await
    .expect("the block fixture writes");
    domain_block::block_domain(server.pool(), reader.id, "spam.example")
        .await
        .expect("the domain block fixture writes");
    conversation_mute::mute(server.pool(), reader.id, muted_thread)
        .await
        .expect("the thread mute fixture writes");
    let mut client = open(&server.url(&format!("{PUBLIC}&access_token={secret}"))).await;

    server.publish(authored_update(
        "by-muted",
        muted.id,
        "muted",
        Uuid::now_v7(),
    ));
    server.publish(authored_update(
        "by-blocked",
        blocked.id,
        "blocked",
        Uuid::now_v7(),
    ));
    server.publish(authored_update(
        "from-blocked-domain",
        Uuid::now_v7(),
        "noisy@spam.example",
        Uuid::now_v7(),
    ));
    server.publish(authored_update(
        "in-muted-thread",
        Uuid::now_v7(),
        "talkative",
        muted_thread,
    ));
    server.publish(public_update("control"));

    let frames = read_until(&mut client, "control").await;
    assert_absent(
        &frames,
        &[
            "by-muted",
            "by-blocked",
            "from-blocked-domain",
            "in-muted-thread",
        ],
    );
}

/// An `Everyone` event reaches an authenticated socket and no anonymous one; a
/// `Public` event reaches both.
#[tokio::test]
async fn audience_decides_between_an_anonymous_and_an_authenticated_socket() {
    let server = Server::start().await;
    let reader = account(server.pool(), "audience-reader").await;
    let secret = token(server.pool(), &reader, "read").await;
    let mut anonymous = open(&server.url(PUBLIC)).await;
    let mut authenticated = open(&server.url(&format!("{PUBLIC}&access_token={secret}"))).await;

    server.publish(event_on(&["public"], "for-everyone", Audience::Everyone));
    server.publish(public_update("for-anyone"));

    let both = read_until(&mut authenticated, "for-anyone").await;
    assert!(
        both.iter().any(|frame| frame.contains("for-everyone")),
        "an authenticated socket sees an Everyone event: {both:?}"
    );
    let alone = read_until(&mut anonymous, "for-anyone").await;
    assert_absent(&alone, &["for-everyone"]);
}

/// Every heartbeat period the server pings the socket, and the client's
/// automatic pong keeps the connection alive: what a proxy needs to see to leave
/// a streaming connection alone.
#[tokio::test]
async fn the_server_pings_a_live_socket() {
    let config = Config {
        streaming_ping_interval_secs: 1,
        ..common::test_config()
    };
    let server = Server::with_config(config).await;
    let mut client = open(&server.url(PUBLIC)).await;

    let frame = timeout(PING_WAIT, client.next())
        .await
        .expect("a ping arrives inside its interval")
        .expect("the socket is healthy")
        .expect("the frame arrives");

    assert!(
        matches!(frame, Frame::Ping(_)),
        "the heartbeat is a WebSocket Ping, not a frame the endpoint invented: {frame:?}"
    );
}

/// A socket that never answers a ping is closed. This test handshakes over a raw
/// TCP connection so that nothing answers for it, and reads frames until the
/// server ends the connection.
#[tokio::test]
async fn a_socket_that_never_answers_pings_is_closed() {
    let config = Config {
        streaming_ping_interval_secs: 1,
        ..common::test_config()
    };
    let server = Server::with_config(config).await;
    let mut raw = handshake_by_hand(&server, PUBLIC).await;

    let saw = timeout(PING_WAIT, watch_frames(&mut raw))
        .await
        .expect("the server ends the silent socket within its two intervals");

    assert!(saw.ping, "the server pinged a socket that never answered");
    assert!(
        saw.close,
        "and said so with a close frame before hanging up: {saw:?}"
    );
}

/// The frame opcodes a raw client saw before its server hung up.
#[derive(Debug, Default)]
struct Saw {
    /// A heartbeat ping arrived.
    ping: bool,
    /// A close frame arrived.
    close: bool,
}

/// Reads frames from `raw` until the server ends the connection, noting the
/// opcodes that arrived on the way.
async fn watch_frames(raw: &mut TcpStream) -> Saw {
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
async fn handshake_by_hand(server: &Server, query: &str) -> TcpStream {
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
