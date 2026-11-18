#![cfg(test)]

//! End-to-end realtime scenarios (WBE-226): the streaming WebSocket driven
//! through a real listener, Web Push delivered end to end against a mock
//! push service, the client-to-server `ActivityPub` walk, a `Link`-header
//! conformance sweep over every shared-`PageParams` listing, and the
//! emoji-pack lifecycles.
//!
//! Unlike every other module of this crate, this one serves the real
//! application over a `TcpListener` bound on `127.0.0.1:0` and talks to it
//! over actual sockets: `tokio-tungstenite` for the streaming frames and
//! `reqwest` for HTTP, because these scenarios are precisely the ones a
//! `oneshot` request cannot express — a WebSocket needs the live connection
//! to hand over, a push delivery leaves the process through the instance's
//! own outbound client, and the `Link` conformance sweep reads response
//! headers the way a remote client does. Every fact a scenario observes is
//! produced through the public HTTP API; the databases are still leased
//! slots of the fixture (decision 13), held by the served app for its whole
//! life, and the streaming events ride the hub the served router reads.
//!
//! The per-endpoint refusals live in the area files (`streaming.rs`,
//! `push.rs`, `activitypub.rs`, `emoji_packs.rs`, each listing's own); this
//! module proves the journeys: a post made over HTTP arriving rendered on
//! five different sockets, a follow over HTTP waking an encrypted push
//! payload a browser key decrypts, an outbox `Create` with an uploaded
//! attachment reading back as a real status with its media, twenty-five
//! rows walking their cursors through the `Link` header, and a pack built,
//! shared, archived, downloaded from a mock instance and reacted with.

use std::{
    collections::BTreeMap,
    io::{Cursor, Read as _, Write as _},
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
    response::Response,
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt as _, StreamExt as _};
use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
use p256::{
    SecretKey,
    elliptic_curve::{Generate as _, sec1::ToSec1Point as _},
};
use rand::RngCore as _;
use reqwest::RequestBuilder;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{
    net::TcpStream,
    task::JoinHandle,
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message as Frame, client::IntoClientRequest as _},
};
use uuid::Uuid;
use wired_be::{
    config::{Config, TEST_BASE_URL},
    models::user::{Flag, NewUser, User},
    services::{vapid, web_push},
    state::AppState,
    streaming::hub::{Audience, StreamEvent},
    test_db::TestDb,
};

use crate::common;

/// How long one frame may take to arrive before a test gives up: every
/// event a scenario waits for is produced by an HTTP call that has already
/// returned — the hub holds the event before the response does — so a frame
/// that is late at all is late by microseconds.
const READ_WAIT: Duration = Duration::from_secs(20);
/// The instance origin every served response is expected to quote.
const BASE: &str = TEST_BASE_URL;
/// The scopes the scenario actors carry: the root pair covers every
/// sub-scope the journeys touch (decision 9's scope tree).
const SCOPES: &str = "read write";
/// The fallback push icon: the same default the `Account` entity renders,
/// spelled here because the constant is crate-private.
const DEFAULT_AVATAR: &str = "/images/avi.png";
/// The boundary every multipart body of this module is framed with.
const MULTIPART_BOUNDARY: &str = "wired-e2e-realtime-boundary";

// ───────────────────────────  The served app  ───────────────────────────

/// The application on a real port: the address to dial over HTTP and
/// WebSocket, the state to run jobs on, and the lease that keeps its
/// database alive.
#[derive(Debug)]
struct Server {
    /// The loopback address the listener bound.
    addr: SocketAddr,
    /// The state the router runs on — the same one the scenario hands to
    /// [`common::run_all_jobs`].
    state: AppState,
    /// The outbound client the scenarios call the served app with.
    http: reqwest::Client,
    /// The `http://{addr}` origin to prefix paths with.
    base: String,
    /// The fixture slot the router reads, held for the server's whole life
    /// so the pool behind it stays leased (decision 13).
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

    /// Serves the real application with `config`: how the emoji tests get
    /// their own pack root and the upload test its own uploads dir.
    async fn with_config(config: Config) -> Self {
        let db = common::test_db().await;
        let state = common::test_app_state(db.pool().clone(), config);
        let app = wired_be::app(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a free loopback port");
        let addr = listener.local_addr().expect("the bound address");
        let task = tokio::spawn(async move {
            let _served = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        Self {
            addr,
            base: format!("http://{addr}"),
            http: reqwest::Client::new(),
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
    fn ws_url(&self, query: &str) -> String {
        if query.is_empty() {
            return format!("ws://{}/api/v1/streaming", self.addr);
        }
        format!("ws://{}/api/v1/streaming?{query}", self.addr)
    }

    /// Puts one bare control `update` naming `reader` on the hub the served
    /// router reads, for the negative assertions that must bound a socket's
    /// silence with an event only that socket's stream carries.
    fn publish_control(&self, stream: &str, reader: Uuid, marker: &str) {
        self.state.hub.publish(StreamEvent {
            streams: vec![stream.to_owned()],
            event: "update".to_owned(),
            payload: Some(format!(r#"{{"id":"{marker}"}}"#)),
            audience: Audience::Users(vec![reader]),
            render: None,
            local_only: false,
        });
    }
}

// A streaming socket ends with its server: the serve task goes when it does.
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ──────────────────────────  The HTTP helpers  ──────────────────────────

/// The verbs the scenario calls speak.
#[derive(Clone, Copy, Debug)]
enum Verb {
    /// Read a listing or an entity.
    Get,
    /// Write an entity or perform an action.
    Post,
    /// Replace what the body names.
    Put,
    /// Merge what the body names.
    Patch,
    /// Withdraw it.
    Delete,
}

/// A response from the served application: status, headers and the
/// buffered body.
#[derive(Debug)]
struct Reply {
    /// The response status.
    status: StatusCode,
    /// The response headers.
    headers: HeaderMap,
    /// The buffered body.
    body: Bytes,
}

impl Reply {
    /// Sends a finished request and buffers the answer whole.
    async fn send(request: RequestBuilder) -> Self {
        let response = request.send().await.expect("the served app answers");
        Self {
            status: response.status(),
            headers: response.headers().clone(),
            body: response.bytes().await.expect("the body reads"),
        }
    }

    /// The body parsed as JSON.
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("the answer is JSON")
    }

    /// The body decoded as text.
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The header `name` as text, `None` when absent.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// Runs `verb path` against `srv`, presenting `token` when given and a JSON
/// `body` when one is passed.
async fn call(
    srv: &Server,
    verb: Verb,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> Reply {
    let url = format!("{}{path}", srv.base);
    let mut request = match verb {
        Verb::Get => srv.http.get(&url),
        Verb::Post => srv.http.post(&url),
        Verb::Put => srv.http.put(&url),
        Verb::Patch => srv.http.patch(&url),
        Verb::Delete => srv.http.delete(&url),
    };
    if let Some(found) = token {
        request = request.bearer_auth(found);
    }
    if let Some(payload) = body {
        request = request.json(payload);
    }
    Reply::send(request).await
}

/// Asserts `reply` carries `status`, with the body in the failure message.
fn expect(reply: &Reply, status: StatusCode, who: &str) {
    assert_eq!(
        reply.status,
        status,
        "{who}: {} {}",
        reply.status.as_u16(),
        reply.text()
    );
}

/// The `GET` of `path`, asserted 200 and parsed.
async fn get_json(srv: &Server, token: Option<&str>, path: &str) -> Reply {
    let reply = call(srv, Verb::Get, path, token, None).await;
    expect(&reply, StatusCode::OK, &format!("GET {path}"));
    reply
}

/// The `GET` of `path` as JSON, asserted 200.
async fn get_value(srv: &Server, token: Option<&str>, path: &str) -> Value {
    get_json(srv, token, path).await.json()
}

/// A write of `body`, asserted 200 and parsed.
async fn write_ok(srv: &Server, verb: Verb, path: &str, token: &str, body: &Value) -> Value {
    let reply = call(srv, verb, path, Some(token), Some(body)).await;
    expect(&reply, StatusCode::OK, path);
    reply.json()
}

/// A body-less action `POST`, asserted 200 and parsed.
async fn action_ok(srv: &Server, path: &str, token: &str) -> Value {
    let reply = call(srv, Verb::Post, path, Some(token), None).await;
    expect(&reply, StatusCode::OK, path);
    reply.json()
}

/// One raw `multipart/form-data` part for [`post_multipart`].
#[derive(Debug)]
struct Part {
    /// The form field the part arrives under.
    name: &'static str,
    /// The client file name, which is what makes a part a file part.
    filename: Option<&'static str>,
    /// The part's `content-type`, when it carries one.
    content_type: Option<&'static str>,
    /// The part's bytes.
    bytes: Vec<u8>,
}

impl Part {
    /// A text field part.
    fn text(name: &'static str, value: &'static str) -> Self {
        Self {
            name,
            filename: None,
            content_type: None,
            bytes: value.as_bytes().to_vec(),
        }
    }

    /// A file part uploaded under `filename`.
    fn file(name: &'static str, filename: &'static str, bytes: Vec<u8>) -> Self {
        Self {
            name,
            filename: Some(filename),
            content_type: Some("application/octet-stream"),
            bytes,
        }
    }
}

/// Frames `parts` as a `multipart/form-data` body: the same writing
/// [`common::TestRequest::multipart`] does, sent over the wire.
fn multipart_body(parts: &[Part]) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
        match part.filename {
            Some(filename) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{}\"; filename=\"{filename}\"\r\n",
                    part.name
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{}\"\r\n", part.name).as_bytes(),
            ),
        }
        if let Some(content_type) = part.content_type {
            body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(&part.bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());
    body
}

/// `POST`s `parts` to `path` framed as `multipart/form-data`, answered raw
/// so the caller can assert either the status or the body bytes.
async fn post_multipart(srv: &Server, path: &str, token: &str, parts: &[Part]) -> Reply {
    let url = format!("{}{path}", srv.base);
    let request = srv
        .http
        .post(&url)
        .bearer_auth(token)
        .header(
            CONTENT_TYPE,
            format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}"),
        )
        .body(multipart_body(parts));
    Reply::send(request).await
}

// ───────────────────────  The WebSocket client  ───────────────────────

/// A connected streaming client. Every test ends it by dropping, which
/// closes the connection out from under the server's loop.
type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Opens `url`, panicking with the handshake failure if the server refused
/// the upgrade.
async fn open(url: &str) -> Client {
    let (client, _response) = connect_async(url)
        .await
        .unwrap_or_else(|err| panic!("the upgrade of {url} succeeds: {err}"));
    client
}

/// Opens `url` offering `protocol` as its `Sec-WebSocket-Protocol`, and
/// returns the socket beside the handshake answer.
async fn open_with_protocol(url: &str, protocol: &str) -> (Client, Response<Option<Vec<u8>>>) {
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
async fn send(client: &mut Client, text: &str) {
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
async fn next_json(client: &mut Client) -> Value {
    serde_json::from_str(&next_text(client).await).expect("every streaming frame is JSON")
}

/// Blocks until the server has taken every frame this client sent: a
/// `subscribe` of a stream that does not exist is always answered with the
/// refusal frame, and the socket loop handles client frames in arrival
/// order — so that answer is the barrier.
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

/// Every text frame up to and including the first that carries `until`:
/// the control fact a scenario produces last bounds the wait for
/// everything before it, because the HTTP write that published the earlier
/// event returned before the one that published the control began.
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

/// Reads frames until one carries `marker` and returns that frame parsed;
/// a test that needs what came before calls [`read_until`] instead.
async fn read_until_json(client: &mut Client, marker: &str) -> Value {
    let frames = read_until(client, marker).await;
    let frame = frames
        .iter()
        .rfind(|frame| frame.contains(marker))
        .expect("the control frame arrived");
    serde_json::from_str(frame).expect("every streaming frame is JSON")
}

/// The frame's `stream` array, as strings.
fn streams_of(frame: &Value) -> Vec<&str> {
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
fn assert_absent(frames: &[String], markers: &[&str]) {
    for marker in markers {
        assert!(
            !frames.iter().any(|frame| frame.contains(marker)),
            "`{marker}` was delivered: {frames:?}"
        );
    }
}

/// The JSON payload the frame's envelope carries, parsed.
fn frame_payload(frame: &Value) -> Value {
    serde_json::from_str(
        frame["payload"]
            .as_str()
            .expect("the envelope carries its entity as a payload string"),
    )
    .expect("the payload is the entity JSON")
}

/// The `user:{id}` hub stream name.
fn user_stream(id: Uuid) -> String {
    format!("user:{id}")
}

/// The `user:notification:{id}` hub stream name.
fn notification_stream(id: Uuid) -> String {
    format!("user:notification:{id}")
}

/// The `direct:{id}` hub stream name.
fn direct_stream(id: Uuid) -> String {
    format!("direct:{id}")
}

/// The `user:pleroma_chat:{id}` hub stream name.
fn chat_stream(id: Uuid) -> String {
    format!("user:pleroma_chat:{id}")
}

/// The `list:{id}` hub stream name.
fn list_stream(id: Uuid) -> String {
    format!("list:{id}")
}

// ─────────────────────────  The world builders  ─────────────────────────

/// A fresh local account named `nickname` beside a bearer token granting
/// `scopes`.
async fn account(srv: &Server, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(srv.pool(), nickname).await;
    let token = common::create_app_and_token(srv.pool(), &created, scopes).await;
    (created, token)
}

/// A fresh local account displaying as `name`, with a `scopes` token: the
/// shape a profile with a display name has (the push title reads it).
async fn named_account(srv: &Server, nickname: &str, name: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(srv.pool(), nickname, |user| NewUser {
        name: name.to_owned(),
        ..user
    })
    .await;
    let token = common::create_app_and_token(srv.pool(), &created, scopes).await;
    (created, token)
}

/// A fresh local admin named `nickname` with a `scopes` token.
async fn admin(srv: &Server, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(srv.pool(), nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(srv.pool(), &created, scopes).await;
    (created, token)
}

/// The `POST /api/v1/statuses` of `text` at `visibility`, answered entity.
async fn post_status(srv: &Server, token: &str, text: &str, visibility: &str) -> Value {
    write_ok(
        srv,
        Verb::Post,
        "/api/v1/statuses",
        token,
        &json!({ "status": text, "visibility": visibility }),
    )
    .await
}

/// The `id` of a JSON array of entities, in answer order.
fn ids_of(array: &Value) -> Vec<String> {
    array
        .as_array()
        .expect("the listing answers a JSON array")
        .iter()
        .map(|entity| {
            entity["id"]
                .as_str()
                .expect("every entity carries its id")
                .to_owned()
        })
        .collect()
}

/// The id of the entity a write answered with.
fn id_of(entity: &Value) -> String {
    entity["id"]
        .as_str()
        .expect("the entity carries its id")
        .to_owned()
}

/// `token`'s user follows the account `target`, asserted granted.
async fn follow(srv: &Server, token: &str, target: Uuid) {
    let relationship = action_ok(srv, &format!("/api/v1/accounts/{target}/follow"), token).await;
    assert_eq!(
        relationship["following"],
        json!(true),
        "the unlocked local target grants the follow on the spot"
    );
}

/// `token`'s user favourites the status `status`.
async fn favourite(srv: &Server, token: &str, status: &str) {
    let body = action_ok(srv, &format!("/api/v1/statuses/{status}/favourite"), token).await;
    assert_eq!(body["favourited"], json!(true), "the like lands");
}

// ────────────────────  Scenario 1: streaming sockets  ────────────────────

/// The whole lifecycle of one public post, produced over HTTP and streamed
/// to an anonymous `public` socket: Mastodon's `update` with the rendered
/// entity as the payload string, `status.update` for the `PUT` edit, and
/// `delete` whose payload is the withdrawn id.
#[tokio::test]
async fn a_public_status_streams_create_edit_and_delete() {
    let srv = Server::start().await;
    let (author, author_token) = account(&srv, "rt226-lc-author", SCOPES).await;
    let mut client = open(&srv.ws_url("stream=public")).await;

    let entity = post_status(&srv, &author_token, "the lifecycle post", "public").await;
    let id = id_of(&entity);
    let created = next_json(&mut client).await;
    assert_eq!(created["event"], "update", "the create arrives as update");
    assert!(
        streams_of(&created).contains(&"public"),
        "it rides the public stream: {created}"
    );
    let payload = frame_payload(&created);
    assert_eq!(
        payload["id"], entity["id"],
        "the entity is the created post"
    );
    assert_eq!(
        payload["account"]["id"],
        json!(author.id.to_string()),
        "rendered with its author"
    );
    assert!(
        payload["content"]
            .as_str()
            .is_some_and(|content| content.contains("the lifecycle post")),
        "and its content: {payload}"
    );

    let _edited = write_ok(
        &srv,
        Verb::Put,
        &format!("/api/v1/statuses/{id}"),
        &author_token,
        &json!({ "status": "the edited post" }),
    )
    .await;
    let edit = next_json(&mut client).await;
    assert_eq!(
        edit["event"], "status.update",
        "the edit redraws every socket that saw the post"
    );
    let edited_payload = frame_payload(&edit);
    assert_eq!(
        edited_payload["id"],
        json!(id),
        "same entity, restated, and the socket still holds its post"
    );
    assert!(
        edited_payload["content"]
            .as_str()
            .is_some_and(|content| content.contains("the edited post")),
        "rendered from the edited row: {edited_payload}"
    );

    let deleted = call(
        &srv,
        Verb::Delete,
        &format!("/api/v1/statuses/{id}"),
        Some(&author_token),
        None,
    )
    .await;
    expect(&deleted, StatusCode::OK, "the status delete");
    let frame = next_json(&mut client).await;
    assert_eq!(frame["event"], "delete", "the withdrawal streams too");
    assert_eq!(
        frame["payload"].as_str(),
        Some(id.as_str()),
        "and its payload is the deleted id"
    );
}

/// The hashtag stream moves with `subscribe` / `unsubscribe` frames: a tag
/// the handshake opened delivers the tagged post, a frame adds a second
/// tag, and once a frame takes the first tag back the same connection goes
/// silent about that tag's events — the edit of the first post reaches no
/// socket that left its tag — while the newly held tag keeps flowing.
#[tokio::test]
async fn hashtag_frames_subscribe_and_unsubscribe_a_live_socket() {
    let srv = Server::start().await;
    let (_author, author_token) = account(&srv, "rt226-tag-author", SCOPES).await;
    let mut client = open(&srv.ws_url("stream=hashtag&tag=alpha")).await;

    let first = id_of(&post_status(&srv, &author_token, "#Alpha one", "public").await);
    let arrived = next_json(&mut client).await;
    assert_eq!(arrived["event"], "update", "the tagged post arrives");
    assert!(
        streams_of(&arrived).contains(&"hashtag:alpha"),
        "on the tag its query string opened: {arrived}"
    );

    send(
        &mut client,
        r#"{"type":"subscribe","stream":"hashtag","tag":"beta"}"#,
    )
    .await;
    settled(&mut client).await;
    let second = post_status(&srv, &author_token, "#Beta two", "public").await;
    let second_id = id_of(&second);
    let beta = next_json(&mut client).await;
    assert!(
        beta["payload"]
            .as_str()
            .is_some_and(|payload| payload.contains(&second_id)),
        "the newly subscribed tag flows: {beta}"
    );

    send(
        &mut client,
        r#"{"type":"unsubscribe","stream":"hashtag","tag":"alpha"}"#,
    )
    .await;
    settled(&mut client).await;
    let _edit = write_ok(
        &srv,
        Verb::Put,
        &format!("/api/v1/statuses/{first}"),
        &author_token,
        &json!({ "status": "#Alpha edited again" }),
    )
    .await;
    let control = id_of(&post_status(&srv, &author_token, "#Beta control", "public").await);
    let frames = read_until(&mut client, &control).await;
    assert_absent(&frames, &[r#""event":"status.update""#, &first]);
    assert!(
        frames
            .iter()
            .any(|frame| { frame.contains(&control) && frame.contains(r#""event":"update""#) }),
        "the still-held tag keeps streaming: {frames:?}"
    );
}

/// `public:local` carries what the public timeline's local half holds and
/// who may hold a socket on it: a `local`-visibility post renders onto an
/// authenticated socket and never onto an anonymous one, and the same
/// event stays dark on a socket whose reader blocked or muted the author —
/// suppression sets read when the socket opened, judged against a post
/// written after, bounded by a control the public-local timeline also
/// carries.
#[tokio::test]
async fn public_local_carries_local_posts_to_authenticated_unfiltered_viewers() {
    let srv = Server::start().await;
    let (author, author_token) = account(&srv, "rt226-pl-author", SCOPES).await;
    let (_mate, mate_token) = account(&srv, "rt226-pl-mate", SCOPES).await;
    let (_watcher, watcher_token) = account(&srv, "rt226-pl-watcher", SCOPES).await;
    let (_blocker, blocker_token) = account(&srv, "rt226-pl-blocker", SCOPES).await;
    let (_muter, muter_token) = account(&srv, "rt226-pl-muter", SCOPES).await;
    let _blocked = action_ok(
        &srv,
        &format!("/api/v1/accounts/{}/block", author.id),
        &blocker_token,
    )
    .await;
    let _muted = action_ok(
        &srv,
        &format!("/api/v1/accounts/{}/mute", author.id),
        &muter_token,
    )
    .await;

    let mut watcher_socket =
        open(&srv.ws_url(&format!("stream=public:local&access_token={watcher_token}"))).await;
    let mut anon_socket = open(&srv.ws_url("stream=public:local")).await;
    let mut blocker_socket =
        open(&srv.ws_url(&format!("stream=public:local&access_token={blocker_token}"))).await;
    let mut muter_socket =
        open(&srv.ws_url(&format!("stream=public:local&access_token={muter_token}"))).await;

    let local_post = post_status(&srv, &author_token, "kept at home", "local").await;
    let local_id = id_of(&local_post);
    let seen = next_json(&mut watcher_socket).await;
    assert_eq!(seen["event"], "update", "the local viewer sees the post");
    assert!(
        streams_of(&seen).contains(&"public:local"),
        "on the local timeline: {seen}"
    );
    assert!(
        seen["payload"]
            .as_str()
            .is_some_and(|payload| payload.contains(&local_id)),
        "rendered with the status itself"
    );

    let control = id_of(&post_status(&srv, &mate_token, "the local control", "public").await);
    let anonymous = read_until(&mut anon_socket, &control).await;
    assert_absent(&anonymous, &[&local_id, "kept at home"]);
    let blocked = read_until(&mut blocker_socket, &control).await;
    assert_absent(&blocked, &[&local_id, "kept at home"]);
    let muted = read_until(&mut muter_socket, &control).await;
    assert_absent(&muted, &[&local_id, "kept at home"]);
    for (frames, who) in [
        (&anonymous, "anonymous"),
        (&blocked, "blocker"),
        (&muted, "muter"),
    ] {
        assert!(
            frames.iter().any(|frame| frame.contains(&control)),
            "the {who} socket is open and streaming its control: {frames:?}"
        );
    }
}

/// A followed author's post reaches the follower's `user` home socket and
/// every `list` socket that holds the author as a member — both computed
/// by the real target rules over the facts the HTTP writes just made.
#[tokio::test]
async fn home_and_list_sockets_stream_the_followed_and_listed_post() {
    let srv = Server::start().await;
    let (author, author_token) = account(&srv, "rt226-hl-author", SCOPES).await;
    let (follower, follower_token) = account(&srv, "rt226-hl-follower", SCOPES).await;
    let (_owner, owner_token) = account(&srv, "rt226-hl-owner", SCOPES).await;
    follow(&srv, &follower_token, author.id).await;
    // The list membership requires the owner to follow the author first —
    // the Mastodon rule the list-accounts write enforces.
    follow(&srv, &owner_token, author.id).await;

    let list = write_ok(
        &srv,
        Verb::Post,
        "/api/v1/lists",
        &owner_token,
        &json!({ "title": "Realtime" }),
    )
    .await;
    let list_id = Uuid::parse_str(&id_of(&list)).expect("the list id is a uuid");
    let _members = write_ok(
        &srv,
        Verb::Post,
        &format!("/api/v1/lists/{list_id}/accounts"),
        &owner_token,
        &json!({ "account_ids": [author.id.to_string()] }),
    )
    .await;

    let held_home = user_stream(follower.id);
    let held_list = list_stream(list_id);
    let mut home = open(&srv.ws_url(&format!("stream=user&access_token={follower_token}"))).await;
    let mut list_socket = open(&srv.ws_url(&format!(
        "stream=list&list={list_id}&access_token={owner_token}"
    )))
    .await;
    settled(&mut home).await;
    settled(&mut list_socket).await;

    let id = id_of(&post_status(&srv, &author_token, "for home and list", "public").await);
    let home_frame = read_until_json(&mut home, &id).await;
    assert_eq!(home_frame["event"], "update", "the home socket sees it");
    assert!(
        streams_of(&home_frame).contains(&held_home.as_str()),
        "riding the follower's own user stream: {home_frame}"
    );
    let list_frame = read_until_json(&mut list_socket, &id).await;
    assert_eq!(list_frame["event"], "update", "the list socket sees it too");
    assert!(
        streams_of(&list_frame).contains(&held_list.as_str()),
        "on its list stream: {list_frame}"
    );
}

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
    let status_id = id_of(&post_status(&srv, &alice_token, "to be liked", "public").await);

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
    assert!(
        streams_of(&like).contains(&alice_stream.as_str()),
        "named by the recipient's notification stream: {like}"
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
    let message_id = id_of(&message);
    let frames = read_until(&mut client, r#""event":"conversation""#).await;
    let update: Value = serde_json::from_str(
        frames
            .iter()
            .find(|frame| frame.contains(r#""event":"update""#))
            .expect("the message itself streamed before the conversation"),
    )
    .expect("frames are JSON envelopes");
    let held_streams = streams_of(&update);
    assert!(
        held_streams.contains(&own_direct.as_str()) && held_streams.contains(&own_home.as_str()),
        "the DM rides the reader's direct and user streams: {update}"
    );
    let conversation: Value = serde_json::from_str(
        frames
            .iter()
            .find(|frame| frame.contains(r#""event":"conversation""#))
            .expect("the conversation frame arrived"),
    )
    .expect("frames are JSON envelopes");
    assert!(
        streams_of(&conversation).contains(&own_direct.as_str()),
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
    let alice_chat_stream = chat_stream(alice.id);
    let alice_home = user_stream(alice.id);

    let alice_chat = action_ok(
        &srv,
        &format!("/api/v1/pleroma/chats/by-account-id/{}", bob.id),
        &alice_token,
    )
    .await;
    let alice_chat_id = id_of(&alice_chat);
    let bob_chat = action_ok(
        &srv,
        &format!("/api/v1/pleroma/chats/by-account-id/{}", alice.id),
        &bob_token,
    )
    .await;
    let bob_chat_id = id_of(&bob_chat);

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
    assert!(
        streams_of(&update).contains(&alice_chat_stream.as_str()),
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
    assert!(
        streams_of(&changed).contains(&alice_home.as_str()),
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

    let id = id_of(&post_status(&srv, &alice_token, "query and protocol", "public").await);
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
    assert!(
        streams_of(&query_frame).contains(&alice_own.as_str()),
        "the query-token socket streams for its user: {query_frame}"
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
    assert!(
        streams_of(&protocol_frame).contains(&bob_own.as_str()),
        "the subprotocol socket streams for ITS user: {protocol_frame}"
    );
    let protocol_payload = frame_payload(&protocol_frame);
    assert_eq!(
        protocol_payload["account"]["pleroma"]["relationship"]["following"],
        json!(true),
        "the same hub event rendered Bob's own view of Alice's account: {protocol_payload}"
    );
}

// ──────────────────────────  Scenario 2: Web Push  ──────────────────────────

/// One request a mock push service captured: its headers and body bytes.
type Captured = (HeaderMap, Vec<u8>);

/// What one mock push service remembers: the status it answers with and
/// the requests it captured.
#[derive(Debug)]
struct MockPush {
    /// The status code every answer carries.
    status: u16,
    /// The captured requests, in arrival order.
    seen: Mutex<Vec<Captured>>,
}

impl MockPush {
    /// How many deliveries arrived.
    fn count(&self) -> usize {
        self.seen.lock().expect("an unpoisoned lock").len()
    }

    /// The last captured request, cloned out from under the lock.
    fn last(&self) -> Captured {
        let captured: Captured = {
            let guard = self.seen.lock().expect("an unpoisoned lock");
            guard.last().expect("the mock served a request").clone()
        };
        captured
    }

    /// The named header of the last captured request.
    fn header(&self, name: &str) -> String {
        let (headers, _body) = self.last();
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    /// The last captured request body.
    fn last_body(&self) -> Vec<u8> {
        let (_headers, body) = self.last();
        body
    }
}

/// The mock's handler: record the request, answer with the scripted status.
async fn catch_push(
    State(mock): State<Arc<MockPush>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    mock.seen
        .lock()
        .expect("an unpoisoned lock")
        .push((headers, body.to_vec()));
    StatusCode::from_u16(mock.status).expect("the test statuses are representable")
}

/// Serves a mock push service answering every `POST /push` with `status`,
/// on a fresh loopback port: the endpoint URL, its capture, and the task.
/// A plain-`http` loopback is exactly what the subscription endpoint
/// accepts (the `src/api/push.rs` gate follows Pleroma, which stores any
/// URL the delivery can call), and the instance's own client reaches it
/// because the test config allows private hosts.
async fn mock_push_service(status: u16) -> (String, Arc<MockPush>, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port to serve on");
    let url = format!(
        "http://{}/push",
        listener.local_addr().expect("the bound address")
    );
    let mock = Arc::new(MockPush {
        status,
        seen: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route("/push", post(catch_push))
        .with_state(Arc::clone(&mock));
    let server = tokio::spawn(async move {
        if axum::serve(listener, app).await.is_err() {
            // A dead mock reports itself through the empty capture.
        }
    });
    (url, mock, server)
}

/// The browser side of a subscription: the stored public half beside the
/// private scalar and auth secret the test decrypts with.
#[derive(Clone, Debug)]
struct ClientKeys {
    /// The stored client public point, base64url.
    p256dh: String,
    /// The stored auth secret, base64url.
    auth: String,
    /// The private scalar, test-side only.
    private_key: String,
}

/// A fresh P-256 key pair and 16-octet auth secret, the way a browser
/// generates them for `PushManager.subscribe`.
fn client_keys() -> ClientKeys {
    let secret = SecretKey::try_generate().expect("a fresh P-256 client key");
    let mut auth = [0_u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut auth);
    ClientKeys {
        p256dh: URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_point(false).as_bytes()),
        auth: URL_SAFE_NO_PAD.encode(auth),
        private_key: URL_SAFE_NO_PAD.encode(secret.to_bytes()),
    }
}

/// The pieces one `POST /api/v1/push/subscription` carries.
#[derive(Debug)]
struct PushSpec<'a> {
    /// The `push`-scoped bearer registering.
    token: &'a str,
    /// The push service's callback endpoint.
    endpoint: &'a str,
    /// The generated browser key pair.
    keys: &'a ClientKeys,
    /// The named alert flags.
    alerts: Value,
    /// The stored policy, `None` for the default `all`.
    policy: Option<&'a str>,
}

impl PushSpec<'_> {
    /// The body this spec registers with.
    fn body(&self) -> Value {
        json!({
            "subscription": {
                "endpoint": self.endpoint,
                "keys": {"p256dh": self.keys.p256dh, "auth": self.keys.auth},
            },
            "data": { "alerts": self.alerts.clone(), "policy": self.policy },
        })
    }
}

/// Registers `spec`'s subscription over HTTP, returning the entity.
async fn register_subscription(srv: &Server, spec: &PushSpec<'_>) -> Value {
    write_ok(
        srv,
        Verb::Post,
        "/api/v1/push/subscription",
        spec.token,
        &spec.body(),
    )
    .await
}

/// The decrypted push payload of the mock's last capture, as JSON.
fn delivered_payload(mock: &MockPush, keys: &ClientKeys) -> Value {
    let decrypted = web_push::decrypt_payload(&mock.last_body(), &keys.private_key, &keys.auth)
        .expect("the client's key reads the body");
    serde_json::from_slice(&decrypted).expect("the payload is JSON")
}

/// The pieces of a follow-alert registration for `token` at `endpoint`.
fn follow_spec<'a>(token: &'a str, endpoint: &'a str, keys: &'a ClientKeys) -> PushSpec<'a> {
    PushSpec {
        token,
        endpoint,
        keys,
        alerts: json!({"follow": true}),
        policy: None,
    }
}

/// The delivered document's shape asserted as a whole: the token, the
/// type, the display-name title, the plain-text body and the locale.
fn assert_wake(payload: &Value, token: &str, kind: &str, title: &str, body: &str) {
    assert_eq!(
        payload["access_token"],
        json!(token),
        "the wake token is the subscription's own"
    );
    assert_eq!(payload["notification_type"], json!(kind), "the type");
    assert!(
        Uuid::parse_str(
            payload["notification_id"]
                .as_str()
                .expect("a notification id")
        )
        .is_ok(),
        "it names a notification: {payload}"
    );
    assert_eq!(payload["title"], json!(title), "display name + verb");
    assert_eq!(payload["body"], json!(body), "the plain-text body");
    assert_eq!(payload["preferred_locale"], json!("en"), "the locale");
}

/// The wake end to end: a subscription registered over HTTP with a fresh
/// browser key pair, a follow written over HTTP, the delivery job drained
/// against the mock — and the captured body decrypting with that key to
/// the Mastodon-shaped document: the subscriber's own access token, the
/// notification id and type, the default-avatar icon, the display-name
/// title, and the transport headers (`aes128gcm`, the four-week `TTL`, the
/// VAPID `Authorization` naming the instance's stored key).
#[tokio::test]
async fn a_follow_over_http_delivers_a_decryptable_push_payload() {
    let srv = Server::start().await;
    let (alice, alice_token) =
        named_account(&srv, "rt226-wp-alice", "Alice", "read write push").await;
    let (_bob, bob_token) = named_account(&srv, "rt226-wp-bob", "Bob", SCOPES).await;
    let (url, mock, server) = mock_push_service(201).await;
    let keys = client_keys();

    let spec = follow_spec(&alice_token, &url, &keys);
    let entity = register_subscription(&srv, &spec).await;
    assert_eq!(entity["endpoint"], json!(url), "the endpoint reads back");
    let server_key = vapid::vapid_public_key(srv.pool())
        .await
        .expect("the instance VAPID pair");
    assert_eq!(
        entity["server_key"],
        json!(server_key),
        "the response advertises the instance key"
    );
    assert_eq!(
        entity["alerts"]["follow"],
        json!(true),
        "the named alert is on"
    );
    assert_eq!(
        entity["alerts"]["mention"],
        json!(false),
        "the unnamed one is off"
    );
    assert_eq!(entity["policy"], json!("all"), "the default policy");

    follow(&srv, &bob_token, alice.id).await;
    let ran = common::run_all_jobs(&srv.state).await;
    server.abort();
    assert!(ran >= 1, "the follow queued its delivery job");

    assert_eq!(mock.count(), 1, "one request reached the push service");
    assert_eq!(mock.header("content-encoding"), "aes128gcm", "the coding");
    assert_eq!(mock.header("ttl"), web_push::TTL_HEADER, "the four weeks");
    assert_eq!(mock.header("urgency"), "normal", "the middle urgency");
    let authorization = mock.header("authorization");
    assert!(
        authorization.starts_with("vapid t=")
            && authorization.ends_with(&format!(", k={server_key}")),
        "the VAPID signature names the instance key: {authorization}"
    );

    let payload = delivered_payload(&mock, &keys);
    assert_wake(&payload, &alice_token, "follow", "Bob followed you", "");
    assert_eq!(
        payload["icon"],
        json!(format!("{BASE}{DEFAULT_AVATAR}")),
        "an avatar-less actor wakes with the instance default"
    );
}

/// The alert flags, written and re-written over HTTP: a subscription
/// created with only the follow alert on stays dark when a status of the
/// subscriber's is favourited, and the `PUT` that names the favourite flag
/// turns the next one through — the second delivery's payload carries the
/// status's plain text as its body.
#[tokio::test]
async fn alert_flags_gate_the_wake_and_the_put_reopens_them() {
    let srv = Server::start().await;
    let (_alert_alice, alice_token) =
        named_account(&srv, "rt226-alert-alice", "Alice", "read write push").await;
    let (_alert_bob, bob_token) = named_account(&srv, "rt226-alert-bob", "Bob", SCOPES).await;
    let (_alert_carol, carol_token) =
        named_account(&srv, "rt226-alert-carol", "Carol", SCOPES).await;
    let (url, mock, server) = mock_push_service(201).await;
    let keys = client_keys();
    let spec = follow_spec(&alice_token, &url, &keys);
    let _registered = register_subscription(&srv, &spec).await;

    let liked = id_of(&post_status(&srv, &alice_token, "pushed body text", "public").await);
    favourite(&srv, &bob_token, &liked).await;
    let _ran_dark = common::run_all_jobs(&srv.state).await;
    assert_eq!(mock.count(), 0, "the off favourite alert sends nothing");

    let _updated = write_ok(
        &srv,
        Verb::Put,
        "/api/v1/push/subscription",
        &alice_token,
        &json!({ "data": { "alerts": { "favourite": true } } }),
    )
    .await;
    favourite(&srv, &carol_token, &liked).await;
    let _ran_wake = common::run_all_jobs(&srv.state).await;
    server.abort();

    assert_eq!(mock.count(), 1, "the re-opened alert wakes");
    assert_wake(
        &delivered_payload(&mock, &keys),
        &alice_token,
        "favourite",
        "Carol favourited your post",
        "pushed body text",
    );
}

/// The policy gate, end to end: a `followed` subscription stays dark for
/// an account its owner does not follow, and the follow written over HTTP
/// between the two attempts lets the next telling through.
#[tokio::test]
async fn the_followed_policy_wakes_only_for_accounts_the_owner_follows() {
    let srv = Server::start().await;
    let (_pol_alice, alice_token) =
        named_account(&srv, "rt226-pol-alice", "Alice", "read write push").await;
    let (dave, dave_token) = named_account(&srv, "rt226-pol-dave", "Dave", SCOPES).await;
    let (url, mock, server) = mock_push_service(201).await;
    let keys = client_keys();
    let spec = PushSpec {
        token: &alice_token,
        endpoint: &url,
        keys: &keys,
        alerts: json!({"favourite": true}),
        policy: Some("followed"),
    };
    let _registered = register_subscription(&srv, &spec).await;
    let first = id_of(&post_status(&srv, &alice_token, "one", "public").await);
    let second = id_of(&post_status(&srv, &alice_token, "two", "public").await);

    favourite(&srv, &dave_token, &first).await;
    let _ran_dark = common::run_all_jobs(&srv.state).await;
    assert_eq!(mock.count(), 0, "a stranger stays behind the policy");

    follow(&srv, &alice_token, dave.id).await;
    favourite(&srv, &dave_token, &second).await;
    let _ran_wake = common::run_all_jobs(&srv.state).await;
    server.abort();

    assert_eq!(
        mock.count(),
        1,
        "the followed actor now wakes the subscription"
    );
    assert_wake(
        &delivered_payload(&mock, &keys),
        &alice_token,
        "favourite",
        "Dave favourited your post",
        "two",
    );
}

/// A `410` from the push service retires the subscription through the
/// job: after the drain, `GET /api/v1/push/subscription` is the 404 — the
/// row is gone, not merely quiet.
#[tokio::test]
async fn a_gone_push_service_retires_the_subscription() {
    let srv = Server::start().await;
    let (alice, alice_token) =
        named_account(&srv, "rt226-gone-alice", "Alice", "read write push").await;
    let (_gone_bob, bob_token) = named_account(&srv, "rt226-gone-bob", "Bob", SCOPES).await;
    let (gone_url, gone_mock, gone_server) = mock_push_service(410).await;
    let gone_keys = client_keys();
    let spec = follow_spec(&alice_token, &gone_url, &gone_keys);
    let _registered = register_subscription(&srv, &spec).await;

    follow(&srv, &bob_token, alice.id).await;
    let _ran = common::run_all_jobs(&srv.state).await;
    gone_server.abort();
    assert_eq!(gone_mock.count(), 1, "the dead service got its one try");

    let retired = call(
        &srv,
        Verb::Get,
        "/api/v1/push/subscription",
        Some(&alice_token),
        None,
    )
    .await;
    expect(&retired, StatusCode::NOT_FOUND, "the 410 retired the row");
}

/// A `503` keeps the subscription: the attempt reaches the service once,
/// the job rides the queue's backoff instead of retrying inside the
/// drain, and the subscription still reads 200.
#[tokio::test]
async fn a_flaky_push_service_keeps_the_subscription_and_retries_later() {
    let srv = Server::start().await;
    let (alice, alice_token) =
        named_account(&srv, "rt226-flaky-alice", "Alice", "read write push").await;
    let (_flaky_actor, actor_token) = named_account(&srv, "rt226-flaky-bob", "Bob", SCOPES).await;
    let (retry_url, retry_mock, retry_server) = mock_push_service(503).await;
    let retry_keys = client_keys();
    let spec = follow_spec(&alice_token, &retry_url, &retry_keys);
    let _registered = register_subscription(&srv, &spec).await;

    follow(&srv, &actor_token, alice.id).await;
    let ran = common::run_all_jobs(&srv.state).await;
    let second_ran = common::run_all_jobs(&srv.state).await;
    retry_server.abort();
    assert_eq!(ran, 1, "the flaky service got its attempt");
    assert_eq!(
        second_ran, 0,
        "and nothing more is due yet — the retry rides the queue's backoff"
    );
    assert_eq!(retry_mock.count(), 1, "the try reached it once, 503");

    let standing = call(
        &srv,
        Verb::Get,
        "/api/v1/push/subscription",
        Some(&alice_token),
        None,
    )
    .await;
    expect(
        &standing,
        StatusCode::OK,
        "a retryable failure keeps the subscription",
    );
}

// ────────────────  Scenario 3: client-to-server ActivityPub  ────────────────

/// A decodable 8x8 `PNG` of `seed`-shifted colors: bytes the upload
/// pipeline detects, decodes and stores.
fn png_fixture(seed: u32) -> Vec<u8> {
    let channel = |value: u32| u8::try_from(value % 256).expect("a modulo of 256 fits a u8");
    let image: RgbaImage = ImageBuffer::from_fn(8, 8, |x, y| {
        Rgba([channel(x + seed), channel(y * 7), 128, 255])
    });
    let mut out = Vec::new();
    DynamicImage::ImageRgba8(image)
        .write_to(Cursor::new(&mut out), ImageFormat::Png)
        .expect("the fixture encodes");
    out
}

/// The C2S walk a native client takes: `GET /api/ap/whoami` answers the
/// token's own actor document (and nothing at all to the anonymous
/// request), `POST /api/ap/upload_media` stores a real image and answers
/// its `Document`, and `POST /users/{nickname}/outbox` folds a `Create` of
/// a `Note` — with that upload among its `attachment`s and the public
/// addressee in `to` — into a real status through the client API's own
/// create path, `Location` naming the `Create` the post federates as. The
/// posted note then reads back at `GET /api/v1/statuses/{id}` with its
/// content rendered and its media attached.
#[tokio::test]
async fn the_c2s_outbox_creates_from_an_uploaded_attachment() {
    let uploads = tempfile::TempDir::new().expect("a scratch uploads dir");
    let mut config = common::test_config();
    config.uploads_dir = uploads.path().to_path_buf();
    let srv = Server::with_config(config).await;
    let (alice, alice_token) = account(&srv, "rt226-c2s-alice", "read write write:media").await;

    let anonymous = call(&srv, Verb::Get, "/api/ap/whoami", None, None).await;
    expect(
        &anonymous,
        StatusCode::UNAUTHORIZED,
        "whoami without a token",
    );

    let actor = call(&srv, Verb::Get, "/api/ap/whoami", Some(&alice_token), None).await;
    expect(&actor, StatusCode::OK, "whoami");
    let document = actor.json();
    assert_eq!(
        document["id"],
        json!(alice.ap_id),
        "the token's own actor, the same bytes a peer fetches"
    );
    assert_eq!(document["type"], json!("Person"), "a Person document");

    let media_id = c2s_upload(&srv, &alice_token).await;
    let status_id = c2s_create_note(&srv, &alice_token, &media_id).await;

    let shown = get_value(
        &srv,
        Some(&alice_token),
        &format!("/api/v1/statuses/{status_id}"),
    )
    .await;
    assert_eq!(shown["id"], json!(status_id), "the status reads back");
    assert!(
        shown["content"]
            .as_str()
            .is_some_and(|content| content.contains("posted through the outbox")),
        "rendered from the note's content: {}",
        shown["content"]
    );
    assert_eq!(shown["visibility"], json!("public"), "the to-addressee");
    let attachments = shown["media_attachments"]
        .as_array()
        .expect("the entity carries its media");
    assert_eq!(attachments.len(), 1, "the uploaded attachment rode along");
    assert_eq!(
        attachments[0]["id"],
        json!(media_id),
        "the very upload the Create named"
    );
    assert_eq!(
        attachments[0]["description"],
        json!("a realtime pixel grid"),
        "with its alt text"
    );
}

/// `POST /api/ap/upload_media` with one generated `PNG` and a
/// description, asserted answered as the `Document` Pleroma spells; the
/// media id it carries is what the create names in its `attachment`.
async fn c2s_upload(srv: &Server, token: &str) -> String {
    let upload = post_multipart(
        srv,
        "/api/ap/upload_media",
        token,
        &[
            Part::file("file", "pic.png", png_fixture(3)),
            Part::text("description", "a realtime pixel grid"),
        ],
    )
    .await;
    expect(&upload, StatusCode::OK, "the C2S upload");
    let media = upload.json();
    assert_eq!(
        media["type"],
        json!("Document"),
        "Pleroma's Document answer"
    );
    let media_id = media["id"].as_str().expect("the media id").to_owned();
    assert!(
        media["url"][0]["href"]
            .as_str()
            .is_some_and(|href| href.starts_with(BASE)),
        "the file hangs off the instance: {}",
        media["url"]
    );
    media_id
}

/// `POST /users/rt226-c2s-alice/outbox` with a `Create` of a `Note`
/// carrying `media_id` as its attachment and the public addressee in
/// `to`; asserts the `201` and its `Location`, and answers the status id
/// the `Create` federates from.
async fn c2s_create_note(srv: &Server, token: &str, media_id: &str) -> String {
    let activity = json!({
        "type": "Create",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [],
        "object": {
            "type": "Note",
            "content": "posted through the outbox",
            "attachment": [{"id": media_id}],
        },
    });
    let posted = call(
        srv,
        Verb::Post,
        "/users/rt226-c2s-alice/outbox",
        Some(token),
        Some(&activity),
    )
    .await;
    expect(&posted, StatusCode::CREATED, "the outbox Create");
    let location = posted
        .header("location")
        .expect("a 201 carries a Location")
        .to_owned();
    let status_id = location
        .strip_prefix(&format!("{BASE}/activities/"))
        .unwrap_or(&location)
        .to_owned();
    assert!(
        Uuid::parse_str(&status_id).is_ok(),
        "Location names the federated activity id: {location}"
    );
    status_id
}

/// The owner's C2S inbox read: the collection root names its paged first
/// page, and with `?page=true` the statuses that @-mention the owner —
/// written through the ordinary client API by another account — arrive as
/// their `Create` activities, the unmentioned post absent.
#[tokio::test]
async fn the_c2s_inbox_reads_mentions_as_create_activities() {
    let srv = Server::start().await;
    let (alice, alice_token) = account(&srv, "rt226-in-alice", SCOPES).await;
    let (dave, dave_token) = account(&srv, "rt226-in-dave", SCOPES).await;

    let root = get_value(&srv, Some(&alice_token), "/users/rt226-in-alice/inbox").await;
    assert_eq!(
        root["id"],
        json!(format!("{}/inbox", alice.ap_id)),
        "the collection answers at the actor's inbox address"
    );
    assert_eq!(root["type"], json!("OrderedCollection"), "ordered");
    assert_eq!(
        root["first"],
        json!(format!("{}/inbox?page=true", alice.ap_id)),
        "and names its first page"
    );

    let mentioned = id_of(
        &post_status(
            &srv,
            &dave_token,
            "@rt226-in-alice this names you",
            "public",
        )
        .await,
    );
    let _unmentioned = id_of(&post_status(&srv, &dave_token, "to nobody", "public").await);

    let page = get_value(
        &srv,
        Some(&alice_token),
        "/users/rt226-in-alice/inbox?page=true",
    )
    .await;
    assert_eq!(
        page["type"],
        json!("OrderedCollectionPage"),
        "the paged read is a page"
    );
    let items = page["orderedItems"].as_array().expect("ordered items");
    assert_eq!(items.len(), 1, "only the mentioning post is in the inbox");
    assert_eq!(items[0]["type"], json!("Create"), "as its Create");
    assert_eq!(items[0]["actor"], json!(dave.ap_id), "authored by Dave");
    assert!(
        items[0]["object"]["content"]
            .as_str()
            .is_some_and(|content| content.contains("this names you")),
        "carrying the note: {}",
        items[0]["object"]
    );
    let object_id = items[0]["object"]["id"]
        .as_str()
        .expect("the object's id")
        .rsplit('/')
        .next()
        .expect("a final segment")
        .to_owned();
    assert_eq!(object_id, mentioned, "the very status the API posted");
}

// ──────────────────  Scenario 4: the pagination conformance sweep  ──────────────────

/// The default page size every shared-`PageParams` listing answers with.
const DEFAULT_PAGE: usize = 20;
/// The cap every swept listing clamps an absurd `limit` to.
const PAGE_CAP: usize = 40;
/// How many rows the sweep world seeds into every listing it walks.
const SWEEP_ROWS: usize = 25;

/// One swept listing: the bearer it must be read with, its path, and a
/// label for the failure messages.
#[derive(Debug)]
struct SweepListing {
    /// The token the listing is keyed to.
    token: String,
    /// The endpoint path.
    path: String,
    /// Who the listing is, in the failure text.
    who: &'static str,
}

/// The cursor a `Link` header's `rel="{rel}"` entry carries, whatever
/// query the endpoint repeated ahead of it.
fn link_cursor(link: &str, rel: &str) -> String {
    let wanted = format!("rel=\"{rel}\"");
    let entry = link
        .split(", ")
        .find(|entry| entry.ends_with(&wanted))
        .unwrap_or_else(|| panic!("the header carries a {rel} link: {link}"));
    let url = entry
        .strip_prefix('<')
        .and_then(|rest| rest.split('>').next())
        .expect("every entry is a url in angle brackets");
    url.rsplit_once('=')
        .expect("the cursor is the last pair")
        .1
        .to_owned()
}

/// Asserts the `Link` header is Pleroma's `add_link_headers` shape: the
/// `next` entry hangs `max_id` off the endpoint's own address under the
/// instance origin, the `prev` entry hangs `min_id`, both cursors are
/// uuid texts (decision 4 keys them on the row id, which is not always
/// the entity id the page carries), and they are distinct — `next`
/// points at the page's oldest key, `prev` at its newest.
fn assert_link_shape(link: Option<&str>, path: &str, who: &str) -> (String, String) {
    let value = link.unwrap_or_else(|| panic!("a non-empty {who} page links: {path}"));
    let next = link_cursor(value, "next");
    let prev = link_cursor(value, "prev");
    let next_entry = format!(r#"<{BASE}{path}?max_id={next}>; rel="next""#);
    assert!(
        value.starts_with(next_entry.as_str()),
        "{who}: the next entry leads the header: {value}"
    );
    let prev_entry = format!(r#", <{BASE}{path}?min_id={prev}>; rel="prev""#);
    assert!(
        value.ends_with(prev_entry.as_str()),
        "{who}: the prev entry closes the header: {value}"
    );
    assert_ne!(next, prev, "{who}: a one-row page would repeat the cursor");
    for cursor in [&next, &prev] {
        assert!(
            Uuid::parse_str(cursor).is_ok(),
            "{who}: the {cursor} cursor is an id: {value}"
        );
    }
    (next, prev)
}

/// The `max_id` walk of one listing: page by page down to the empty tail
/// that must carry no `Link` header, every non-empty page carrying one.
/// Answers the oldest cursor the walk saw — the key of the very last
/// row.
async fn sweep_walk(
    srv: &Server,
    listing: &SweepListing,
    first_ids: Vec<String>,
    next_cursor: String,
) -> (Vec<String>, String) {
    let mut walked: Vec<String> = first_ids;
    let mut oldest_cursor = next_cursor;
    loop {
        let reply = call(
            srv,
            Verb::Get,
            &format!("{}?max_id={oldest_cursor}", listing.path),
            Some(&listing.token),
            None,
        )
        .await;
        expect(
            &reply,
            StatusCode::OK,
            &format!("{} max_id walk", listing.who),
        );
        let ids = ids_of(&reply.json());
        let Some(page_link) = reply.header("link").map(str::to_owned) else {
            assert!(
                ids.is_empty(),
                "{}: a non-empty page carries a Link header: {ids:?}",
                listing.who
            );
            return (walked, oldest_cursor);
        };
        assert!(
            !ids.is_empty(),
            "{}: only an empty page drops the Link header",
            listing.who
        );
        walked.extend(ids);
        oldest_cursor = link_cursor(&page_link, "next");
    }
}

/// The cursor parameters of one listing against cursors the walk itself
/// produced: `since_id` polling the newer end of the page from the oldest
/// row's key and from a mid-listing one, and `min_id` answering the page
/// after the oldest key still newest-first.
async fn sweep_cursors(
    srv: &Server,
    listing: &SweepListing,
    expected: &[String],
    oldest_cursor: &str,
    next_cursor: &str,
) {
    let polled = get_value(
        srv,
        Some(&listing.token),
        &format!("{}?since_id={oldest_cursor}&limit={PAGE_CAP}", listing.path),
    )
    .await;
    assert_eq!(
        ids_of(&polled),
        expected[..SWEEP_ROWS - 1],
        "{}: since_id reads the newer rows, still newest-first",
        listing.who
    );
    let flipped = get_value(
        srv,
        Some(&listing.token),
        &format!("{}?min_id={oldest_cursor}&limit={PAGE_CAP}", listing.path),
    )
    .await;
    assert_eq!(
        ids_of(&flipped),
        expected[..SWEEP_ROWS - 1],
        "{}: the min_id page after the cursor answers newest-first too",
        listing.who
    );
    let middle = get_value(
        srv,
        Some(&listing.token),
        &format!("{}?since_id={next_cursor}&limit={PAGE_CAP}", listing.path),
    )
    .await;
    assert_eq!(
        ids_of(&middle),
        expected[..DEFAULT_PAGE - 1],
        "{}: a mid-listing cursor polls exactly the newer rows",
        listing.who
    );
}

/// The keyset walk every `PageParams` listing must perform, against a
/// world that seeded it [`SWEEP_ROWS`] rows: the newest-first default page
/// of [`DEFAULT_PAGE`] rows under a well-shaped `Link` header; the `max_id`
/// walk to the tail and the empty page that carries no header; `since_id`
/// polling only the rows newer than the oldest cursor and than a
/// mid-listing one; `min_id` reading the page just after the cursor still
/// newest-first; `limit` honouring five; and a malformed cursor refused
/// with the shared 400. The cursors are read from the headers themselves:
/// decision 4 keys a listing on its row id, which for the relationship
/// pages is documented to be the follow row rather than the account the
/// entity carries.
async fn sweep(srv: &Server, listing: &SweepListing) {
    let all = get_json(
        srv,
        Some(&listing.token),
        &format!("{}?limit={PAGE_CAP}", listing.path),
    )
    .await;
    let expected = ids_of(&all.json());
    assert_eq!(expected.len(), SWEEP_ROWS, "{}: the full page", listing.who);

    let first = get_json(srv, Some(&listing.token), &listing.path).await;
    let first_ids = ids_of(&first.json());
    assert_eq!(
        first_ids,
        expected[..DEFAULT_PAGE],
        "{}: the default page is the newest twenty, newest first",
        listing.who
    );
    let (next_cursor, _prev_cursor) =
        assert_link_shape(first.header("link"), &listing.path, listing.who);

    let (walked, oldest_cursor) = sweep_walk(srv, listing, first_ids, next_cursor.clone()).await;
    assert_eq!(
        walked, expected,
        "{}: the cursor walk covers every row once",
        listing.who
    );

    sweep_cursors(srv, listing, &expected, &oldest_cursor, &next_cursor).await;

    let small = get_value(
        srv,
        Some(&listing.token),
        &format!("{}?limit=5", listing.path),
    )
    .await;
    assert_eq!(
        ids_of(&small),
        expected[..5],
        "{}: limit sizes the page",
        listing.who
    );

    let bogus = call(
        srv,
        Verb::Get,
        &format!("{}?max_id=nope", listing.path),
        Some(&listing.token),
        None,
    )
    .await;
    expect(
        &bogus,
        StatusCode::BAD_REQUEST,
        &format!("{} bogus cursor", listing.who),
    );
    assert_eq!(
        bogus.json()["error"],
        json!("Invalid id"),
        "{}: the shared refusal wording",
        listing.who
    );
}

/// Twenty-five public posts each by Alice and by Carol (whose posts are
/// the favourite and bookmark targets, kept out of Bob's home timeline by
/// standing outside his follow graph).
async fn seed_sweep_posts(srv: &Server, alice_token: &str, carol_token: &str) {
    for index in 1..=SWEEP_ROWS {
        let alice_post =
            post_status(srv, alice_token, &format!("sweep alice {index}"), "public").await;
        assert!(alice_post["id"].is_string(), "alice's post {index} lands");
        let carol_post =
            post_status(srv, carol_token, &format!("sweep carol {index}"), "public").await;
        assert!(carol_post["id"].is_string(), "carol's post {index} lands");
    }
}

/// The twenty-five followers of Bob, each following over HTTP: the
/// followers page and Bob's twenty-five-row notification queue. (They
/// follow Bob, not Alice, so that the home sweep's own follow of Alice —
/// the twenty-sixth telling her queue would otherwise carry — belongs to
/// neither of those listings.)
async fn seed_sweep_followers(srv: &Server, bob: &User) {
    for index in 1..=SWEEP_ROWS {
        let (follower, token) = account(srv, &format!("rt226-sw-f{index}"), SCOPES).await;
        assert_eq!(
            follower.nickname,
            format!("rt226-sw-f{index}"),
            "the follower {index} exists"
        );
        follow(srv, &token, bob.id).await;
    }
}

/// Alice's favourites and bookmarks of all twenty-five of Carol's posts,
/// read through the statuses listing the sweep itself will walk.
async fn seed_sweep_engagements(srv: &Server, alice_token: &str, carol: &User) {
    let carol_statuses = get_value(
        srv,
        Some(alice_token),
        &format!("/api/v1/accounts/{}/statuses?limit={PAGE_CAP}", carol.id),
    )
    .await;
    for status in carol_statuses.as_array().expect("a listing") {
        let id = id_of(status);
        favourite(srv, alice_token, &id).await;
        let _bookmarked = write_ok(
            srv,
            Verb::Post,
            &format!("/api/v1/statuses/{id}/bookmark"),
            alice_token,
            &json!({}),
        )
        .await;
    }
}

/// The remaining twenty-five-row worlds: Alice's mutes and blocks of two
/// disjoint crowds, and one direct thread per third crowd, opened by Alice.
async fn seed_sweep_relationships(srv: &Server, alice_token: &str) {
    for index in 1..=SWEEP_ROWS {
        let (mute_target, _mute_token) = account(srv, &format!("rt226-sw-m{index}"), SCOPES).await;
        let _muted = action_ok(
            srv,
            &format!("/api/v1/accounts/{}/mute", mute_target.id),
            alice_token,
        )
        .await;
        let (block_target, _block_token) =
            account(srv, &format!("rt226-sw-b{index}"), SCOPES).await;
        let _blocked = action_ok(
            srv,
            &format!("/api/v1/accounts/{}/block", block_target.id),
            alice_token,
        )
        .await;
        let (partner, _partner_token) = account(srv, &format!("rt226-sw-c{index}"), SCOPES).await;
        let dm = post_status(
            srv,
            alice_token,
            &format!("@{} quietly {index}", partner.nickname),
            "direct",
        )
        .await;
        assert!(dm["id"].is_string(), "the dm {index} lands");
    }
}

/// The conformance sweep over every listing that shares `PageParams`'
/// keyset on the row id (decision 4): the home timeline, an account's
/// statuses, the notification queue, the followers page, the favourites,
/// bookmarks, mutes and blocks listings, and the conversations — each on a
/// world of twenty-five rows built entirely over HTTP, each walked through
/// its cursors and its `Link` header the way any client would.
#[tokio::test]
async fn every_keyset_listing_walks_its_cursors_under_link_headers() {
    let srv = Server::start().await;
    let (alice, alice_token) = account(&srv, "rt226-sw-alice", SCOPES).await;
    let (bob, bob_token) = account(&srv, "rt226-sw-bob", SCOPES).await;
    let (carol, carol_token) = account(&srv, "rt226-sw-carol", SCOPES).await;
    follow(&srv, &bob_token, alice.id).await;
    seed_sweep_posts(&srv, &alice_token, &carol_token).await;
    seed_sweep_followers(&srv, &bob).await;
    seed_sweep_engagements(&srv, &alice_token, &carol).await;
    seed_sweep_relationships(&srv, &alice_token).await;

    let listings = vec![
        SweepListing {
            token: bob_token.clone(),
            path: "/api/v1/timelines/home".to_owned(),
            who: "home timeline",
        },
        SweepListing {
            token: alice_token.clone(),
            // Carol's history: a clean twenty-five public rows. Alice's
            // own listing would also carry the twenty-five threads of
            // `seed_sweep_relationships` addressed to her as direct,
            // which is the account-statuses endpoint's rule, not a
            // pagination deviation.
            path: format!("/api/v1/accounts/{}/statuses", carol.id),
            who: "account statuses",
        },
        SweepListing {
            token: bob_token,
            path: "/api/v1/notifications".to_owned(),
            who: "notifications",
        },
        SweepListing {
            token: alice_token.clone(),
            path: format!("/api/v1/accounts/{}/followers", bob.id),
            who: "followers",
        },
        SweepListing {
            token: alice_token.clone(),
            path: "/api/v1/favourites".to_owned(),
            who: "favourites",
        },
        SweepListing {
            token: alice_token.clone(),
            path: "/api/v1/bookmarks".to_owned(),
            who: "bookmarks",
        },
        SweepListing {
            token: alice_token.clone(),
            path: "/api/v1/mutes".to_owned(),
            who: "mutes",
        },
        SweepListing {
            token: alice_token.clone(),
            path: "/api/v1/blocks".to_owned(),
            who: "blocks",
        },
        SweepListing {
            token: alice_token,
            path: "/api/v1/conversations".to_owned(),
            who: "conversations",
        },
    ];
    for listing in &listings {
        sweep(&srv, listing).await;
    }
}

// ──────────────────────────  Scenario 5: emoji packs  ──────────────────────────

/// A pack listing's names, sorted: only their presence matters to the
/// scenarios, and the listing keys packs by name.
fn pack_names(body: &Value) -> Vec<String> {
    let mut names: Vec<String> = body["packs"]
        .as_object()
        .expect("the listing keys its packs")
        .keys()
        .cloned()
        .collect();
    names.sort_unstable();
    names
}

/// The pack directory names inside an emoji root, sorted: the level
/// `GET /packs` reads.
fn root_pack_dirs(root: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .expect("the root reads")
        .filter_map(|entry| {
            let entry = entry.expect("the entry reads");
            entry
                .path()
                .is_dir()
                .then(|| entry.file_name().to_string_lossy().into_owned())
        })
        .collect();
    names.sort_unstable();
    names
}

/// A zip archive holding `entries`.
fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, payload) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .expect("the entry opens");
        writer.write_all(payload).expect("the entry writes");
    }
    writer.finish().expect("the archive closes").into_inner()
}

/// The named bytes of an archive body.
fn read_zip(body: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut reader = zip::ZipArchive::new(Cursor::new(body.to_vec())).expect("the body is a zip");
    let mut entries = BTreeMap::new();
    for index in 0..reader.len() {
        let mut file = reader.by_index(index).expect("the entry reads");
        let name = file.name().expect("the entry has a name").into_owned();
        let mut bytes = Vec::new();
        let read: usize = file.read_to_end(&mut bytes).expect("the entry drains");
        assert_eq!(read, bytes.len(), "entry {name} came back complete");
        let _past = entries.insert(name, bytes);
    }
    entries
}

/// A `Server` with the emoji root moved into a fresh scratch directory.
async fn emoji_server() -> (Server, tempfile::TempDir) {
    let root = tempfile::TempDir::new().expect("a scratch emoji root");
    let mut config = common::test_config();
    config.emoji_dir = root.path().to_path_buf();
    let srv = Server::with_config(config).await;
    (srv, root)
}

/// `GET /packs` and `GET /api/v1/custom_emojis` agree on the freshly
/// filled pack: one pack, one synced emoji, its image on the static
/// route's address and its category the pack name.
async fn assert_pack_listed(srv: &Server) {
    let listing = get_value(srv, None, "/api/v1/pleroma/emoji/packs").await;
    assert_eq!(pack_names(&listing), ["dogs"], "the pack lists");
    assert_eq!(listing["count"], json!(1), "with its total");
    let emojis = get_value(srv, None, "/api/v1/custom_emojis").await;
    let woof = emojis
        .as_array()
        .expect("the endpoint answers an array")
        .iter()
        .find(|emoji| emoji["shortcode"] == json!("woof"))
        .expect("the resync served the new file");
    assert_eq!(
        woof["url"],
        json!(format!("{BASE}/emoji/dogs/woof.png")),
        "the image hangs off the static route"
    );
    assert_eq!(woof["category"], json!("dogs"), "the pack is its category");
}

/// The sharing `PATCH` and the archive it unlocks: the zip comes back with
/// the manifest and the file byte for byte.
async fn assert_share_and_archive(srv: &Server, admin_token: &str, woof_png: &[u8]) {
    let _shared = write_ok(
        srv,
        Verb::Patch,
        "/api/v1/pleroma/emoji/pack?name=dogs",
        admin_token,
        &json!({ "metadata": { "share-files": true } }),
    )
    .await;
    let archive = call(
        srv,
        Verb::Get,
        "/api/v1/pleroma/emoji/packs/archive?name=dogs",
        None,
        None,
    )
    .await;
    expect(&archive, StatusCode::OK, "the archive download");
    assert_eq!(
        archive.header("content-type"),
        Some("application/zip"),
        "the body is a zip"
    );
    let entries = read_zip(&archive.body);
    let names: Vec<&str> = entries.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        ["pack.json", "woof.png"],
        "the manifest plus the file"
    );
    let manifest: Value = serde_json::from_slice(&entries["pack.json"]).expect("a manifest");
    assert_eq!(
        manifest["files"]["woof"],
        json!("woof.png"),
        "mapping its emoji"
    );
    assert_eq!(
        entries["woof.png"], woof_png,
        "the file bytes survived the round trip"
    );
}

/// The full pack journey on the served instance: `POST /pack` creates the
/// pack, `POST /packs/files` lands an uploaded image and resyncs the
/// custom emojis so `GET /api/v1/custom_emojis` lists it and a
/// `:shortcode:` reaction on a real status renders its image, `PATCH`
/// shares the pack, `GET /packs/archive` zips it back out with the
/// manifest, and the file itself serves from the static emoji route.
#[tokio::test]
async fn a_pack_is_created_filled_shared_archived_and_reacted_with() {
    let (srv, root) = emoji_server().await;
    let (_admin, admin_token) = admin(&srv, "rt226-emoji-admin", "admin:read admin:write").await;
    let (reactor, reactor_token) = account(&srv, "rt226-emoji-reactor", SCOPES).await;

    let _created = write_ok(
        &srv,
        Verb::Post,
        "/api/v1/pleroma/emoji/pack?name=dogs",
        &admin_token,
        &json!({}),
    )
    .await;
    let woof_png = png_fixture(11);
    let files = post_multipart(
        &srv,
        "/api/v1/pleroma/emoji/packs/files?name=dogs",
        &admin_token,
        &[
            Part::text("shortcode", "woof"),
            Part::file("file", "whatever name.png", woof_png.clone()),
        ],
    )
    .await;
    expect(&files, StatusCode::OK, "the pack file upload");
    assert_eq!(
        files.json(),
        json!({"woof": "woof.png"}),
        "the files object after the add, the client name replaced"
    );

    assert_pack_listed(&srv).await;

    let status = post_status(&srv, &reactor_token, "react to this", "public").await;
    let status_id = id_of(&status);
    let reacted = call(
        &srv,
        Verb::Put,
        &format!("/api/v1/pleroma/statuses/{status_id}/reactions/:woof:"),
        Some(&reactor_token),
        None,
    )
    .await;
    expect(&reacted, StatusCode::OK, "the custom-emoji reaction");
    assert_eq!(
        reacted.json()["pleroma"]["emoji_reactions"],
        json!([{
            "name": ":woof:",
            "count": 1,
            "me": true,
            "url": format!("{BASE}/emoji/dogs/woof.png"),
            "account_ids": [reactor.id.to_string()],
        }]),
        "the shortcode reacts with its image"
    );

    assert_share_and_archive(&srv, &admin_token, &woof_png).await;

    let served = call(&srv, Verb::Get, "/emoji/dogs/woof.png", None, None).await;
    expect(&served, StatusCode::OK, "the static emoji route");
    assert_eq!(served.body.as_ref(), woof_png.as_slice(), "byte for byte");
    assert_eq!(
        root_pack_dirs(root.path()),
        ["dogs"],
        "the pack lives under the scratch root only"
    );
}

/// The mock remote's script: the one-pack listing the download reads and
/// the archive it fetches.
#[derive(Debug)]
struct RemoteScript {
    /// The body of `GET /api/v1/pleroma/emoji/pack`.
    listing: Value,
    /// The bytes of `GET /api/v1/pleroma/emoji/packs/archive`.
    archive: Vec<u8>,
}

/// Serves the pack listing the download's two reads expect.
async fn remote_pack_list(State(script): State<Arc<RemoteScript>>) -> Json<Value> {
    Json(script.listing.clone())
}

/// Serves the pack archive.
async fn remote_pack_archive(State(script): State<Arc<RemoteScript>>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/zip")
        .body(Body::from(script.archive.clone()))
        .expect("the mock answer is a valid response")
}

/// A mock remote instance serving `script` on a loopback port: its base
/// URL and its task — the instance `POST /packs/download` installs from,
/// reachable because the test config allows private hosts.
async fn spawn_pack_remote(script: RemoteScript) -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port for the mock remote");
    let base = format!("http://{}", listener.local_addr().expect("the address"));
    let app = Router::new()
        .route("/api/v1/pleroma/emoji/pack", get(remote_pack_list))
        .route(
            "/api/v1/pleroma/emoji/packs/archive",
            get(remote_pack_archive),
        )
        .with_state(Arc::new(script));
    let server = tokio::spawn(async move {
        if axum::serve(listener, app).await.is_err() {
            // A dead remote reports itself through the download's answer.
        }
    });
    (base, server)
}

/// Installing a pack from the outside, end to end: `POST /packs/download`
/// reads a mock remote instance's listing over real HTTP, fetches the
/// shared archive it names, and lands the pack under its local name with
/// its manifest's metadata, its files on disk and its emoji served.
#[tokio::test]
async fn a_pack_downloads_from_a_mock_remote() {
    let (srv, root) = emoji_server().await;
    let (_admin, admin_token) = admin(&srv, "rt226-installer", "admin:read admin:write").await;

    let cat_png = png_fixture(21);
    let archive = build_zip(&[
        (
            "pack.json",
            br#"{"pack": {"license": "MIT", "share-files": true}, "files": {"cat": "cat.png"}}"#
                .as_slice(),
        ),
        ("cat.png", cat_png.as_slice()),
    ]);
    let listing = json!({
        "pack": {"license": "MIT", "share-files": true},
        "files": {"cat": "cat.png"},
        "files_count": 1,
    });
    let (base, remote) = spawn_pack_remote(RemoteScript { listing, archive }).await;

    let downloaded = write_ok(
        &srv,
        Verb::Post,
        "/api/v1/pleroma/emoji/packs/download",
        &admin_token,
        &json!({ "url": base, "name": "cats", "as": "imported" }),
    )
    .await;
    remote.abort();
    assert_eq!(downloaded, json!("ok"), "Pleroma's ok body");

    let installed = get_value(&srv, None, "/api/v1/pleroma/emoji/packs").await;
    assert_eq!(pack_names(&installed), ["imported"], "the installed pack");
    let emojis = get_value(&srv, None, "/api/v1/custom_emojis").await;
    assert!(
        emojis
            .as_array()
            .expect("an array")
            .iter()
            .any(|emoji| emoji["shortcode"] == json!("cat")),
        "the installed emoji serves: {emojis}"
    );
    assert_eq!(
        root_pack_dirs(root.path()),
        ["imported"],
        "the pack directory landed on disk"
    );
    let served = call(&srv, Verb::Get, "/emoji/imported/cat.png", None, None).await;
    expect(&served, StatusCode::OK, "the installed file serves");
    assert_eq!(
        served.body.as_ref(),
        cat_png.as_slice(),
        "the archive's bytes"
    );
}

/// The other install path: `GET /packs/import` writes the manifest for a
/// loose directory dropped into the emoji root by hand, so its files
/// become pack entries named after their stems — and a second import
/// finds nothing left to do.
#[tokio::test]
async fn loose_pack_directories_import_by_their_file_stems() {
    let (srv, root) = emoji_server().await;
    let (_admin, admin_token) = admin(&srv, "rt226-looser", "admin:read admin:write").await;

    std::fs::create_dir_all(root.path().join("loose")).expect("the loose dir writes");
    std::fs::write(root.path().join("loose/bark.png"), png_fixture(22))
        .expect("the loose file writes");

    let imported = get_value(
        &srv,
        Some(&admin_token),
        "/api/v1/pleroma/emoji/packs/import",
    )
    .await;
    assert_eq!(imported, json!(["loose"]), "the loose directory reported");
    let listing = get_value(&srv, None, "/api/v1/pleroma/emoji/packs").await;
    assert_eq!(pack_names(&listing), ["loose"], "now a pack");
    let manifest = std::fs::read_to_string(root.path().join("loose").join("pack.json"))
        .expect("the import wrote the manifest");
    assert!(
        manifest.contains("bark"),
        "the file stem keyed its emoji: {manifest}"
    );
    let emojis = get_value(&srv, None, "/api/v1/custom_emojis").await;
    assert!(
        emojis
            .as_array()
            .expect("an array")
            .iter()
            .any(|emoji| emoji["shortcode"] == json!("bark")),
        "the imported emoji serves: {emojis}"
    );
    let after = get_value(
        &srv,
        Some(&admin_token),
        "/api/v1/pleroma/emoji/packs/import",
    )
    .await;
    assert_eq!(after, json!([]), "everything has a manifest now");
}
