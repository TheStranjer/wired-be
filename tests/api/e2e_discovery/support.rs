//! The scenario actors, request helpers and route constants the discovery
//! end-to-end tests share. The test modules are its children, so they reach
//! every helper through `use super::*`.

mod feeds_cards;
mod filters;
mod remote_mention;
mod search;

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpListener, task::JoinHandle};
use wired_be::{
    models::user::{self, User},
    state::AppState,
};

use crate::common;

/// The status create/read route.
const STATUSES: &str = "/api/v1/statuses";
/// The v1 combined search.
const SEARCH_V1: &str = "/api/v1/search";
/// The v2 combined search.
const SEARCH_V2: &str = "/api/v2/search";
/// The account search.
const ACCOUNT_SEARCH: &str = "/api/v1/accounts/search";
/// The v1 filters CRUD route.
const FILTERS: &str = "/api/v1/filters";
/// The public timeline read.
const PUBLIC: &str = "/api/v1/timelines/public";
/// The home timeline read.
const HOME: &str = "/api/v1/timelines/home";
/// The caller's notification listing.
const NOTIFICATIONS: &str = "/api/v1/notifications";
/// The scopes every scenario actor carries: reads, writes and follows.
const SCOPES: &str = "read write follow";
/// The instance origin of [`common::test_config`].
const BASE: &str = "https://test.example";
/// The `ActivityStreams` public collection, as the `Create` addresses it.
const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// A local user named `nickname` plus a bearer token granting [`SCOPES`].
async fn user(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, SCOPES).await;
    (created, token)
}

/// The router and the [`AppState`] behind it over `pool`: requests ride
/// the router, [`common::run_all_jobs`] drains on the state.
fn app_and_state(pool: PgPool) -> (Router, AppState) {
    let state = common::test_app_state(pool, common::test_config());
    (wired_be::app(state.clone()), state)
}

/// `POST /api/v1/statuses` as `token` with the JSON `body`, asserted 200
/// and answered as the status entity it wrote.
async fn created(app: &Router, token: &str, body: Value) -> Value {
    let response = common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the fixture post creates");
    response.json_value()
}

/// A status of `text` and `visibility` by `token`, answered as its entity.
async fn published(app: &Router, token: &str, text: &str, visibility: &str) -> Value {
    created(
        app,
        token,
        json!({"status": text, "visibility": visibility}),
    )
    .await
}

/// `GET`s `path` with `token`, answered raw.
async fn read(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// The ids of a rendered status array, in the order it came back.
fn ids_of(listing: &Value) -> Vec<String> {
    listing
        .as_array()
        .expect("a status listing is an array")
        .iter()
        .map(|status| {
            status["id"]
                .as_str()
                .expect("every status has a string id")
                .to_owned()
        })
        .collect()
}

/// The answered listing's ids, newest first.
fn page_ids(response: &common::TestResponse) -> Vec<String> {
    common::assert_answer(response, StatusCode::OK, "the timeline reads");
    ids_of(&response.json_value())
}

/// The `statuses` arm of an answered search document.
fn searched(response: &common::TestResponse) -> Vec<String> {
    common::assert_answer(response, StatusCode::OK, "the search answers");
    ids_of(&response.json_value()["statuses"])
}

/// The `id` of a created status entity.
fn id_of(entity: &Value) -> String {
    entity["id"]
        .as_str()
        .expect("the entity has a string id")
        .to_owned()
}

/// A local follow of `target` by the holder of `token`.
async fn follow(app: &Router, token: &str, target: &User) {
    let response = common::TestRequest::post(&format!("/api/v1/accounts/{}/follow", target.id))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the fixture follow is granted");
}

/// `POST /api/v1/filters` as `token`, asserted 200 and answered as the
/// filter entity it wrote.
async fn filter_created(app: &Router, token: &str, body: Value) -> Value {
    let response = common::TestRequest::post(FILTERS)
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the fixture filter creates");
    response.json_value()
}

/// The phrases of an answered `GET /api/v1/filters`, in listing order.
fn listed_phrases(response: &common::TestResponse) -> Vec<String> {
    common::assert_answer(response, StatusCode::OK, "the filter listing reads");
    response
        .json_value()
        .as_array()
        .expect("the listing is an array")
        .iter()
        .map(|filter| {
            filter["phrase"]
                .as_str()
                .expect("every filter has a phrase")
                .to_owned()
        })
        .collect()
}

/// The types of an answered notification listing, in listing order.
fn notification_types(response: &common::TestResponse) -> Vec<String> {
    common::assert_answer(response, StatusCode::OK, "the notification listing reads");
    response
        .json_value()
        .as_array()
        .expect("the listing is an array")
        .iter()
        .map(|row| {
            row["type"]
                .as_str()
                .expect("every notification has a type")
                .to_owned()
        })
        .collect()
}

/// One recorded inbox `POST`: the request headers and the decoded body.
#[derive(Clone, Debug)]
struct ReceivedPost {
    /// The headers as lower-cased name/value pairs.
    headers: Vec<(String, String)>,
    /// The activity JSON the peer was asked to fold.
    body: Value,
}

impl ReceivedPost {
    /// The value of request header `name`, `None` when it was not sent.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_key, value)| value.as_str())
    }
}

/// The shared half of the mock peer: its origin and the counters and
/// captures the tests assert on.
#[derive(Clone, Debug)]
struct Peer {
    /// The `http://{addr}` origin this peer answers on.
    base: String,
    /// `WebFinger` requests it answered.
    webfinger_hits: Arc<AtomicUsize>,
    /// Actor dereferences it answered.
    actor_hits: Arc<AtomicUsize>,
    /// Card-page fetches it answered.
    page_hits: Arc<AtomicUsize>,
    /// The inbox `POST`s it recorded.
    inbox_posts: Arc<Mutex<Vec<ReceivedPost>>>,
}

/// The loopback mock federation peer (scenarios 7 and 8 upstream): an HTML
/// card page, a self-identifying `WebFinger`, actor documents in the
/// `follow_by_uri.rs` shape, and an inbox that records every delivery.
struct MockPeer {
    /// The `http://{addr}` origin it answers on.
    base: String,
    /// The shared counters and captures of its handlers.
    peer: Peer,
    /// The task serving it, aborted when the test is done.
    server: JoinHandle<()>,
}

impl MockPeer {
    /// Binds a port and serves the whole mock surface.
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock peer binds a port");
        let base = format!("http://{}", listener.local_addr().expect("a bound address"));
        let peer = Peer {
            base: base.clone(),
            webfinger_hits: Arc::new(AtomicUsize::new(0)),
            actor_hits: Arc::new(AtomicUsize::new(0)),
            page_hits: Arc::new(AtomicUsize::new(0)),
            inbox_posts: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route("/.well-known/webfinger", get(webfinger))
            .route("/users/{nick}", get(actor))
            .route("/users/{nick}/inbox", post(inbox))
            .route("/page", get(page))
            .with_state(peer.clone());
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock peer stopped: {err}");
            }
        });
        Self { base, peer, server }
    }

    /// The `WebFinger` requests this peer answered.
    fn webfinger_hits(&self) -> usize {
        self.peer.webfinger_hits.load(Ordering::SeqCst)
    }

    /// The actor dereferences this peer answered.
    fn actor_hits(&self) -> usize {
        self.peer.actor_hits.load(Ordering::SeqCst)
    }

    /// The card-page fetches this peer answered.
    fn page_hits(&self) -> usize {
        self.peer.page_hits.load(Ordering::SeqCst)
    }

    /// The inbox deliveries this peer recorded, oldest first.
    fn received(&self) -> Vec<ReceivedPost> {
        self.peer
            .inbox_posts
            .lock()
            .expect("the capture lock")
            .clone()
    }
}

impl Drop for MockPeer {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Answers any `acct:` `WebFinger` with a JRD whose subject echoes the
/// asked resource and whose self link points at this peer's own actor.
async fn webfinger(
    State(peer): State<Peer>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let _hits = peer.webfinger_hits.fetch_add(1, Ordering::SeqCst);
    let Some(resource) = params
        .get("resource")
        .and_then(|value| value.strip_prefix("acct:"))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let nickname = resource.split('@').next().unwrap_or(resource);
    let jrd = json!({
        "subject": format!("acct:{resource}"),
        "links": [{
            "rel": "self",
            "type": "application/activity+json",
            "href": format!("{}/users/{nickname}", peer.base),
        }],
    });
    ([(CONTENT_TYPE, "application/jrd+json")], jrd.to_string()).into_response()
}

/// Answers the self-identifying actor document, the `follow_by_uri.rs`
/// shape, with an inbox under its own id.
async fn actor(State(peer): State<Peer>, Path(nick): Path<String>) -> Response {
    let _hits = peer.actor_hits.fetch_add(1, Ordering::SeqCst);
    let id = format!("{}/users/{nick}", peer.base);
    let document = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Person",
        "id": id,
        "preferredUsername": nick,
        "name": "Remote",
        "inbox": format!("{id}/inbox"),
        "publicKey": {
            "id": format!("{id}#main-key"),
            "owner": id,
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
        },
    });
    (
        [(CONTENT_TYPE, "application/activity+json")],
        document.to_string(),
    )
        .into_response()
}

/// Records the delivery and accepts it: the happy remote inbox.
async fn inbox(State(peer): State<Peer>, headers: HeaderMap, body: Bytes) -> Response {
    let captured = ReceivedPost {
        headers: headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect(),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    };
    peer.inbox_posts
        .lock()
        .expect("the capture lock")
        .push(captured);
    StatusCode::ACCEPTED.into_response()
}

/// Serves the Open Graph page the card is made from.
async fn page(State(peer): State<Peer>) -> Response {
    let _hits = peer.page_hits.fetch_add(1, Ordering::SeqCst);
    let html = format!(
        "<html><head><title>Plain Title</title>\
         <meta property=\"og:title\" content=\"Mock og Title\">\
         <meta property=\"og:description\" content=\"Mock og description\">\
         <meta property=\"og:image\" content=\"{}/og.png\">\
         <meta property=\"og:type\" content=\"article\">\
         </head><body><p>hello from the mock</p></body></html>",
        peer.base
    );
    ([(CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}
