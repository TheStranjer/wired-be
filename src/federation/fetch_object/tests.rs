//! Tests of the object fetcher over loopback mock peers: the shared peer
//! fixtures live here, the cases in the submodules.

mod backfill;
mod fetch;
mod quote_backfill;
mod resolve;

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse as _, Response},
    routing::get,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::{fetch::MAX_OBJECT_BODY_BYTES, *};
use crate::test_server::{bind, origin, spawn};
use crate::{
    config::{Config, TEST_BASE_URL},
    federation::{dispatch::FederationError, fetch_actor::ACTOR_ACCEPT},
    http_client::HttpClient,
    jobs::JobError,
    models::{
        activitypub_actor::ActivityPubActorWrite,
        status::{self, NewStatus},
        user::{self, Flag, NewUser, User},
    },
    services::ingest::{self, IngestOutcome},
    state::AppState,
    test_fixtures::write_actor,
};

/// `User-Agent` for the test clients.
const TEST_UA: &str = "Wired (https://test.example)";
/// The content type a federation peer answers an object document with.
const OBJECT_CONTENT_TYPE: &str = "application/activity+json";
/// The public addressing every fixture note carries.
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The actor every fixture note is authored by, under the peer's base.
const ALICE: &str = "alice";

/// The state under test: `pool` on the default test config — whose
/// `HTTP_ALLOW_PRIVATE_HOSTS` lets the fetch client reach the mocks.
fn test_state(pool: PgPool) -> AppState {
    state_with_config(pool, Config::for_tests())
}

/// The state under test on `config` instead of the stock test one.
fn state_with_config(pool: PgPool, config: Config) -> AppState {
    AppState {
        http_client: HttpClient::new_for_tests(TEST_UA),
        ..AppState::for_tests(pool, config)
    }
}

/// The state every mock-peer handler runs with: its own origin, the
/// request count, the last `Accept` seen, and the fixture's thread
/// shape (object name → answered object name).
#[derive(Clone)]
struct Peer {
    /// The `http://{addr}` origin of this mock server.
    base: String,
    /// Requests this peer answered, across all its routes.
    hits: Arc<AtomicUsize>,
    /// The `Accept` header of the most recent request, when recorded.
    last_accept: Arc<Mutex<Option<String>>>,
    /// Which fixture objects answer which other fixture object.
    parents: Arc<Mutex<HashMap<String, String>>>,
    /// Which fixture objects quote which url (`quoteUrl`).
    quotes: Arc<Mutex<HashMap<String, String>>>,
}

/// A mock peer at `base`: nothing fetched yet, no thread shape.
fn peer_at(base: &str) -> Peer {
    Peer {
        base: base.to_owned(),
        hits: Arc::new(AtomicUsize::new(0)),
        last_accept: Arc::new(Mutex::new(None)),
        parents: Arc::new(Mutex::new(HashMap::new())),
        quotes: Arc::new(Mutex::new(HashMap::new())),
    }
}

/// A minimal actor document for `id`, the way a `Mastodon` peer serves
/// one: everything [`parse_actor`] requires is present.
///
/// [`parse_actor`]: crate::federation::remote_actor::parse_actor
fn actor_document(id: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Person",
        "id": id,
        "preferredUsername": ALICE,
        "name": "First",
        "inbox": format!("{id}/inbox"),
        "publicKey": {
            "id": format!("{id}#main-key"),
            "owner": id,
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
        },
    })
}

/// The fixture note named `name` on `peer`: authored by its `alice`,
/// public, and answering whatever the peer's thread shape says it does.
fn note_document(peer: &Peer, name: &str) -> Value {
    let parent = peer.parents.lock().unwrap().get(name).cloned();
    let mut document = json!({
        "id": format!("{}/objects/{name}", peer.base),
        "type": "Note",
        "attributedTo": format!("{}/users/{ALICE}", peer.base),
        "to": [PUBLIC],
        "content": format!("<p>object {name}</p>"),
        "published": "2024-03-01T10:00:00.000Z",
    });
    if let Some(answered) = parent {
        document["inReplyTo"] = json!(format!("{}/objects/{answered}", peer.base));
    }
    if let Some(quoted) = peer.quotes.lock().unwrap().get(name) {
        document["quoteUrl"] = json!(quoted);
    }
    document
}

/// A body labelled [`OBJECT_CONTENT_TYPE`].
fn json_response(document: &Value) -> Response {
    (
        [(header::CONTENT_TYPE, OBJECT_CONTENT_TYPE)],
        document.to_string(),
    )
        .into_response()
}

/// The peer's standard surface: alice's actor document and any fixture
/// note by name.
fn peer_app(peer: Peer) -> Router {
    Router::new()
        .route("/users/{name}", get(serve_actor))
        .route("/objects/{name}", get(serve_object))
        .route("/hop/{n}", get(serve_hop))
        .with_state(peer)
}

/// Counts the hit and answers alice's self-identifying actor document.
async fn serve_actor(State(peer): State<Peer>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    json_response(&actor_document(&format!("{}/users/{ALICE}", peer.base)))
}

/// Counts the hit, records the `Accept`, and answers the fixture note
/// named by the path — which carries the peer's thread shape.
async fn serve_object(
    State(peer): State<Peer>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    if let Some(accept) = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
    {
        let _previous = peer.last_accept.lock().unwrap().replace(accept.to_owned());
    }
    json_response(&note_document(&peer, &name))
}

/// Counts the hit; `/hop/0` lands on the fixture note `landed` and
/// every other hop redirects one closer to it.
async fn serve_hop(State(peer): State<Peer>, Path(n): Path<u32>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    if n == 0 {
        return json_response(&note_document(&peer, "landed"));
    }
    (
        StatusCode::FOUND,
        [(header::LOCATION, format!("{}/hop/{}", peer.base, n - 1))],
    )
        .into_response()
}

/// A one-route server for one fixed document at one exact path.
fn fixed_app(path: &str, document: Value) -> Router {
    let exact = path.to_owned();
    Router::new()
        .route(&exact, get(serve_fixed))
        .with_state(document)
}

/// [`fixed_app`]'s handler.
async fn serve_fixed(State(document): State<Value>) -> Response {
    json_response(&document)
}

/// A one-route server whose exact `path` answers a 301 to `location`.
fn redirect_app(path: &str, location: String) -> Router {
    let exact = path.to_owned();
    Router::new()
        .route(&exact, get(serve_redirect))
        .with_state(location)
}

/// [`redirect_app`]'s handler.
async fn serve_redirect(State(location): State<String>) -> Response {
    (
        StatusCode::MOVED_PERMANENTLY,
        [(header::LOCATION, location)],
    )
        .into_response()
}

/// One fixed document plus the peer counter its deliveries run up.
#[derive(Clone)]
struct ServedDoc {
    /// The peer's request counter.
    hits: Arc<AtomicUsize>,
    /// The document the route answers with.
    document: Value,
}

/// A [`fixed_app`] whose deliveries the peer counts.
fn counted_fixed_app(peer: &Peer, path: &str, document: Value) -> Router {
    let exact = path.to_owned();
    Router::new()
        .route(&exact, get(serve_counted_fixed))
        .with_state(ServedDoc {
            hits: Arc::clone(&peer.hits),
            document,
        })
}

/// [`counted_fixed_app`]'s handler.
async fn serve_counted_fixed(State(served): State<ServedDoc>) -> Response {
    let _hits = served.hits.fetch_add(1, Ordering::SeqCst);
    json_response(&served.document)
}

/// The object url of fixture `name` on a peer answering at `base`.
fn object_url(base: &str, name: &str) -> String {
    format!("{base}/objects/{name}")
}

/// Stores the runtime MRF entry enabling the `simple` policy.
///
/// `lists` is its domain lists (`{"reject": [...]}` / `{"accept": [...]}`).
async fn set_simple_lists(pool: &PgPool, lists: &Value) {
    crate::services::runtime_config::write(
        pool,
        ":pleroma",
        ":mrf",
        &json!({"policies": ["simple"], "simple": lists}),
    )
    .await
    .expect("the mrf entry writes");
}

/// Counts the `statuses` rows stored under `uri`.
async fn rows_for_uri(pool: &PgPool, uri: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri = $1")
        .bind(uri)
        .fetch_one(pool)
        .await
        .expect("the row count reads")
}

/// The live `federator.backfill` job queued for `status_id`, if any:
/// its args and its attempt budget.
async fn pending_backfill(pool: &PgPool, status_id: Uuid) -> Option<(Value, i32)> {
    sqlx::query_as::<_, (Value, i32)>(
        "SELECT args, max_attempts FROM jobs
              WHERE kind = $1 AND unique_key = $2 AND state IN ('available', 'retryable')",
    )
    .bind(KIND_BACKFILL)
    .bind(backfill_key(status_id))
    .fetch_optional(pool)
    .await
    .expect("the job read")
}

/// The stored mirror of `{name}@peer`, inserted straight: the authors
/// the direct-ingest fixtures are written under.
async fn mirror_user(pool: &PgPool, name: &str, base: &str) -> User {
    let host = base.strip_prefix("http://").unwrap_or(base).to_owned();
    let stored = user::insert(
        pool,
        &NewUser {
            nickname: format!("{name}@{host}"),
            ap_id: format!("{base}/users/{name}"),
            local: Flag::new(false),
            domain: Some(host),
            ..NewUser::default()
        },
    )
    .await
    .expect("the mirror fixture inserts");
    let _row = write_actor(
        pool,
        &ActivityPubActorWrite {
            followers_url: Some(format!("{base}/users/{name}/followers")),
            ..ActivityPubActorWrite::bare(stored.id)
        },
    )
    .await;
    stored
}

/// The stored mirror of `alice@peer`.
async fn alice_mirror(pool: &PgPool, base: &str) -> User {
    mirror_user(pool, ALICE, base).await
}

/// Answers `body` labelled the way an HTML page is, to test the
/// content-type rule.
fn html_response(body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, "text/html")], body).into_response()
}

/// A peer answering `/gone` with 410, `/missing` with 404 and `/flaky`
/// with 500, plus `/html` (wrong media type) and `/broken` (claimed
/// JSON that is not).
fn refusal_app(peer: &Peer) -> Router {
    Router::new()
        .route("/gone", get(serve_gone).with_state(Arc::clone(&peer.hits)))
        .route(
            "/missing",
            get(serve_missing).with_state(Arc::clone(&peer.hits)),
        )
        .route(
            "/flaky",
            get(serve_flaky).with_state(Arc::clone(&peer.hits)),
        )
        .route("/html", get(serve_html).with_state(Arc::clone(&peer.hits)))
        .route(
            "/broken",
            get(serve_broken).with_state(Arc::clone(&peer.hits)),
        )
}

/// [`refusal_app`]'s status arms: each counts its hit.
async fn serve_gone(State(hits): State<Arc<AtomicUsize>>) -> StatusCode {
    let _hits = hits.fetch_add(1, Ordering::SeqCst);
    StatusCode::GONE
}

/// [`refusal_app`]'s 404 arm.
async fn serve_missing(State(hits): State<Arc<AtomicUsize>>) -> StatusCode {
    let _hits = hits.fetch_add(1, Ordering::SeqCst);
    StatusCode::NOT_FOUND
}

/// [`refusal_app`]'s 500 arm.
async fn serve_flaky(State(hits): State<Arc<AtomicUsize>>) -> StatusCode {
    let _hits = hits.fetch_add(1, Ordering::SeqCst);
    StatusCode::INTERNAL_SERVER_ERROR
}

/// [`refusal_app`]'s HTML arm.
async fn serve_html(State(hits): State<Arc<AtomicUsize>>) -> Response {
    let _hits = hits.fetch_add(1, Ordering::SeqCst);
    html_response("<html>not activity</html>")
}

/// [`refusal_app`]'s unparsable-body arm.
async fn serve_broken(State(hits): State<Arc<AtomicUsize>>) -> Response {
    let _hits = hits.fetch_add(1, Ordering::SeqCst);
    (
        [(header::CONTENT_TYPE, OBJECT_CONTENT_TYPE)],
        "this is not json",
    )
        .into_response()
}

/// A peer whose `/objects/moved` serves the fixture note `real`: the
/// moved-document arm of containment, where the refetch at the declared
/// id succeeds because that document names itself.
fn moved_app(peer: &Peer) -> Router {
    Router::new()
        .route("/objects/moved", get(serve_moved).with_state(peer.clone()))
        .merge(peer_app(peer.clone()))
}

/// [`moved_app`]'s handler: `real`'s document, served under `moved`.
async fn serve_moved(State(peer): State<Peer>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    json_response(&note_document(&peer, "real"))
}
