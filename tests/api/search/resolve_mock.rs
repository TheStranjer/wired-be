//! The federation fixtures the resolve tests run against: a mock
//! `ActivityPub` peer on loopback serving `zoe`'s actor and one public note
//! per name, with the request counter the "was this attempted at all"
//! assertions read; the origin of a port nothing listens on, for the
//! unreachable peer; and the database readers that pull the mirrored rows
//! back out for the tests to compare against.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    extract::{Path, State},
    http::header::CONTENT_TYPE,
    response::{IntoResponse as _, Response},
    routing::get as route_get,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpListener, task::JoinHandle};
use wired_be::models::user;

/// The public addressing a fetched note needs to ingest as the world's.
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// A mock `ActivityPub` peer on loopback: `zoe`'s actor document and one
/// public note per name, with the request counter the "was this attempted at
/// all" assertions read.
pub(super) struct MockPeer {
    /// The peer's `http://{addr}` origin.
    base: String,
    /// Requests it has answered.
    hits: Arc<AtomicUsize>,
    /// The task serving it, aborted when the test is done.
    server: JoinHandle<()>,
}

/// The mock's shared state: the origin its documents must name themselves
/// with, beside the hit counter.
#[derive(Clone)]
struct Peer {
    /// The `http://{addr}` origin.
    base: String,
    /// Requests answered.
    hits: Arc<AtomicUsize>,
}

/// A body labelled the way an `ActivityPub` peer labels one.
fn ap_json(document: &Value) -> Response {
    (
        [(CONTENT_TYPE, "application/activity+json")],
        document.to_string(),
    )
        .into_response()
}

/// Counts the hit and answers `zoe`'s self-identifying actor document.
async fn serve_actor(State(peer): State<Peer>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    let id = format!("{}/users/zoe", peer.base);
    ap_json(&json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Person",
        "id": id,
        "preferredUsername": "zoe",
        "name": "Zoe",
        "inbox": format!("{id}/inbox"),
        "publicKey": {
            "id": format!("{id}#main-key"),
            "owner": id,
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
        },
    }))
}

/// Counts the hit and answers the self-identifying public note named by the
/// path, authored by this peer's `zoe`.
async fn serve_object(State(peer): State<Peer>, Path(name): Path<String>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    ap_json(&json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Note",
        "id": format!("{}/objects/{name}", peer.base),
        "attributedTo": format!("{}/users/zoe", peer.base),
        "to": [PUBLIC],
        "content": format!("<p>remote post {name}</p>"),
        "published": "2024-03-01T10:00:00.000Z",
    }))
}

impl MockPeer {
    /// Binds a port and serves `zoe` at `{base}/users/zoe` and any note at
    /// `{base}/objects/{name}`.
    pub(super) async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock peer binds a port");
        let address = listener.local_addr().expect("a bound address");
        let peer = Peer {
            base: format!("http://{address}"),
            hits: Arc::new(AtomicUsize::new(0)),
        };
        Self {
            base: peer.base.clone(),
            hits: Arc::clone(&peer.hits),
            server: tokio::spawn(async move {
                let app = Router::new()
                    .route("/users/{nick}", route_get(serve_actor))
                    .route("/objects/{name}", route_get(serve_object))
                    .with_state(peer);
                if let Err(err) = axum::serve(listener, app).await {
                    tracing::debug!("mock peer stopped: {err}");
                }
            }),
        }
    }

    /// The actor URL this peer serves.
    pub(super) fn actor_url(&self) -> String {
        format!("{}/users/zoe", self.base)
    }

    /// The `host:port` authority of [`MockPeer::base`]: what the mirror
    /// write puts into `user@domain` after the loopback `http://` scheme
    /// (decision 7, port included — the fetch names the domain exactly).
    pub(super) fn authority(&self) -> String {
        self.base
            .strip_prefix("http://")
            .expect("the mock origin is http loopback")
            .to_owned()
    }

    /// The object URL of fixture note `name`.
    pub(super) fn object_url(&self, name: &str) -> String {
        format!("{}/objects/{name}", self.base)
    }

    /// The requests answered so far.
    pub(super) fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for MockPeer {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// The origin of a port nothing listens on any more: every request to it
/// fails to connect, which is how an unreachable peer reads here without
/// waiting on any network or timer.
pub(super) async fn dead_origin() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port is free to bind");
    let base = format!("http://{}", listener.local_addr().expect("a bound address"));
    drop(listener);
    base
}

/// The stored mirror id of the mock peer's `zoe`.
pub(super) async fn zoe_id(pool: &PgPool, ap_id: &str) -> String {
    user::get_by_ap_id(pool, ap_id)
        .await
        .unwrap()
        .expect("the resolved actor is mirrored")
        .id
        .to_string()
}

/// The ids of the live statuses stored under `uri`, as a list.
pub(super) async fn status_rows_by_uri(pool: &PgPool, uri: &str) -> Vec<String> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT id::text FROM statuses WHERE uri = $1")
        .bind(uri)
        .fetch_all(pool)
        .await
        .unwrap();
    rows.into_iter().map(|(id,)| id).collect()
}

/// The number of stored statuses whose uri ends in fixture note `name`.
pub(super) async fn stored_notes(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri LIKE $1")
        .bind(format!("%/objects/{name}"))
        .fetch_one(pool)
        .await
        .unwrap()
}
