//! The mock relay, the fixtures and the reads the relay tests share. The
//! test modules are its children, so they reach every helper through
//! `use super::*`.

mod fan_out;
mod follow;
mod routes;

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
        follow::{Follow, FollowState},
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
    wired_be::models::follow::get(&state.db, instance.id, mirror.id)
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

/// The inboxes of every queued `federator.deliver` job, insertion order.
async fn queued_inboxes(pool: &PgPool) -> Vec<String> {
    delivery_jobs(pool)
        .await
        .into_iter()
        .map(|args| args["inbox"].as_str().expect("a queued inbox").to_owned())
        .collect()
}
