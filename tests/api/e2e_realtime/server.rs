//! The served application and its HTTP helpers: the real router on a
//! loopback port, the verbs and replies the scenarios call it with, the raw
//! multipart framing and the shared image fixture.

use std::{io::Cursor, net::SocketAddr};

use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
};
use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
use reqwest::RequestBuilder;
use serde_json::Value;
use sqlx::PgPool;
use tokio::task::JoinHandle;
use uuid::Uuid;
use wired_be::{
    config::{Config, TEST_BASE_URL},
    state::AppState,
    streaming::hub::{Audience, StreamEvent},
    test_db::TestDb,
};

use crate::common;

/// The instance origin every served response is expected to quote.
pub(super) const BASE: &str = TEST_BASE_URL;
/// The scopes the scenario actors carry: the root pair covers every
/// sub-scope the journeys touch (decision 9's scope tree).
pub(super) const SCOPES: &str = "read write";
/// The boundary every multipart body of this module is framed with.
const MULTIPART_BOUNDARY: &str = "wired-e2e-realtime-boundary";

// ───────────────────────────  The served app  ───────────────────────────

/// The application on a real port: the address to dial over HTTP and
/// WebSocket, the state to run jobs on, and the lease that keeps its
/// database alive.
#[derive(Debug)]
pub(super) struct Server {
    /// The loopback address the listener bound.
    addr: SocketAddr,
    /// The state the router runs on — the same one the scenario hands to
    /// [`common::run_all_jobs`].
    pub(super) state: AppState,
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
    pub(super) async fn start() -> Self {
        Self::with_config(common::test_config()).await
    }

    /// Serves the real application with `config`: how the emoji tests get
    /// their own pack root and the upload test its own uploads dir.
    pub(super) async fn with_config(config: Config) -> Self {
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
    pub(super) fn pool(&self) -> &PgPool {
        &self.state.db
    }

    /// The streaming endpoint's url with `query` appended after a `?`.
    pub(super) fn ws_url(&self, query: &str) -> String {
        if query.is_empty() {
            return format!("ws://{}/api/v1/streaming", self.addr);
        }
        format!("ws://{}/api/v1/streaming?{query}", self.addr)
    }

    /// Puts one bare control `update` naming `reader` on the hub the served
    /// router reads, for the negative assertions that must bound a socket's
    /// silence with an event only that socket's stream carries.
    pub(super) fn publish_control(&self, stream: &str, reader: Uuid, marker: &str) {
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
pub(super) enum Verb {
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
pub(super) struct Reply {
    /// The response status.
    status: StatusCode,
    /// The response headers.
    headers: HeaderMap,
    /// The buffered body.
    pub(super) body: Bytes,
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
    pub(super) fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("the answer is JSON")
    }

    /// The body decoded as text.
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The header `name` as text, `None` when absent.
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// Runs `verb path` against `srv`, presenting `token` when given and a JSON
/// `body` when one is passed.
pub(super) async fn call(
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
pub(super) fn expect(reply: &Reply, status: StatusCode, who: &str) {
    assert_eq!(
        reply.status,
        status,
        "{who}: {} {}",
        reply.status.as_u16(),
        reply.text()
    );
}

/// The `GET` of `path`, asserted 200 and parsed.
pub(super) async fn get_json(srv: &Server, token: Option<&str>, path: &str) -> Reply {
    let reply = call(srv, Verb::Get, path, token, None).await;
    expect(&reply, StatusCode::OK, &format!("GET {path}"));
    reply
}

/// The `GET` of `path` as JSON, asserted 200.
pub(super) async fn get_value(srv: &Server, token: Option<&str>, path: &str) -> Value {
    get_json(srv, token, path).await.json()
}

/// A write of `body`, asserted 200 and parsed.
pub(super) async fn write_ok(
    srv: &Server,
    verb: Verb,
    path: &str,
    token: &str,
    body: &Value,
) -> Value {
    let reply = call(srv, verb, path, Some(token), Some(body)).await;
    expect(&reply, StatusCode::OK, path);
    reply.json()
}

/// A body-less action `POST`, asserted 200 and parsed.
pub(super) async fn action_ok(srv: &Server, path: &str, token: &str) -> Value {
    let reply = call(srv, Verb::Post, path, Some(token), None).await;
    expect(&reply, StatusCode::OK, path);
    reply.json()
}

/// One raw `multipart/form-data` part for [`post_multipart`].
#[derive(Debug)]
pub(super) struct Part {
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
    pub(super) fn text(name: &'static str, value: &'static str) -> Self {
        Self {
            name,
            filename: None,
            content_type: None,
            bytes: value.as_bytes().to_vec(),
        }
    }

    /// A file part uploaded under `filename`.
    pub(super) fn file(name: &'static str, filename: &'static str, bytes: Vec<u8>) -> Self {
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
pub(super) async fn post_multipart(srv: &Server, path: &str, token: &str, parts: &[Part]) -> Reply {
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

/// A decodable 8x8 `PNG` of `seed`-shifted colors: bytes the upload
/// pipeline detects, decodes and stores.
pub(super) fn png_fixture(seed: u32) -> Vec<u8> {
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
