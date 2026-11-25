//! The mock peer's shared state, constants and imports. Its parts are
//! children, so they reach every private item through `use super::*`.

mod delivery;
mod keys;
mod peer;
mod routes;

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
    models::{
        activitypub_actor::ActivityPubActorWrite,
        user::{NewUser, User},
    },
    state::AppState,
    util::keys::{KeyPair, generate_keypair},
};

use self::{keys::peer_keys, routes::router};
use crate::common;

pub use self::keys::{keyed_instance_actor, keyed_local_user, keyed_local_user_with, local_keys};

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
/// The `@context` every mock `ActivityPub` document names.
const AS_CONTEXT: &str = "https://www.w3.org/ns/activitystreams";
/// The `@context` of the standalone [`Key`](MockPeer::key_url) document, the
/// `security` vocabulary a `publicKeyPem` belongs to.
const SECURITY_CONTEXT: &str = "https://w3id.org/security/v1";
/// The actor that owns the `/key` mirror: a nickname the peer serves an actor
/// document for, like every other mock account, so a `keyId` naming the key
/// settles on a fetchable owner.
pub const KEY_OWNER: &str = "alice";
/// The activity ids one page of a mock outbox carries: small enough that a
/// test with three published activities walks two pages.
const OUTBOX_PAGE: usize = 2;
/// The `published` of a mock note: a fixed past instant, so no clock-skew
/// clamp ever moves it.
const DEFAULT_PUBLISHED: &str = "2026-11-02T00:00:00Z";

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
/// scripted answers its inboxes still owe, the key pair its documents
/// currently publish, the notes it serves by name and the activity ids it
/// has published to an actor's outbox.
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
    /// The shaped documents `GET /objects/{name}` answers by `name`, in
    /// place of the canned public note.
    notes: NoteShelf,
    /// The activity ids each actor's outbox lists, by nickname, oldest
    /// first — paged by `GET /users/{nick}/outbox`.
    outbox: OutboxIds,
}

/// The mock's outbox listings: actor nickname to the activity ids it publishes,
/// behind the lock the handler and [`MockPeer::publish`] share.
type OutboxIds = Arc<Mutex<HashMap<String, Vec<String>>>>;

/// The shaped notes the mock serves by object name: `GET /objects/{name}`
/// answers what a test put here, and its canned note otherwise.
type NoteShelf = Arc<Mutex<HashMap<String, Value>>>;

impl Peer {
    /// The activity ids published to the actor `nick`'s outbox, oldest
    /// first; empty when nothing was published there.
    ///
    /// # Panics
    ///
    /// If the outbox lock is poisoned, which only a panicking handler leaves
    /// behind.
    fn listed(&self, nick: &str) -> Vec<String> {
        self.outbox
            .lock()
            .expect("the outbox lock")
            .get(nick)
            .cloned()
            .unwrap_or_default()
    }

    /// The `publicKeyPem` this peer's documents carry right now.
    ///
    /// # Panics
    ///
    /// If the key lock is poisoned, which only a panicking handler leaves
    /// behind.
    fn public_pem(&self) -> String {
        self.key.lock().expect("the key lock").public_pem.clone()
    }
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
