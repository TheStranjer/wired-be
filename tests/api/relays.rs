//! Relays over federation (WBE-186): the instance actor's follow of a
//! remote relay, the publisher's fan-out to accepted relays, the relay's
//! `Announce` fold, and the relay actor's own endpoints.
//!
//! Everything runs against a loopback mock of a relay instance: its
//! actor document at `/relay` (plus a `Service`, a `Person` and a `Group`
//! for the type gate), an inbox that records every delivery, and the
//! `Note` a relay re-broadcast is fetched from — the stock test config
//! lets the instance's own HTTP client reach loopback. What this pins,
//! per the ticket:
//!
//! - [`follow_relay`] fetches the relay actor, writes the instance
//!   actor's `follows` row with its minted activity id and the `relays`
//!   row `pending`, and sends the `Follow` — `to` the relay's inbox,
//!   `object` its actor id — as one signed delivery; a restated follow
//!   keeps one row and one queued send; the relay's `Accept`, folded by
//!   the existing handler with nobody following the relay, turns both
//!   rows accepted;
//! - the publisher adds the accepted relay's inbox to the fan-out of
//!   public `Create`, `Announce`, `Update` and `Delete` activities and of
//!   the actor's own `Update`, and nothing at all while the subscription
//!   is `pending` or the activity addresses nobody public;
//! - an `Announce` from an accepted relay folds by fetching its object
//!   through `resolve_status` and ingesting it as an ordinary public
//!   remote status, and creates no reblog row; a non-relay actor's
//!   `Announce` of the same object still boosts, and a `pending` relay is
//!   no relay yet;
//! - [`unfollow_relay`] sends the `Undo` of that very `Follow`, deletes
//!   the `follows` row and the `relays` row, and takes the relay out of
//!   the fan-out;
//! - `GET /relay` answers the instance actor document,
//!   `GET /relay/following` and `GET /relay/followers` answer empty
//!   `OrderedCollection`s, and `POST /relay/inbox` is the shared inbox
//!   handler — all with the usual `Accept` negotiation.
//!
//! The admin endpoints that call these functions are the admin tickets'
//! range; here they are exercised directly on the [`AppState`].
//!
//! [`follow_relay`]: wired_be::services::relay::follow_relay
//! [`unfollow_relay`]: wired_be::services::relay::unfollow_relay

#![cfg(test)]

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use wired_be::{
    error::AppError,
    federation::{
        delivery::KIND_DELIVER,
        dispatch::{KIND_INCOMING, handle_incoming},
        publisher::{self, PublishOpts},
        signature::{SigningKey, sign_request},
    },
    models::{
        follow::{self, Follow, FollowState},
        instance, relay,
        relay::RelayState,
        status::{self, Visibility},
        user::{User, get_by_ap_id},
    },
    services::{
        instance_actor::{INSTANCE_ACTOR_PATH, ensure_instance_actor},
        relay::{follow_relay, list_relays, unfollow_relay},
    },
    state::AppState,
};

use crate::common;

/// The instance origin of [`common::test_config`].
const BASE: &str = "https://test.example";
/// The public address.
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The media type of an `ActivityPub` document, charset included.
const ACTIVITY_JSON: &str = "application/activity+json; charset=utf-8";

// --- the mock relay ----------------------------------------------------------------

/// The mock's shared state: its own origin and the activities its inbox
/// recorded.
#[derive(Clone, Debug)]
struct Mock {
    /// The `http://{addr}` origin this relay answers on.
    base: String,
    /// The inbox `POST` bodies it recorded, oldest first.
    posts: Arc<Mutex<Vec<Value>>>,
}

/// The running mock relay; dropping it stops the server.
#[derive(Debug)]
struct MockRelay {
    /// The handlers' shared state.
    mock: Mock,
    /// The task serving it, aborted on drop.
    server: JoinHandle<()>,
}

impl MockRelay {
    /// Binds a loopback port and serves actors, one note and a capturing
    /// inbox.
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock relay binds a port");
        let base = format!("http://{}", listener.local_addr().expect("a bound address"));
        let mock = Mock {
            base,
            posts: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route("/relay", get(relay_actor))
            .route("/service", get(service_actor))
            .route("/person", get(person_actor))
            .route("/group", get(group_actor))
            .route("/missing", get(|| async { StatusCode::NOT_FOUND }))
            .route("/relay/inbox", post(capture))
            .route("/objects/{name}", get(note))
            .route("/users/{name}", get(named_actor))
            .with_state(mock.clone());
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock relay stopped: {err}");
            }
        });
        Self { mock, server }
    }

    /// The relay actor's `ActivityPub` id on this mock.
    fn actor_ap_id(&self) -> String {
        format!("{}/relay", self.mock.base)
    }

    /// The relay actor's inbox URL on this mock.
    fn inbox_url(&self) -> String {
        format!("{}/relay/inbox", self.mock.base)
    }

    /// The object id of the `Note` this mock serves as `name`.
    fn object_uri(&self, name: &str) -> String {
        format!("{}/objects/{name}", self.mock.base)
    }

    /// An activity id this mock's relay actor can carry without failing
    /// the dispatcher's host containment.
    fn activity_uri(&self, name: &str) -> String {
        format!("{}/activities/{name}", self.mock.base)
    }

    /// The bodies the inbox recorded, oldest first.
    fn received(&self) -> Vec<Value> {
        self.mock.posts.lock().expect("the capture lock").clone()
    }
}

impl Drop for MockRelay {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// The `GET /relay` fixture: the `Application` actor the follow tests are
/// about, its inbox at `/relay/inbox`.
async fn relay_actor(State(mock): State<Mock>) -> Json<Value> {
    actor_doc(&mock, "relay", "relay", "Application")
}

/// A relay served as a `Service`, the kind Pleroma's own relay actor
/// wears.
async fn service_actor(State(mock): State<Mock>) -> Json<Value> {
    actor_doc(&mock, "service", "relay", "Service")
}

/// A hand-run relay: a plain `Person`.
async fn person_actor(State(mock): State<Mock>) -> Json<Value> {
    actor_doc(&mock, "person", "someone", "Person")
}

/// Not a relay: a `Group` is outside the ticket's three kinds.
async fn group_actor(State(mock): State<Mock>) -> Json<Value> {
    actor_doc(&mock, "group", "gang", "Group")
}

/// The note's author, and every other name-addressed account on this
/// mock.
async fn named_actor(State(mock): State<Mock>, Path(name): Path<String>) -> Json<Value> {
    actor_doc(&mock, &format!("users/{name}"), &name, "Person")
}

/// A self-naming actor document of `kind` at `{base}/{path}`, with an
/// inbox under its own id and the inert public key the fetch fixtures
/// store.
fn actor_doc(mock: &Mock, path: &str, preferred: &str, kind: &str) -> Json<Value> {
    let id = format!("{}/{path}", mock.base);
    Json(json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": kind,
        "id": id,
        "preferredUsername": preferred,
        "name": preferred,
        "inbox": format!("{id}/inbox"),
        "followers": format!("{id}/followers"),
        "publicKey": {
            "id": format!("{id}#main-key"),
            "owner": id,
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
        },
    }))
}

/// Records the delivered activity and accepts it.
async fn capture(State(mock): State<Mock>, body: Bytes) -> StatusCode {
    let parsed = serde_json::from_slice(&body).unwrap_or(Value::Null);
    mock.posts.lock().expect("the capture lock").push(parsed);
    StatusCode::ACCEPTED
}

/// The public note a relay re-broadcasts: authored by this mock's own
/// account, addressed to the world.
async fn note(State(mock): State<Mock>, Path(name): Path<String>) -> Json<Value> {
    let id = format!("{}/objects/{name}", mock.base);
    Json(json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": id,
        "type": "Note",
        "attributedTo": format!("{}/users/author", mock.base),
        "to": [PUBLIC],
        "content": "<p>a relayed note</p>",
        "published": "2026-11-02T00:00:00Z",
    }))
}

// --- the fixtures and reads ---------------------------------------------------------

/// The dispatcher state over the stock test config — whose
/// `HTTP_ALLOW_PRIVATE_HOSTS` lets the resolver's client reach the mock.
fn relay_state(pool: &PgPool) -> AppState {
    common::test_app_state(pool.clone(), common::test_config())
}

/// The stored mirror of the mock's relay actor.
async fn relay_mirror(state: &AppState, mock: &MockRelay) -> User {
    get_by_ap_id(&state.db, &mock.actor_ap_id())
        .await
        .expect("the mirror reads")
        .expect("the follow fetched the relay actor")
}

/// The instance actor's relationship row with `mirror`, when one stands.
async fn instance_follow(state: &AppState, mirror: &User) -> Option<Follow> {
    let instance = ensure_instance_actor(&state.db, &state.config)
        .await
        .expect("the instance actor stands");
    follow::get(&state.db, instance.id, mirror.id)
        .await
        .expect("the follow reads")
}

/// Runs the mock relay's `Accept` of `follow_id` through the real
/// dispatcher, as a peer named by its stored mirror.
async fn answer_accept(state: &AppState, mock: &MockRelay, relay_user: &User, follow_id: &str) {
    let accept = json!({
        "id": mock.activity_uri("accept"),
        "type": "Accept",
        "actor": mock.actor_ap_id(),
        "object": follow_id,
    });
    handle_incoming(
        state.clone(),
        json!({"activity": accept, "signer_id": relay_user.id.to_string(), "inbox_owner": null}),
    )
    .await
    .expect("the relay's Accept completes the job");
}

/// Follows the mock's `Application` relay and drives its `Accept` through
/// the dispatcher: the subscription stands accepted when this returns,
/// with the follow row and the relay's stored mirror.
async fn followed_and_accepted(state: &AppState, mock: &MockRelay) -> (Follow, User) {
    let subscription = follow_relay(state, &mock.actor_ap_id())
        .await
        .expect("the relay follow runs");
    assert_eq!(
        subscription.state,
        RelayState::Pending,
        "a fresh subscription waits for its answer"
    );
    let relay_user = relay_mirror(state, mock).await;
    let row = instance_follow(state, &relay_user)
        .await
        .expect("the instance actor now follows the relay");
    let follow_id = row
        .ap_id
        .clone()
        .expect("the instance actor's follow mints its id");
    answer_accept(state, mock, &relay_user, &follow_id).await;
    let granted = instance_follow(state, &relay_user)
        .await
        .expect("the pair stands after the grant");
    (granted, relay_user)
}

/// Delivers `activity` through the real dispatcher as `signer`.
async fn deliver(state: &AppState, activity: &Value, signer: &User) {
    let kind = activity["type"].as_str().unwrap_or("activity");
    handle_incoming(
        state.clone(),
        json!({"activity": activity, "signer_id": signer.id.to_string(), "inbox_owner": null}),
    )
    .await
    .unwrap_or_else(|err| panic!("the {kind} delivery completes the job: {err}"));
}

/// The args of every queued `federator.deliver` job, in insertion order.
async fn delivery_jobs(pool: &PgPool) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>("SELECT args FROM jobs WHERE kind = $1 ORDER BY inserted_at, id")
        .bind(KIND_DELIVER)
        .fetch_all(pool)
        .await
        .expect("the delivery jobs read")
        .into_iter()
        .map(|(args,)| args)
        .collect()
}

/// The `relays` row for `actor_ap_id`, insisting it stands.
async fn standing_relay(pool: &PgPool, actor_ap_id: &str) -> relay::Relay {
    relay::get(pool, actor_ap_id)
        .await
        .expect("the relay reads")
        .expect("the subscription stands")
}

// --- the handshake -------------------------------------------------------------------

/// Following a relay writes the pair of rows and sends the `Follow` to
/// the relay's inbox — once, however often the follow is restated while
/// the send is still queued; the worker's POST lands at the mock; and the
/// relay's own `Accept` — folded by the existing handler, nobody having
/// to follow the relay for it — grants both the `follows` row and the
/// subscription.
#[tokio::test]
async fn following_a_relay_runs_the_handshake_to_accepted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let mock = MockRelay::start().await;

    let subscription = follow_relay(&state, &mock.actor_ap_id())
        .await
        .expect("the relay follow runs");
    assert_eq!(
        subscription.actor_ap_id,
        mock.actor_ap_id(),
        "the subscription is keyed by the relay actor id"
    );
    assert_eq!(
        subscription.inbox,
        mock.inbox_url(),
        "and carries the inbox the fetched document named"
    );
    assert_eq!(subscription.state, RelayState::Pending, "pending first");

    let relay_user = relay_mirror(&state, &mock).await;
    assert_eq!(
        relay_user.actor_type, "Application",
        "the fetched actor is mirrored as the Application it is"
    );
    let row = instance_follow(&state, &relay_user)
        .await
        .expect("the instance actor now follows the relay");
    assert_eq!(row.state, FollowState::Pending, "the remote follow waits");
    let follow_id = row.ap_id.clone().expect("the row minted its id");
    assert!(
        follow_id.starts_with(&format!("{BASE}/activities/")),
        "the id is decision 6's local activity address: {follow_id}"
    );
    assert_queued_follow(&pool, &mock, &follow_id).await;

    let ran = common::run_all_jobs(&state).await;
    assert!(ran >= 1, "the delivery job ran");
    let posts = mock.received();
    assert_eq!(posts.len(), 1, "the relay's inbox recorded the one Follow");
    assert_eq!(posts[0]["id"], json!(follow_id), "with its activity id");

    answer_accept(&state, &mock, &relay_user, &follow_id).await;
    let granted = instance_follow(&state, &relay_user)
        .await
        .expect("the pair stands after the grant");
    assert_eq!(
        granted.state,
        FollowState::Accepted,
        "the follow stands granted"
    );
    assert_eq!(
        standing_relay(&pool, &mock.actor_ap_id()).await.state,
        RelayState::Accepted,
        "and so does the subscription"
    );
}

/// The one queued `federator.deliver` job carrying the instance actor's
/// `Follow` of the mock: the row's stored id, the instance actor as its
/// maker, the relay actor as its object and the relay's inbox as its only
/// address and its only delivery target.
async fn assert_queued_follow(pool: &PgPool, mock: &MockRelay, follow_id: &str) {
    let jobs = delivery_jobs(pool).await;
    assert_eq!(jobs.len(), 1, "the Follow travels as one delivery");
    assert_eq!(jobs[0]["inbox"], mock.inbox_url(), "to the relay's inbox");
    let body = &jobs[0]["body"];
    assert_eq!(body["type"], "Follow", "the activity");
    assert_eq!(body["id"], json!(follow_id), "under the row's stored id");
    assert_eq!(
        body["actor"],
        format!("{BASE}{INSTANCE_ACTOR_PATH}"),
        "signed by the instance actor"
    );
    assert_eq!(body["object"], mock.actor_ap_id(), "naming the relay actor");
    assert_eq!(
        body["to"],
        json!([mock.inbox_url()]),
        "addressed straight to the relay's inbox"
    );
}

/// A restated follow while the send is still queued: one subscription,
/// the same pair under the same activity id, and the queue's per-activity
/// dedup swallows the repeat.
#[tokio::test]
async fn a_restated_relay_follow_sends_nothing_twice() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let mock = MockRelay::start().await;

    let _subscription = follow_relay(&state, &mock.actor_ap_id())
        .await
        .expect("the relay follow runs");
    let relay_user = relay_mirror(&state, &mock).await;
    let row = instance_follow(&state, &relay_user)
        .await
        .expect("the instance actor now follows the relay");
    let follow_id = row.ap_id.clone().expect("the row minted its id");

    let restated = follow_relay(&state, &mock.actor_ap_id())
        .await
        .expect("re-following a relay is no error");
    assert_eq!(
        restated.state,
        RelayState::Pending,
        "the restatement does not move the state"
    );
    let again = instance_follow(&state, &relay_user)
        .await
        .expect("the pair stands");
    assert_eq!(again.id, row.id, "the same follows row");
    assert_eq!(again.ap_id, Some(follow_id), "the same activity id");
    let again_id = again.ap_id.expect("just compared equal to Some");
    assert_queued_follow(&pool, &mock, &again_id).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM relays")
            .fetch_one(&pool)
            .await
            .expect("the ledger reads"),
        1,
        "and still one subscription"
    );
}

/// A `Service` and a `Person` are followable relay shapes; a `Group` is
/// refused, and so is an address with no actor behind it or of no shape
/// at all.
#[tokio::test]
async fn only_relay_shapes_are_followable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let mock = MockRelay::start().await;

    for path in ["/service", "/person"] {
        let url = format!("{}{path}", mock.mock.base);
        let subscription = follow_relay(&state, &url)
            .await
            .unwrap_or_else(|err| panic!("following `{path}` runs: {err}"));
        assert_eq!(
            subscription.actor_ap_id, url,
            "the subscription names the actor that was fetched"
        );
    }

    let group = follow_relay(&state, &format!("{}/group", mock.mock.base)).await;
    assert!(
        matches!(group, Err(AppError::Unprocessable(_))),
        "a Group actor is no relay: {group:?}"
    );
    let gone = follow_relay(&state, &format!("{}/missing", mock.mock.base)).await;
    assert!(
        matches!(gone, Err(AppError::NotFound)),
        "an actor that answers 404 is refused: {gone:?}"
    );
    let nonsense = follow_relay(&state, "not a host/ at all").await;
    assert!(
        matches!(nonsense, Err(AppError::Unprocessable(_))),
        "an address that is neither URL nor host is refused: {nonsense:?}"
    );
}

// --- the publisher's fan-out ----------------------------------------------------------

/// While the subscription is `pending`, no activity fans out to it; once
/// the relay's `Accept` folds, its inbox joins the fan-out of every
/// public `Create`, `Announce`, `Update` and `Delete` activity.
#[tokio::test]
async fn public_activities_reach_the_accepted_relay_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let mock = MockRelay::start().await;

    let alice = common::create_local_user(&pool, "alice").await;
    let activity = |n: u32, kind: &str, to: Value| {
        json!({
            "id": format!("{BASE}/activities/relay-fanout-{n}"),
            "type": kind,
            "actor": alice.ap_id,
            "object": format!("{BASE}/objects/fanout-{n}"),
            "to": to,
        })
    };

    // Pending: the subscription is not a target yet.
    let _subscription = relay::upsert(&pool, &mock.actor_ap_id(), &mock.inbox_url())
        .await
        .unwrap();
    let pending = publisher::publish(
        &state,
        &alice,
        &activity(1, "Create", json!([PUBLIC])),
        PublishOpts::default(),
    )
    .await
    .expect("the publish runs");
    assert_eq!(pending, 0, "a pending relay is nobody's fan-out target");

    // Accepted: every public activity reaches it — the four types the
    // ticket names, each as one delivery to the relay's inbox alone.
    let _granted = relay::mark_accepted(&pool, &mock.actor_ap_id())
        .await
        .unwrap();
    for (n, kind) in ["Create", "Announce", "Update", "Delete"]
        .into_iter()
        .enumerate()
    {
        let n = u32::try_from(n + 2).unwrap_or(2);
        let count = publisher::publish(
            &state,
            &alice,
            &activity(n, kind, json!([PUBLIC])),
            PublishOpts::default(),
        )
        .await
        .expect("the publish runs");
        assert_eq!(
            count, 1,
            "a public {kind} fans out to the relay's inbox alone"
        );
    }

    let inboxes = queued_inboxes(&pool).await;
    assert_eq!(
        inboxes,
        vec![mock.inbox_url(); 4],
        "every queued delivery asks for the accepted relay's inbox"
    );
}

/// The accepted relay stays out of the fan-out of activities that name
/// nobody public — and the actor's own `Update` reaches it through the
/// self-broadcast arm, so relays keep their copies fresh.
#[tokio::test]
async fn non_public_activities_leave_the_accepted_relay_out() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let mock = MockRelay::start().await;
    let alice = common::create_local_user(&pool, "alice").await;
    let strangers_followers = "https://remote.example/users/erin/followers";
    let _subscription = relay::upsert(&pool, &mock.actor_ap_id(), &mock.inbox_url())
        .await
        .unwrap();
    let _granted = relay::mark_accepted(&pool, &mock.actor_ap_id())
        .await
        .unwrap();
    let activity = |n: u32, kind: &str, to: Value| {
        json!({
            "id": format!("{BASE}/activities/relay-fanout-{n}"),
            "type": kind,
            "actor": alice.ap_id,
            "object": format!("{BASE}/objects/fanout-{n}"),
            "to": to,
        })
    };

    let followers_only = publisher::publish(
        &state,
        &alice,
        &activity(6, "Create", json!([strangers_followers])),
        PublishOpts::default(),
    )
    .await
    .expect("the publish runs");
    assert_eq!(
        followers_only, 0,
        "a followers-addressed post reaches no relay"
    );
    let direct = publisher::publish(
        &state,
        &alice,
        &activity(7, "Announce", json!([alice.ap_id])),
        PublishOpts::default(),
    )
    .await
    .expect("the publish runs");
    assert_eq!(direct, 0, "a non-public Announce is nobody's relay feed");

    // The actor's own Update, addressed only to a followers collection:
    // the self-broadcast arm carries it to the relay too.
    let own_update = json!({
        "id": format!("{BASE}/activities/relay-fanout-8"),
        "type": "Update",
        "actor": alice.ap_id,
        "object": alice.ap_id,
        "to": [strangers_followers],
    });
    let self_broadcast = publisher::publish(&state, &alice, &own_update, PublishOpts::default())
        .await
        .expect("the publish runs");
    assert_eq!(
        self_broadcast, 1,
        "the actor's own Update runs the relay arm"
    );

    let inboxes = queued_inboxes(&pool).await;
    assert_eq!(
        inboxes,
        vec![mock.inbox_url()],
        "and the queue holds exactly that one relay delivery"
    );
}

/// The inboxes of every queued `federator.deliver` job, insertion order.
async fn queued_inboxes(pool: &PgPool) -> Vec<String> {
    delivery_jobs(pool)
        .await
        .into_iter()
        .map(|args| args["inbox"].as_str().expect("a queued inbox").to_owned())
        .collect()
}

// --- the Announce fold ----------------------------------------------------------------

/// An `Announce` from an accepted relay — whom no user of this instance
/// follows — is accepted: the announced object is fetched through
/// `resolve_status` and ingested as an ordinary public remote status, and
/// no reblog row names the relay.
#[tokio::test]
async fn a_relay_announce_ingests_without_boosting() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let mock = MockRelay::start().await;
    let (_row, relay_user) = followed_and_accepted(&state, &mock).await;

    let alice = common::create_local_user(&pool, "alice").await;
    let local_post = common::create_status(&pool, &alice, "boostable", Visibility::Public).await;

    let announce_remote = json!({
        "id": mock.activity_uri("announce-remote"),
        "type": "Announce",
        "actor": mock.actor_ap_id(),
        "object": mock.object_uri("relayed"),
        "to": [PUBLIC],
    });
    let announce_local = json!({
        "id": mock.activity_uri("announce-local"),
        "type": "Announce",
        "actor": mock.actor_ap_id(),
        "object": local_post.uri,
        "to": [PUBLIC],
    });
    for activity in [&announce_remote, &announce_local] {
        deliver(&state, activity, &relay_user).await;
    }

    let ingested = status::get_by_uri(&pool, &mock.object_uri("relayed"))
        .await
        .expect("the status reads")
        .expect("the announced object was fetched and ingested");
    assert_eq!(
        ingested.visibility,
        Visibility::Public,
        "as an ordinary public remote status"
    );
    assert!(!ingested.local, "and it stays a mirror, not a local post");
    let author = get_by_ap_id(&pool, &format!("{}/users/author", mock.mock.base))
        .await
        .expect("the author reads")
        .expect("the ingest mirrored the author");
    assert_eq!(
        ingested.user_id, author.id,
        "authored by the object's own account"
    );

    for announce in [&announce_remote, &announce_local] {
        let boost = status::get_by_uri(&pool, announce["id"].as_str().expect("an id"))
            .await
            .expect("the status reads");
        assert!(
            boost.is_none(),
            "the relay's Announce {} creates no reblog row",
            announce["id"]
        );
    }
    let relayed_boosts = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM statuses WHERE user_id = $1 AND reblog_of_id IS NOT NULL",
    )
    .bind(relay_user.id)
    .fetch_one(&pool)
    .await
    .expect("the count reads");
    assert_eq!(
        relayed_boosts, 0,
        "and the relay holds no boost of anything"
    );
}

/// The control beside the relay fold: an `Announce` of a local public
/// post from a non-relay actor still makes its reblog row.
#[tokio::test]
async fn a_non_relay_announce_still_boosts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let alice = common::create_local_user(&pool, "alice").await;
    let local_post = common::create_status(&pool, &alice, "boostable", Visibility::Public).await;
    let bobo = common::create_remote_user(&pool, "bobo", "remote.example").await;

    let plain = json!({
        "id": "https://remote.example/activities/boost-1",
        "type": "Announce",
        "actor": bobo.ap_id,
        "object": local_post.uri,
        "to": [PUBLIC],
    });
    deliver(&state, &plain, &bobo).await;
    let boost = status::get_by_uri(&pool, "https://remote.example/activities/boost-1")
        .await
        .expect("the status reads")
        .expect("a non-relay Announce still makes its reblog row");
    assert_eq!(
        boost.reblog_of_id,
        Some(local_post.id),
        "boosting the very post the relay only carries"
    );
}

/// A pending subscription opens no relay arm: until the `Accept` folds,
/// the actor's `Announce` folds like any other remote account's — with
/// its reblog row.
#[tokio::test]
async fn a_pending_relay_announce_folds_like_a_strangers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);

    let relay_user = common::create_remote_user(&pool, "relay", "relay.mock").await;
    let _subscription = relay::upsert(
        &pool,
        &relay_user.ap_id,
        relay_user.inbox.as_deref().expect("a fixture inbox"),
    )
    .await
    .unwrap();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &alice, "pending-target", Visibility::Public).await;

    let announce = json!({
        "id": "https://relay.mock/activities/pending-announce",
        "type": "Announce",
        "actor": relay_user.ap_id,
        "object": post.uri,
        "to": [PUBLIC],
    });
    deliver(&state, &announce, &relay_user).await;
    let boost = status::get_by_uri(&pool, "https://relay.mock/activities/pending-announce")
        .await
        .expect("the status reads")
        .expect("an unaccepted relay is still an ordinary booster");
    assert_eq!(boost.user_id, relay_user.id, "the row names its actor");
    assert_eq!(
        boost.reblog_of_id,
        Some(post.id),
        "and folds the boost the accepted relay would skip"
    );
}

// --- the withdrawal -------------------------------------------------------------------

/// Unfollowing a relay sends the `Undo` of that very `Follow` to its
/// inbox, deletes the `follows` row and the `relays` row, and takes the
/// relay out of the fan-out; an unknown relay is the 404.
#[tokio::test]
async fn unfollowing_a_relay_withdraws_the_follow_and_the_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let state = relay_state(&pool);
    let mock = MockRelay::start().await;
    let (row, relay_user) = followed_and_accepted(&state, &mock).await;
    let follow_id = row.ap_id.clone().expect("the minted id");
    let _delivered = common::run_all_jobs(&state).await;
    assert_eq!(mock.received().len(), 1, "the Follow reached the relay");

    unfollow_relay(&state, &mock.actor_ap_id())
        .await
        .expect("the withdrawal runs");

    let jobs = delivery_jobs(&pool).await;
    assert_eq!(jobs.len(), 2, "the Follow's Undo rides beside it");
    assert_eq!(jobs[1]["inbox"], mock.inbox_url(), "to the relay's inbox");
    let body = &jobs[1]["body"];
    assert_eq!(body["type"], "Undo", "the withdrawal activity");
    assert_eq!(
        body["actor"],
        format!("{BASE}{INSTANCE_ACTOR_PATH}"),
        "by the instance actor"
    );
    assert_eq!(
        body["to"],
        json!([mock.inbox_url()]),
        "addressed to the inbox"
    );
    assert_eq!(body["object"]["type"], "Follow", "embedding the Follow");
    assert_eq!(
        body["object"]["id"],
        json!(follow_id),
        "under its stored id"
    );
    assert_eq!(
        body["object"]["actor"],
        format!("{BASE}{INSTANCE_ACTOR_PATH}"),
        "as the instance actor sent it"
    );

    assert!(
        instance_follow(&state, &relay_user).await.is_none(),
        "the follows row is gone"
    );
    assert!(
        relay::get(&pool, &mock.actor_ap_id())
            .await
            .unwrap()
            .is_none(),
        "and so is the subscription"
    );
    assert!(
        list_relays(&pool).await.unwrap().is_empty(),
        "the ledger reads empty"
    );
    assert!(
        instance::relay_inboxes(&pool).await.unwrap().is_empty(),
        "and the fan-out hook holds nothing"
    );

    let _drained = common::run_all_jobs(&state).await;
    let posts = mock.received();
    assert_eq!(posts.len(), 2, "the relay's inbox recorded the Undo too");
    assert_eq!(posts[1]["type"], "Undo", "as an Undo");

    let stranger = unfollow_relay(&state, "never-followed.example").await;
    assert!(
        matches!(stranger, Err(AppError::NotFound)),
        "withdrawing a relay that was never followed is the 404: {stranger:?}"
    );
}

// --- the endpoints --------------------------------------------------------------------

/// `GET /relay` answers the instance actor document — the very JSON
/// `GET /internal/fetch` serves — under the usual negotiation: an
/// `Accept` naming an `ActivityPub` media type gets it, anyone else the
/// plain 404, and every answer says `Vary: Accept`.
#[tokio::test]
async fn the_relay_route_serves_the_instance_actor_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);

    let internal = common::TestRequest::get(INSTANCE_ACTOR_PATH)
        .header("accept", "application/activity+json")
        .send(&app)
        .await;
    let relay = common::TestRequest::get("/relay")
        .header("accept", "application/activity+json")
        .send(&app)
        .await;
    assert_eq!(relay.status, StatusCode::OK, "GET /relay answers a peer");
    assert_eq!(
        relay.header("content-type"),
        Some(ACTIVITY_JSON),
        "as an ActivityPub document"
    );
    assert_eq!(
        relay.header("vary"),
        Some("Accept"),
        "and the negotiation is cache-visible"
    );
    let document = relay.json_value();
    assert_eq!(
        document,
        internal.json_value(),
        "the very document /internal/fetch serves"
    );
    assert_eq!(
        document["id"],
        format!("{BASE}{INSTANCE_ACTOR_PATH}"),
        "under the instance actor's id"
    );

    let silent = common::TestRequest::get("/relay").send(&app).await;
    common::assert_answer(
        &silent,
        StatusCode::NOT_FOUND,
        "a client that states no Accept",
    );
    let browser = common::TestRequest::get("/relay")
        .header("accept", "text/html")
        .send(&app)
        .await;
    common::assert_error(&browser, "Not found", "a browser on the relay actor");
}

/// The two relay subscription collections answer empty
/// `OrderedCollection`s at their own addresses, under the same
/// negotiation: a peer gets the root, a silent client the plain 404.
#[tokio::test]
async fn the_relay_collections_are_empty_ordered_collections() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);

    for side in ["following", "followers"] {
        let path = format!("/relay/{side}");
        let asked = common::TestRequest::get(&path)
            .header("accept", "application/ld+json")
            .send(&app)
            .await;
        assert_eq!(asked.status, StatusCode::OK, "GET {path} answers a peer");
        assert_eq!(
            asked.header("content-type"),
            Some(ACTIVITY_JSON),
            "{path} is an ActivityPub document"
        );
        assert_eq!(
            asked.header("vary"),
            Some("Accept"),
            "and the answer varies on Accept"
        );
        let collection = asked.json_value();
        assert_eq!(
            collection["id"],
            format!("{BASE}/relay/{side}"),
            "the collection names its own address"
        );
        assert_eq!(
            collection["type"], "OrderedCollection",
            "the ticket's OrderedCollection"
        );
        assert_eq!(collection["totalItems"], 0, "empty");
        assert_eq!(
            collection["first"],
            format!("{BASE}/relay/{side}?page=true"),
            "with the paged-first link the other roots carry"
        );
        assert!(
            collection.get("@context").is_some(),
            "and the JSON-LD context every document carries"
        );

        let quiet = common::TestRequest::get(&path).send(&app).await;
        common::assert_answer(
            &quiet,
            StatusCode::NOT_FOUND,
            &format!("a silent client on {path}"),
        );
    }
}

/// `POST /relay/inbox` is the shared inbox: a signed delivery is queued as
/// one `federator.incoming` job with no `inbox_owner` and answered 202,
/// and an unsigned one is the extractor's 401.
#[tokio::test]
async fn the_relay_inbox_is_the_shared_inbox() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let config = common::test_config();
    let actor = ensure_instance_actor(&pool, &config)
        .await
        .expect("the instance actor is provisioned");
    let private_pem = actor
        .private_key
        .clone()
        .expect("the provisioned actor holds its private key");
    let key_id = format!("{}#main-key", actor.ap_id);
    let app = common::test_app(pool.clone());

    let unsigned = common::TestRequest::post("/relay/inbox")
        .json(&json!({
            "id": "https://peer.example/activities/un-signed",
            "type": "Follow",
            "actor": "https://peer.example/users/alice",
            "object": format!("{BASE}/users/bob"),
        }))
        .send(&app)
        .await;
    assert_eq!(
        unsigned.status,
        StatusCode::UNAUTHORIZED,
        "an unsigned delivery is refused"
    );

    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://peer.example/activities/relay-signed",
        "type": "Follow",
        "actor": actor.ap_id,
        "object": format!("{BASE}/users/bob"),
    });
    let wire = sign_request(
        "POST",
        &format!("{BASE}/relay/inbox"),
        Some(&serde_json::to_vec(&activity).expect("the test activity serializes")),
        &SigningKey {
            key_id: &key_id,
            private_pem: &private_pem,
        },
        Utc::now(),
    )
    .expect("signing the test delivery");
    let accepted = common::TestRequest::post("/relay/inbox")
        .json(&activity)
        .header("host", &wire.host)
        .header("date", &wire.date)
        .header("digest", wire.digest.as_deref().expect("a body digest"))
        .header("signature", &wire.signature)
        .send(&app)
        .await;
    assert_eq!(
        accepted.status,
        StatusCode::ACCEPTED,
        "the relay inbox accepts a signed delivery"
    );
    assert_eq!(accepted.text(), "", "with an empty body");

    let rows = sqlx::query_as::<_, (Value,)>("SELECT args FROM jobs WHERE kind = $1")
        .bind(KIND_INCOMING)
        .fetch_all(&pool)
        .await
        .expect("the incoming jobs read");
    assert_eq!(rows.len(), 1, "exactly one job was queued");
    assert_eq!(
        rows[0].0["inbox_owner"],
        Value::Null,
        "the shared handler claims no owner"
    );
    assert_eq!(
        rows[0].0["signer_id"],
        actor.id.to_string(),
        "the signer is recorded"
    );
    assert_eq!(rows[0].0["activity"], activity, "and the activity rides");
}
