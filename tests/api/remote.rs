//! The shared loopback mock federation peer (WBE-224 slice 1): one real HTTP
//! server a federation test points this instance at.
//!
//! Both halves of a round trip are real: the instance fetches this peer's
//! actors and delivers to its inboxes over the wire, and the peer posts signed
//! activities back through the instance's own inbox routes. A [`MockPeer`]
//! serves:
//!
//! - `GET /users/{nick}` — a self-identifying `Person` actor with a *real* RSA
//!   public key, a personal inbox under its own id and the peer-wide shared
//!   inbox in `endpoints`, so a delivery addressed to either lands in the
//!   capture and an inbound signature verifies against the key the document
//!   names ([`MockPeer::deliver`] signs with the matching private half);
//! - `GET /relay` — a `Service` actor (the shape relays are served as) whose
//!   inbox *is* the shared one, which is what lets a test watch the
//!   publisher's per-inbox deduplication;
//! - `GET /objects/{name}` — a public `Note` authored by the peer's own
//!   `author` account, the object a relay re-broadcasts;
//! - `GET /.well-known/webfinger` — a JRD echoing the asked `acct:` resource
//!   and naming that nickname's actor, so `acct:` handles resolve here;
//! - `POST /inbox` and `POST /users/{nick}/inbox` — both record the exact
//!   bytes, headers and path they were sent to and answer `202 Accepted`,
//!   or the next answer the test scripted with [`MockPeer::script_inbox`]
//!   / [`MockPeer::script_inbox_permanently`] (a `500`, a `503` with a
//!   `Retry-After`, a `410` that never lifts — whatever the scenario is
//!   making the delivery worker see);
//! - [`MockPeer::rotate_keys`] swaps the key pair every actor document
//!   publishes from then on, so a scenario can stand the peer in a key
//!   rotation and compare what this instance still holds against what is
//!   served now.
//!
//! Sending is [`MockPeer::deliver`], which signs as one of the peer's actors
//! with [`wired_be::federation::signature::sign_request`] and rides any path
//! the caller names through the test router in-process; [`MockPeer::activity`]
//! builds the documents with ids on the peer's own host, which is what the
//! dispatcher's containment check asks of them. Reading is
//! [`MockPeer::received`], [`MockPeer::delivered`] and
//! [`MockPeer::delivery_count`], and a [`Delivery`] proves its own authenticity
//! through [`Delivery::signature_verifies`] and [`Delivery::digest_matches`].
//!
//! [`wired_be::federation::signature::sign_request`]: wired_be::federation::signature::sign_request

#![cfg(test)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{
    HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header::CONTENT_TYPE, header::RETRY_AFTER,
};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use wired_be::{
    federation::signature::{
        SignedHeaders, SigningKey, sign_request,
        verify::{SignedRequestParts, parse_signature_header, verify, verify_digest},
    },
    models::user::{self, NewUser, User},
    state::AppState,
    util::keys::{KeyPair, generate_keypair},
};

use crate::common;

/// The `ActivityStreams` public address, as every mock document names it.
pub const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The path of this instance's shared `ActivityPub` inbox, which is also the
/// path the mock peer's own shared inbox answers on.
pub const INBOX: &str = "/inbox";
/// The media type an `ActivityPub` document is served with.
const AP_CONTENT_TYPE: &str = "application/activity+json";
/// The media type a `WebFinger` answer is served with.
const JRD_CONTENT_TYPE: &str = "application/jrd+json";
/// The `#main-key` fragment every actor's `keyId` ends with.
const KEY_SUFFIX: &str = "#main-key";

/// The mock peer's key pair, made once for the process.
///
/// RSA-2048 generation is slow in a debug build, and one identity for every
/// mock actor is enough for a peer that only ever signs its own activities.
fn peer_keys() -> &'static KeyPair {
    static KEYS: OnceLock<KeyPair> = OnceLock::new();
    KEYS.get_or_init(|| generate_keypair().expect("the mock peer's key pair generates"))
}

/// A key pair for *local* fixture accounts, made once for the process.
///
/// Deliberately not the [mock peer's](peer_keys): a delivery that verifies
/// against this public key was signed by this instance's own actor, not by the
/// peer. A local account needs a private key to federate at all — the delivery
/// job discards an activity its signing actor cannot sign — so a scenario that
/// expects a real `POST` out of this instance builds its local accounts with
/// [`keyed_local_user`].
///
/// # Panics
///
/// If the key pair cannot be generated, which it always can here.
pub fn local_keys() -> &'static KeyPair {
    static KEYS: OnceLock<KeyPair> = OnceLock::new();
    KEYS.get_or_init(|| generate_keypair().expect("the local fixture key pair generates"))
}

/// A local [`User`] named `nickname` as registration leaves it.
///
/// The account carries [`local_keys`] and the endpoint set a registered
/// account gets — `followers_url` and friends, which the publisher's follower
/// expansion reads — plus whatever else `customize` changes on top:
/// [`keyed_local_user`], or the same account locked, a bot, hidden, and still
/// able to federate.
///
/// # Panics
///
/// If the insert fails, e.g. because `nickname` is taken in this slot.
pub async fn keyed_local_user_with<F>(pool: &PgPool, nickname: &str, customize: F) -> User
where
    F: FnOnce(NewUser) -> NewUser,
{
    let keys = local_keys().clone();
    let shared_inbox = format!("{}/inbox", common::test_config().base_url);
    common::create_local_user_with(pool, nickname, move |user| {
        let ap_id = user.ap_id.clone();
        customize(NewUser {
            inbox: Some(user::inbox(&ap_id)),
            shared_inbox: Some(shared_inbox),
            outbox: Some(user::outbox(&ap_id)),
            followers_url: Some(user::followers(&ap_id)),
            following_url: Some(user::following(&ap_id)),
            featured_url: Some(user::featured(&ap_id)),
            public_key: Some(keys.public_pem),
            private_key: Some(keys.private_pem),
            ..user
        })
    })
    .await
}

/// A local [`User`] named `nickname` that can sign its own deliveries:
/// [`keyed_local_user_with`] with nothing else changed.
///
/// # Panics
///
/// If the insert fails, e.g. because `nickname` is taken in this slot.
pub async fn keyed_local_user(pool: &PgPool, nickname: &str) -> User {
    keyed_local_user_with(pool, nickname, std::convert::identity).await
}

/// The router and the [`AppState`] behind it over `pool`.
///
/// Both on the stock [`common::test_config`], whose `HTTP_ALLOW_PRIVATE_HOSTS`
/// is what lets the instance reach a mock on loopback: requests ride the
/// router, [`common::run_all_jobs`] drains the folds and deliveries on the
/// state, and a scenario that needs this instance's own signing actor reads it
/// through
/// [`wired_be::services::instance_actor::ensure_instance_actor`].
///
/// [`wired_be::services::instance_actor::ensure_instance_actor`]: wired_be::services::instance_actor::ensure_instance_actor
pub fn app_and_state(pool: PgPool) -> (Router, AppState) {
    let state = common::test_app_state(pool, common::test_config());
    (wired_be::app(state.clone()), state)
}

/// One inbox `POST` the mock recorded: where it arrived, the exact bytes and
/// headers it arrived with, and the parsed activity.
#[derive(Clone, Debug)]
pub struct Delivery {
    /// The request path the activity was posted to.
    path: String,
    /// The body exactly as it arrived, before parsing.
    body: Bytes,
    /// The parsed activity, [`Value::Null`] when the body was not JSON.
    activity: Value,
    /// The request headers, names lowercased.
    headers: Vec<(String, String)>,
}

impl Delivery {
    /// The path this delivery arrived at — the shared inbox or one actor's
    /// personal inbox, which is how a test tells the two endpoints apart.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The delivered activity.
    pub fn activity(&self) -> &Value {
        &self.activity
    }

    /// The value of request header `name`, or `None` when it was not sent.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_key, value)| value.as_str())
    }

    /// The `type` of the delivered activity, when it carries one.
    pub fn activity_type(&self) -> Option<&str> {
        self.activity.get("type").and_then(Value::as_str)
    }

    /// The `keyId` the `Signature` header names, when that header parses.
    pub fn key_id(&self) -> Option<String> {
        let value = self.header("signature")?;
        parse_signature_header(value)
            .ok()
            .map(|parsed| parsed.key_id)
    }

    /// The exact bytes the `POST` arrived with, before parsing. A retry
    /// test compares two deliveries with these to prove the same signed
    /// body went out again.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Whether the `Digest` header describes the bytes that arrived.
    pub fn digest_matches(&self) -> bool {
        verify_digest(&self.header_map(), &self.body).is_ok()
    }

    /// Whether the `Signature` header authenticates this `POST` against
    /// `public_pem`.
    ///
    /// The signature covers the `(request-target)`, `host`, `date` and
    /// `digest` lines as they arrived, so a true answer means the holder of
    /// that key sent these bytes to this path.
    pub fn signature_verifies(&self, public_pem: &str) -> bool {
        let Some(value) = self.header("signature") else {
            return false;
        };
        let Ok(parsed) = parse_signature_header(value) else {
            return false;
        };
        let request = SignedRequestParts {
            method: "POST",
            path_and_query: &self.path,
            headers: self.header_map(),
        };
        verify(&parsed, &request, public_pem, Utc::now()).is_ok()
    }

    /// The captured headers as the [`HeaderMap`] the verifiers take.
    fn header_map(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.headers {
            let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value))
            else {
                continue;
            };
            let _previous: Option<HeaderValue> = headers.insert(name, value);
        }
        headers
    }
}

/// One answer a test asked the inboxes to give before falling back to the
/// plain `202`: the status, an optional `Retry-After` delta in seconds, and
/// how many further `POST`s it still stands for.
#[derive(Clone, Debug)]
struct Scripted {
    /// The status to answer with.
    status: StatusCode,
    /// Seconds to name in the `Retry-After` header, when the answer asks
    /// for a scheduled retry.
    retry_after: Option<u64>,
    /// How many more `POST`s get this answer; the queue entry is spent
    /// when it reaches zero.
    remaining: usize,
}

/// The mock's shared half: its origin, the deliveries it recorded, the
/// scripted answers its inboxes still owe and the key pair its documents
/// currently publish.
#[derive(Clone, Debug)]
struct Peer {
    /// The `http://{addr}` origin this peer answers on.
    base: String,
    /// The inbox `POST`s it recorded, oldest first.
    posts: Arc<Mutex<Vec<Delivery>>>,
    /// The scripted answers, oldest first; an empty queue means `202`.
    script: Arc<Mutex<VecDeque<Scripted>>>,
    /// The key pair actor documents name as their `publicKeyPem`, swappable
    /// under [`MockPeer::rotate_keys`] while the capture keeps recording.
    key: Arc<Mutex<Arc<KeyPair>>>,
}

/// The running mock peer; dropping it stops the server.
#[derive(Debug)]
pub struct MockPeer {
    /// The handlers' shared state.
    peer: Peer,
    /// The task serving it, aborted on drop.
    server: JoinHandle<()>,
}

/// The `keyId` of the actor `actor`: its id with the conventional Mastodon key
/// fragment.
fn main_key(actor: &str) -> String {
    format!("{actor}{KEY_SUFFIX}")
}

impl MockPeer {
    /// Binds a loopback port and serves the whole peer surface.
    ///
    /// # Panics
    ///
    /// If no loopback port can be bound, which tests never provoke.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock peer binds a port");
        let address = listener.local_addr().expect("a bound address");
        let peer = Peer {
            base: format!("http://{address}"),
            posts: Arc::new(Mutex::new(Vec::new())),
            script: Arc::new(Mutex::new(VecDeque::new())),
            key: Arc::new(Mutex::new(Arc::new(peer_keys().clone()))),
        };
        let app = Router::new()
            .route("/.well-known/webfinger", get(webfinger))
            .route("/relay", get(relay_actor))
            .route("/objects/{name}", get(note))
            .route("/users/{nick}", get(person_actor))
            .route("/users/{nick}/inbox", post(capture))
            .route(INBOX, post(capture))
            .with_state(peer.clone());
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock peer stopped: {err}");
            }
        });
        Self { peer, server }
    }

    /// The peer's `host:port` authority: the domain half of every handle it
    /// serves, since decision 7 spells a remote account `user@host`.
    #[must_use]
    pub fn host(&self) -> String {
        self.peer
            .base
            .split_once("://")
            .map_or_else(String::new, |(_scheme, rest)| rest.to_owned())
    }

    /// The peer's full `http://{addr}` origin: the base every id it serves
    /// hangs off, which a test needs when it builds a URL the peer does
    /// *not* serve — a key id whose actor no lookup can settle.
    #[must_use]
    pub fn origin(&self) -> String {
        self.peer.base.clone()
    }

    /// The actor id of this peer's account `nick`.
    #[must_use]
    pub fn actor(&self, nick: &str) -> String {
        format!("{}/users/{nick}", self.peer.base)
    }

    /// The `acct:` handle of this peer's account `nick`, as a client of this
    /// instance names it.
    #[must_use]
    pub fn handle(&self, nick: &str) -> String {
        format!("{nick}@{}", self.host())
    }

    /// The request path of the inbox belonging to the peer actor `actor`.
    ///
    /// The capture records where a delivery arrived, so a test compares
    /// against this rather than against the absolute URL the actor document
    /// names.
    #[must_use]
    pub fn inbox_path(&self, actor: &str) -> String {
        actor
            .strip_prefix(&self.peer.base)
            .map_or_else(|| INBOX.to_owned(), |tail| format!("{tail}/inbox"))
    }

    /// The peer-wide shared inbox, which is also the relay's inbox.
    #[must_use]
    pub fn shared_inbox(&self) -> String {
        format!("{}{INBOX}", self.peer.base)
    }

    /// The relay actor's id: a `Service` served at `/relay`.
    #[must_use]
    pub fn relay(&self) -> String {
        format!("{}/relay", self.peer.base)
    }

    /// The object id of the public `Note` this peer serves as `name`.
    #[must_use]
    pub fn note(&self, name: &str) -> String {
        format!("{}/objects/{name}", self.peer.base)
    }

    /// An activity id on this peer's host, which is what the dispatcher's
    /// containment check asks of a peer's own activity.
    #[must_use]
    pub fn activity_id(&self, name: &str) -> String {
        format!("{}/activities/{name}", self.peer.base)
    }

    /// An activity of `kind` by the peer's actor `actor`, named `name` on this
    /// peer's host, whose `object` is `object` — an id, or an embedded
    /// document.
    #[must_use]
    pub fn activity(&self, kind: &str, name: &str, actor: &str, object: &Value) -> Value {
        json!({
            "id": self.activity_id(name),
            "type": kind,
            "actor": actor,
            "object": object,
        })
    }

    /// A `Follow` by the peer's actor `follower` of the account `target`.
    #[must_use]
    pub fn follow_of(&self, name: &str, follower: &str, target: &str) -> Value {
        self.activity("Follow", name, follower, &json!(target))
    }

    /// Script the next `times` inbox `POST`s — on either endpoint, in
    /// arrival order — to be recorded as always but answered `status`
    /// instead of `202`, with a `Retry-After: {secs}` header when
    /// `retry_after` names a delay. This is how a scenario points the
    /// mock's inbox at a `500` a delivery must ride out, a `503` whose
    /// `Retry-After` the worker must honour, or a `401` that reads like a
    /// signature the peer can no longer verify.
    ///
    /// # Panics
    ///
    /// If the script lock is poisoned, which only a panicking handler
    /// leaves behind.
    pub fn script_inbox(&self, status: StatusCode, retry_after: Option<Duration>, times: usize) {
        let retry_after = retry_after.map(|delay| delay.as_secs());
        self.peer
            .script
            .lock()
            .expect("the script lock")
            .push_back(Scripted {
                status,
                retry_after,
                remaining: times,
            });
    }

    /// Script every later inbox `POST` to answer `status` (with
    /// `Retry-After` when asked): the closed address that never reopens,
    /// the peer that stays convinced a signature is bad.
    pub fn script_inbox_permanently(&self, status: StatusCode, retry_after: Option<Duration>) {
        self.script_inbox(status, retry_after, usize::MAX);
    }

    /// Generate and serve a fresh key pair from every actor document now —
    /// the peer mid-rotation. The pair [`deliver`] and the signing helpers
    /// use does not change: what this instance already mirrored of a mock
    /// actor still vouches for the old key, which is exactly the skew a
    /// rotation scenario reads back.
    ///
    /// # Panics
    ///
    /// If the new key pair cannot be generated, which it always can here.
    ///
    /// [`deliver`]: Self::deliver
    pub fn rotate_keys(&self) {
        let next = Arc::new(generate_keypair().expect("the mock peer rotates its key pair"));
        let mut served = self.peer.key.lock().expect("the key lock");
        *served = next;
    }

    /// The `publicKeyPem` the peer's actor documents carry right now — the
    /// rotated one after [`rotate_keys`](Self::rotate_keys), the process
    /// peer key before it. A test compares this against what a stored
    /// mirror still holds to show which side moved.
    ///
    /// # Panics
    ///
    /// If the key lock is poisoned, which only a panicking handler leaves
    /// behind.
    #[must_use]
    pub fn served_public_key(&self) -> String {
        self.peer
            .key
            .lock()
            .expect("the key lock")
            .public_pem
            .clone()
    }

    /// The four auth headers of a signed `POST` of `body` to this instance at
    /// `path`, presented as the peer's actor `actor` under [`main_key`].
    ///
    /// The peer signs every one of its actors with its single process-wide
    /// [key pair](peer_keys), so this needs no instance.
    ///
    /// # Panics
    ///
    /// If the URL cannot be signed, which the test instance's own origin never
    /// is.
    #[must_use]
    pub fn signed_headers(actor: &str, path: &str, body: &[u8]) -> SignedHeaders {
        let key_id = main_key(actor);
        sign_request(
            "POST",
            &format!("{}{path}", common::test_config().base_url),
            Some(body),
            &SigningKey {
                key_id: &key_id,
                private_pem: &peer_keys().private_pem,
            },
            Utc::now(),
        )
        .expect("the mock peer signs its own delivery")
    }

    /// The four auth headers of a signed `GET` of this instance at `path`,
    /// presented as the peer's actor `actor` under [`main_key`] — the
    /// proof an `AUTHORIZED_FETCH` gate reads, over the same origin
    /// [`signed_headers`] signs deliveries to.
    ///
    /// # Panics
    ///
    /// If the URL cannot be signed, which the test instance's own origin
    /// never is.
    #[must_use]
    pub fn signed_get(actor: &str, path: &str) -> SignedHeaders {
        let key_id = main_key(actor);
        sign_request(
            "GET",
            &format!("{}{path}", common::test_config().base_url),
            None,
            &SigningKey {
                key_id: &key_id,
                private_pem: &peer_keys().private_pem,
            },
            Utc::now(),
        )
        .expect("the mock peer signs its own fetch")
    }

    /// The public half of the peer's key pair: the `publicKeyPem` every
    /// mock actor publishes. A test that must let the peer deliver while
    /// the instance may not fetch its origin — a seeded mirror behind a
    /// closed SSRF guard — stores exactly this PEM.
    #[must_use]
    pub fn public_key_pem() -> &'static str {
        &peer_keys().public_pem
    }

    /// `POST`s `activity` to `path` on this instance as the peer's actor
    /// `actor`, signed.
    ///
    /// The answer comes back for the test to read: `202 Accepted` is the
    /// inbox's yes, and every refusal of the auth gate arrives as its own
    /// status.
    ///
    /// # Panics
    ///
    /// If `activity` does not serialize, which the fixtures never do.
    pub async fn deliver(
        &self,
        app: &Router,
        actor: &str,
        path: &str,
        activity: &Value,
    ) -> common::TestResponse {
        let body = serde_json::to_vec(activity).expect("the mock activity serializes");
        let wire = Self::signed_headers(actor, path, &body);
        let digest = wire.digest.as_deref().expect("a signed post digests");
        common::TestRequest::post(path)
            .json(activity)
            .header("host", &wire.host)
            .header("date", &wire.date)
            .header("digest", digest)
            .header("signature", &wire.signature)
            .send(app)
            .await
    }

    /// Every delivery this peer's inboxes recorded, oldest first.
    ///
    /// # Panics
    ///
    /// If the capture lock is poisoned, which only a panicking handler
    /// leaves behind.
    #[must_use]
    pub fn received(&self) -> Vec<Delivery> {
        self.peer.posts.lock().expect("the capture lock").clone()
    }

    /// The recorded deliveries whose activity names `kind` as its `type`,
    /// oldest first: the capture narrowed to the one activity a scenario
    /// asserts on.
    #[must_use]
    pub fn delivered(&self, kind: &str) -> Vec<Delivery> {
        self.received()
            .into_iter()
            .filter(|post| post.activity_type() == Some(kind))
            .collect()
    }

    /// How many deliveries this peer has recorded, which is how a test says
    /// "and nothing else left this instance".
    #[must_use]
    pub fn delivery_count(&self) -> usize {
        self.received().len()
    }
}

impl Drop for MockPeer {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// The actor document every mock account wears: self-identifying, its inbox
/// under its own id, the peer-wide inbox as its `endpoints.sharedInbox`, and
/// the real public key its signatures verify against — the key pair
/// [`rotate_keys`] has moved the peer to, whichever that is now.
///
/// [`rotate_keys`]: MockPeer::rotate_keys
fn actor_doc(peer: &Peer, id: &str, kind: &str, preferred: &str) -> Value {
    let public_pem = peer.key.lock().expect("the key lock").public_pem.clone();
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": kind,
        "id": id,
        "preferredUsername": preferred,
        "name": preferred,
        "summary": "an account on the mock federation peer",
        "inbox": format!("{id}/inbox"),
        "outbox": format!("{id}/outbox"),
        "followers": format!("{id}/followers"),
        "following": format!("{id}/following"),
        "endpoints": { "sharedInbox": format!("{}{INBOX}", peer.base) },
        "manuallyApprovesFollowers": false,
        "discoverable": true,
        "publicKey": {
            "id": main_key(id),
            "owner": id,
            "publicKeyPem": public_pem,
        },
    })
}

/// Answers `document` with the media type `mime`.
fn document_response(document: &Value, mime: &'static str) -> Response {
    (
        [(CONTENT_TYPE, HeaderValue::from_static(mime))],
        document.to_string(),
    )
        .into_response()
}

/// `GET /users/{nick}`: one of the peer's `Person` accounts.
async fn person_actor(State(peer): State<Peer>, Path(nick): Path<String>) -> Response {
    let id = format!("{}/users/{nick}", peer.base);
    document_response(&actor_doc(&peer, &id, "Person", &nick), AP_CONTENT_TYPE)
}

/// `GET /relay`: the peer's relay, a `Service` whose inbox is the shared one.
async fn relay_actor(State(peer): State<Peer>) -> Response {
    let id = format!("{}/relay", peer.base);
    let mut document = actor_doc(&peer, &id, "Service", "relay");
    document["inbox"] = json!(format!("{}{INBOX}", peer.base));
    document_response(&document, AP_CONTENT_TYPE)
}

/// `GET /objects/{name}`: a public `Note` by the peer's own `author`.
async fn note(State(peer): State<Peer>, Path(name): Path<String>) -> Response {
    let id = format!("{}/objects/{name}", peer.base);
    document_response(
        &json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": id,
            "type": "Note",
            "attributedTo": format!("{}/users/author", peer.base),
            "to": [PUBLIC],
            "content": format!("<p>a note relayed as {name}</p>"),
            "published": "2026-11-02T00:00:00Z",
        }),
        AP_CONTENT_TYPE,
    )
}

/// `GET /.well-known/webfinger`: a JRD echoing the asked `acct:` resource and
/// naming that nickname's actor on this peer.
async fn webfinger(
    State(peer): State<Peer>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(resource) = params
        .get("resource")
        .and_then(|value| value.strip_prefix("acct:"))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let nickname = resource.split('@').next().unwrap_or(resource);
    document_response(
        &json!({
            "subject": format!("acct:{resource}"),
            "links": [{
                "rel": "self",
                "type": AP_CONTENT_TYPE,
                "href": format!("{}/users/{nickname}", peer.base),
            }],
        }),
        JRD_CONTENT_TYPE,
    )
}

/// Both inboxes: record the path, headers and bytes as they arrived, and
/// answer the next scripted status when the test queued one (with its
/// `Retry-After` when the script carries one), the plain `202 Accepted`
/// when nothing was scripted.
async fn capture(State(peer): State<Peer>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let delivery = Delivery {
        path: uri.path().to_owned(),
        activity: serde_json::from_slice(&body).unwrap_or(Value::Null),
        headers: headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect(),
        body,
    };
    peer.posts.lock().expect("the capture lock").push(delivery);
    next_scripted(&peer.script).map_or_else(
        || StatusCode::ACCEPTED.into_response(),
        |scripted| answer_with(scripted.status, scripted.retry_after),
    )
}

/// The response for a scripted answer: its status, with a `Retry-After`
/// delta-seconds header when the script carries one.
fn answer_with(status: StatusCode, retry_after: Option<u64>) -> Response {
    let header = retry_after.and_then(|seconds| HeaderValue::try_from(seconds.to_string()).ok());
    header.map_or_else(
        || status.into_response(),
        |value| (status, [(RETRY_AFTER, value)]).into_response(),
    )
}

/// The next scripted answer, spent down: the queue's head answers this
/// `POST` and loses one of its remaining hits, and an exhausted entry is
/// off the queue for the next one. `None` — the plain `202` — when nothing
/// stands scripted.
///
/// # Panics
///
/// If the script lock is poisoned, which only a panicking handler leaves
/// behind.
fn next_scripted(script: &Arc<Mutex<VecDeque<Scripted>>>) -> Option<Scripted> {
    let scripted = {
        let mut queue = script.lock().expect("the script lock");
        let mut next = queue.front().cloned()?;
        next.remaining = next.remaining.saturating_sub(1);
        if next.remaining == 0 {
            let _spent: Option<Scripted> = queue.pop_front();
        }
        next
    };
    Some(scripted)
}
