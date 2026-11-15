//! The admin federation endpoints over HTTP
//! (`/api/v1/pleroma/admin/relay`, `/instances/{instance}` and
//! `/reload_emoji`, WBE-203): the relay follow / unfollow lifecycle
//! driven through the admin API against a loopback mock relay, the
//! `{actor, followed_back}` ledger, the instance deletion staging its
//! `users.delete_remote` sweeps and clearing the `instances` row, the
//! emoji reload resyncing a freshly written pack into `custom_emojis`,
//! every refusal the ticket pins, and the `AdminUser` / scope gate in
//! front of all five routes.
//!
//! The mock is the one `relays.rs` runs — an `Application` actor at
//! `/relay`, a capturing inbox at `/relay/inbox`, a `Group` outside the
//! relay shapes and a path that answers 404 — reached through the stock
//! test config whose HTTP client tolerates loopback. The relay's follow
//! back at the instance actor (the `followed_back` fact) is staged
//! through the `follows` model: the ledger read only asks that question,
//! and no inbound handshake is needed to state the answer.

#![cfg(test)]

use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpListener, task::JoinHandle};
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    federation::delivery::KIND_DELIVER,
    jobs::deletion::KIND_DELETE_REMOTE,
    models::{
        follow, instance, relay,
        user::{Flag, NewUser, User, get_by_ap_id, get_by_id, mark_deleted},
    },
    services::{
        emoji_packs::{self, Pack, PackMeta},
        instance_actor::ensure_instance_actor,
    },
    state::AppState,
};

use crate::common;

/// The relay ledger route.
const RELAY: &str = "/api/v1/pleroma/admin/relay";
/// The instance-deletion route prefix.
const INSTANCES: &str = "/api/v1/pleroma/admin/instances";
/// The emoji reload route.
const RELOAD: &str = "/api/v1/pleroma/admin/reload_emoji";
/// The admin scopes both halves of every gate here accept, as roots.
const ALL_ADMIN: &str = "admin:read admin:write";
/// Bytes for a fixture pack's image files.
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nfake";

// --- the mock relay -------------------------------------------------------------------

/// The mock's shared state: its origin and the activities its inbox
/// recorded.
#[derive(Clone, Debug)]
struct Mock {
    /// The `http://{addr}` origin this relay answers on.
    base: String,
    /// The `POST` bodies its inbox recorded, oldest first.
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
    /// Serves the relay actor, a `Group` actor, a 404 path and a
    /// capturing inbox on a fresh loopback port.
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
            .route("/group", get(group_actor))
            .route("/missing", get(|| async { StatusCode::NOT_FOUND }))
            .route("/relay/inbox", post(capture))
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

    /// Any other origin path, for the refusal cases.
    fn url_at(&self, path: &str) -> String {
        format!("{}{path}", self.mock.base)
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

/// The `GET /relay` fixture: an `Application` actor with its inbox under
/// its own id.
async fn relay_actor(State(mock): State<Mock>) -> Json<Value> {
    Json(actor_doc(&mock, "relay", "Application"))
}

/// Not a relay: a `Group` is outside the three followable shapes.
async fn group_actor(State(mock): State<Mock>) -> Json<Value> {
    Json(actor_doc(&mock, "group", "Group"))
}

/// A self-naming actor document of `kind` at `{base}/{path}`.
fn actor_doc(mock: &Mock, path: &str, kind: &str) -> Value {
    let id = format!("{}/{path}", mock.base);
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": kind,
        "id": id,
        "preferredUsername": path,
        "name": path,
        "inbox": format!("{id}/inbox"),
        "followers": format!("{id}/followers"),
        "publicKey": {
            "id": format!("{id}#main-key"),
            "owner": id,
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
        },
    })
}

/// Records the delivered activity and accepts it.
async fn capture(State(mock): State<Mock>, body: axum::body::Bytes) -> StatusCode {
    let parsed = serde_json::from_slice(&body).unwrap_or(Value::Null);
    mock.posts.lock().expect("the capture lock").push(parsed);
    StatusCode::ACCEPTED
}

// --- shared fixtures and reads --------------------------------------------------------

/// The router and its state over the stock test config (the HTTP client
/// reaches the loopback mock, and the job drain rides the same state).
fn federation_app(pool: &PgPool) -> (Router, AppState) {
    let state = common::test_app_state(pool.clone(), common::test_config());
    (wired_be::app(state.clone()), state)
}

/// A local admin named `nickname` with a bearer token granting `scopes`.
async fn admin(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// The newest `moderation_log` row written for `action`: its sentence
/// and data map, insisting at least one stands.
async fn log_entry(pool: &PgPool, action: &str) -> (String, Value) {
    let found = sqlx::query_as::<_, (String, Value)>(
        "SELECT message, data FROM moderation_log WHERE action = $1 \
         ORDER BY inserted_at DESC, id DESC LIMIT 1",
    )
    .bind(action)
    .fetch_optional(pool)
    .await
    .expect("the log reads");
    found.expect("at least one entry for the action")
}

/// The args of every queued job of `kind`, in insertion order.
async fn queued_args(pool: &PgPool, kind: &str) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>("SELECT args FROM jobs WHERE kind = $1 ORDER BY inserted_at, id")
        .bind(kind)
        .fetch_all(pool)
        .await
        .expect("the jobs read")
        .into_iter()
        .map(|(args,)| args)
        .collect()
}

/// The relay's accepted follow of the instance actor — the fact
/// `followed_back` states — staged through the model: a follow of the
/// locked service actor, granted by hand.
async fn follow_back(pool: &PgPool, relay_user: &User) {
    let config = common::test_config();
    let instance = ensure_instance_actor(pool, &config)
        .await
        .expect("the instance actor stands");
    let row = follow::follow(pool, TEST_BASE_URL, relay_user, &instance)
        .await
        .expect("the relay's follow records");
    let _granted = follow::accept(pool, row.id)
        .await
        .expect("the instance actor grants it");
}

/// `POST /relay` for `relay_url` as `token`, insisting on `status`, and
/// handing back the body.
async fn post_relay(
    app: &Router,
    token: &str,
    relay_url: &str,
    status: StatusCode,
    who: &str,
) -> Value {
    let answer = common::TestRequest::post(RELAY)
        .bearer(token)
        .json(&json!({"relay_url": relay_url}))
        .send(app)
        .await;
    common::assert_answer(&answer, status, who);
    answer.json_value()
}

// --- the relay lifecycle ----------------------------------------------------------------

/// What a successful admin follow of the mock records: the pending
/// subscription row at the mock's inbox, the `relay_follow` entry naming
/// the target, and the one queued `Follow` delivery addressed to that
/// inbox alone.
async fn assert_follow_recorded(pool: &PgPool, mock: &MockRelay) {
    let row = relay::get(pool, &mock.actor_ap_id())
        .await
        .expect("the ledger reads")
        .expect("the subscription stands");
    assert_eq!(row.inbox, mock.inbox_url(), "keyed by the actor, inboxed");
    assert_eq!(row.state, relay::RelayState::Pending, "pending first");
    let (message, data) = log_entry(pool, "relay_follow").await;
    assert_eq!(
        message,
        format!("@root followed relay: {}", mock.actor_ap_id()),
        "the write is logged"
    );
    assert_eq!(data["target"], mock.actor_ap_id(), "with its target");
    let queued = queued_args(pool, KIND_DELIVER).await;
    assert_eq!(queued.len(), 1, "the Follow rides as one delivery");
    assert_eq!(queued[0]["inbox"], mock.inbox_url(), "to the relay's inbox");
    assert_eq!(queued[0]["body"]["type"], "Follow", "the follow activity");
}

/// What the withdrawal leaves behind: neither the `relays` row nor the
/// instance actor's `follows` row stands, and the newest log entry is
/// the `relay_unfollow` sentence naming the target.
async fn assert_relay_withdrawn(pool: &PgPool, relay_user: &User, actor_ap_id: &str) {
    assert!(
        relay::get(pool, actor_ap_id)
            .await
            .expect("the ledger reads")
            .is_none(),
        "the subscription row is gone"
    );
    let config = common::test_config();
    let instance = ensure_instance_actor(pool, &config)
        .await
        .expect("the instance actor stands");
    assert!(
        follow::get(pool, instance.id, relay_user.id)
            .await
            .expect("the follow reads")
            .is_none(),
        "and so is the instance actor's follow of the relay"
    );
    let (sentence, _) = log_entry(pool, "relay_unfollow").await;
    assert_eq!(
        sentence,
        format!("@root unfollowed relay: {actor_ap_id}"),
        "the withdrawal is logged"
    );
}

/// Following a relay through the admin API answers the pair and writes
/// the ledger row, the `relay_follow` entry and the one queued `Follow`;
/// the listing then shows the `followed_back` flag flip once the relay
/// follows the instance actor back, and a restated follow stays one row,
/// one send — answering the current flag.
#[tokio::test]
async fn following_a_relay_through_the_admin_api_walks_the_handshake() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let mock = MockRelay::start().await;
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    let followed = post_relay(
        &app,
        &token,
        &mock.actor_ap_id(),
        StatusCode::OK,
        "following a relay",
    )
    .await;
    assert_eq!(
        followed,
        json!({"actor": mock.actor_ap_id(), "followed_back": false}),
        "the answer names the relay and its fresh state"
    );
    assert_follow_recorded(&pool, &mock).await;

    let listed = common::TestRequest::get(RELAY)
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&listed, StatusCode::OK, "listing the ledger");
    assert_eq!(
        listed.json_value(),
        json!({"relays": [{"actor": mock.actor_ap_id(), "followed_back": false}]}),
        "one entry, not followed back yet"
    );

    let relay_user = get_by_ap_id(&pool, &mock.actor_ap_id())
        .await
        .expect("the mirror reads")
        .expect("the follow mirrored the relay actor");
    follow_back(&pool, &relay_user).await;
    let back = common::TestRequest::get(RELAY)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        back.json_value()["relays"][0]["followed_back"],
        json!(true),
        "the accepted follow of the instance actor reads back"
    );

    let restated = post_relay(
        &app,
        &token,
        &mock.actor_ap_id(),
        StatusCode::OK,
        "restating the follow",
    )
    .await;
    assert_eq!(
        restated,
        json!({"actor": mock.actor_ap_id(), "followed_back": true}),
        "a restated follow answers the current flag"
    );
    assert_eq!(
        queued_args(&pool, KIND_DELIVER).await.len(),
        1,
        "and the queue still holds one send"
    );
    assert_eq!(
        relay::list(&pool).await.expect("the ledger reads").len(),
        1,
        "and still one subscription"
    );
}

/// Unfollowing through the admin API answers the requested address as a
/// JSON string, clears the `relays` row and the instance actor's
/// `follows` row, logs the withdrawal and queues the `Undo` beside the
/// `Follow` — and a second withdrawal of the same relay is the 400
/// `Not found`.
#[tokio::test]
async fn unfollowing_a_relay_through_the_admin_api_withdraws_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = federation_app(&pool);
    let mock = MockRelay::start().await;
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let _first = post_relay(
        &app,
        &token,
        &mock.actor_ap_id(),
        StatusCode::OK,
        "following the walk's relay",
    )
    .await;
    let relay_user = get_by_ap_id(&pool, &mock.actor_ap_id())
        .await
        .expect("the mirror reads")
        .expect("the follow mirrored the relay actor");
    follow_back(&pool, &relay_user).await;

    let unfollowed = common::TestRequest::delete(RELAY)
        .bearer(&token)
        .json(&json!({"relay_url": mock.actor_ap_id(), "force": true}))
        .send(&app)
        .await;
    common::assert_answer(&unfollowed, StatusCode::OK, "unfollowing a relay");
    assert_eq!(
        unfollowed.header("content-type"),
        Some("application/json"),
        "the answer is JSON"
    );
    assert_eq!(
        unfollowed.json::<String>(),
        mock.actor_ap_id(),
        "the requested address, as a JSON string"
    );
    assert_relay_withdrawn(&pool, &relay_user, &mock.actor_ap_id()).await;
    let queued = queued_args(&pool, KIND_DELIVER).await;
    assert_eq!(queued.len(), 2, "the Undo rides beside the Follow");
    assert_eq!(queued[1]["body"]["type"], "Undo", "as an Undo");

    let emptied = common::TestRequest::get(RELAY)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        emptied.json_value(),
        json!({"relays": []}),
        "the ledger reads empty"
    );

    let repeat = common::TestRequest::delete(RELAY)
        .bearer(&token)
        .json(&json!({"relay_url": mock.actor_ap_id()}))
        .send(&app)
        .await;
    common::assert_answer(&repeat, StatusCode::BAD_REQUEST, "an unknown relay");
    common::assert_error(&repeat, "Not found", "the withdrawal of a stranger");

    let ran = common::run_all_jobs(&state).await;
    assert!(ran >= 2, "both deliveries ran");
    let posts = mock.received();
    assert_eq!(posts.len(), 2, "the relay's inbox saw both handshakes");
    assert_eq!(posts[0]["type"], "Follow", "the follow arrived");
    assert_eq!(posts[1]["type"], "Undo", "and the withdrawal");
}

/// The listing's shape over a seeded ledger: one entry per row, oldest
/// subscription first, `followed_back` true only for the relay whose
/// mirror holds an accepted follow of the instance actor — a pending
/// follow and an unmirrored actor both read false.
#[tokio::test]
async fn the_ledger_lists_every_relay_with_its_follow_back_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let (_root, token) = admin(&pool, "root", "admin:read:relays").await;

    let backer = common::create_remote_user(&pool, "relay", "back.example").await;
    let waiter = common::create_remote_user(&pool, "relay", "wait.example").await;
    let stranger = "https://stranger.example/relay";
    let _first = relay::upsert(&pool, &backer.ap_id, "https://back.example/relay/inbox")
        .await
        .expect("the backer's subscription stores");
    let _second = relay::upsert(&pool, stranger, "https://stranger.example/inbox")
        .await
        .expect("the unmirrored relay's subscription stores");
    let _third = relay::upsert(&pool, &waiter.ap_id, "https://wait.example/relay/inbox")
        .await
        .expect("the waiter's subscription stores");
    // Settle the order: back.example oldest, then the stranger, then wait.
    for (ap_id, secs) in [
        (backer.ap_id.as_str(), 120_i32),
        (stranger, 60),
        (waiter.ap_id.as_str(), 1),
    ] {
        let _aged = sqlx::query(
            "UPDATE relays SET inserted_at = now() - make_interval(secs => $2) WHERE actor_ap_id = $1",
        )
        .bind(ap_id)
        .bind(secs)
        .execute(&pool)
        .await
        .expect("the stamps are settable");
    }

    follow_back(&pool, &backer).await;
    let config = common::test_config();
    let instance = ensure_instance_actor(&pool, &config)
        .await
        .expect("the instance actor stands");
    let pending = follow::follow(&pool, TEST_BASE_URL, &waiter, &instance)
        .await
        .expect("the waiter's follow records");
    assert_eq!(
        pending.state,
        follow::FollowState::Pending,
        "a locked target keeps the relay's follow pending"
    );

    let listed = common::TestRequest::get(RELAY)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        listed.json_value(),
        json!({"relays": [
            {"actor": backer.ap_id, "followed_back": true},
            {"actor": stranger, "followed_back": false},
            {"actor": waiter.ap_id, "followed_back": false},
        ]}),
        "one entry per row, oldest first, only the accepted follow-back true"
    );
}

/// The follow refusals, each answered 400 with the service's own
/// message: an address that is neither URL nor host, a body that names
/// no address at all, a `Group` outside the relay shapes, and an actor
/// with nothing behind it.
#[tokio::test]
async fn every_failed_follow_is_a_400_with_its_message() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let mock = MockRelay::start().await;
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;

    let refusals = [
        (
            "nope/nope",
            "`nope/nope` is neither a relay actor URL nor a host",
        ),
        ("", "no relay address was given"),
        (
            &mock.url_at("/group"),
            "the relay actor is a Group, not one of Application, Service or Person",
        ),
        (&mock.url_at("/missing"), "Record not found"),
    ];
    for (address, message) in refusals {
        let answer = common::TestRequest::post(RELAY)
            .bearer(&token)
            .json(&json!({"relay_url": address}))
            .send(&app)
            .await;
        common::assert_answer(&answer, StatusCode::BAD_REQUEST, "a refused follow");
        common::assert_error(&answer, message, &format!("refusing {address:?}"));
    }

    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM relays")
            .fetch_one(&pool)
            .await
            .expect("the ledger reads"),
        0,
        "no refusal left a subscription behind"
    );
}

// --- the instance deletion ---------------------------------------------------------------

/// The instance-deletion world: a local follower of two fresh mirrors
/// and a pre-stamped third one on the doomed domain, a mirror on a kept
/// domain, and a row in the ledger for each host.
#[derive(Debug)]
struct Sweep {
    /// The local account following one doomed mirror.
    alice: User,
    /// The first doomed mirror — the one followed.
    bob: User,
    /// The second doomed mirror.
    carol: User,
    /// The doomed mirror already marked deleted.
    dave: User,
    /// A mirror of another domain.
    erin: User,
}

/// Builds the [`Sweep`] fixture.
async fn sweep_world(pool: &PgPool) -> Sweep {
    let alice = common::create_local_user(pool, "alice").await;
    let bob = common::create_remote_user(pool, "bob", "sweep.example").await;
    let carol = common::create_remote_user(pool, "carol", "sweep.example").await;
    let dave = common::create_remote_user(pool, "dave", "sweep.example").await;
    let erin = common::create_remote_user(pool, "erin", "keep.example").await;
    mark_deleted(pool, dave.id)
        .await
        .expect("the pre-stamped mirror stands deleted");
    let row = follow::follow(pool, TEST_BASE_URL, &alice, &bob)
        .await
        .expect("alice follows bob");
    let _granted = follow::accept(pool, row.id)
        .await
        .expect("the follow is accepted");
    instance::touch(pool, "sweep.example")
        .await
        .expect("the doomed host has a row");
    instance::touch(pool, "keep.example")
        .await
        .expect("the kept host has a row");
    Sweep {
        alice,
        bob,
        carol,
        dave,
        erin,
    }
}

/// `id`'s user row, insisted upon.
async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
    get_by_id(pool, id)
        .await
        .expect("the user reads")
        .expect("the row stands")
}

/// Deleting a REMOTE instance: every fresh mirror of the domain is
/// marked deleted and rides its own background `users.delete_remote`
/// sweep — queued in one batch — while local rows, another domain's
/// mirrors and rows already marked stand aside; the `instances` row goes
/// and the answer is the domain as a JSON string.
#[tokio::test]
async fn deleting_an_instance_stages_the_sweeps_and_clears_the_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let (_root, token) = admin(&pool, "root", ALL_ADMIN).await;
    let world = sweep_world(&pool).await;

    let deleted = common::TestRequest::delete(&format!("{INSTANCES}/sweep.example"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&deleted, StatusCode::OK, "deleting an instance");
    assert_eq!(
        deleted.header("content-type"),
        Some("application/json"),
        "the answer is JSON"
    );
    assert_eq!(
        deleted.json::<String>(),
        "sweep.example",
        "the domain as a JSON string, Pleroma's body"
    );

    let sweeps = queued_args(&pool, KIND_DELETE_REMOTE).await;
    let mut doomed: Vec<String> = sweeps
        .iter()
        .map(|args| args["user_id"].as_str().expect("a user id").to_owned())
        .collect();
    doomed.sort();
    let mut expected = vec![world.bob.id.to_string(), world.carol.id.to_string()];
    expected.sort();
    assert_eq!(
        doomed, expected,
        "one job per fresh mirror — the pre-stamped row is nobody to re-delete"
    );
    for id in [world.bob.id, world.carol.id] {
        let gone = fresh_user(&pool, id).await;
        assert!(!gone.is_active.get(), "{id} is deactivated");
        assert!(gone.deleted_at.is_some(), "{id} carries the deletion stamp");
    }
    let pre = fresh_user(&pool, world.dave.id).await;
    assert!(
        !pre.is_active.get() && pre.deleted_at.is_some(),
        "the pre-stamped mirror stands deleted, queued nothing again"
    );
    let untouched = [
        fresh_user(&pool, world.alice.id).await,
        fresh_user(&pool, world.erin.id).await,
    ];
    assert!(
        untouched
            .iter()
            .all(|u| u.is_active.get() && u.deleted_at.is_none()),
        "the local account and the kept domain's mirror stand untouched"
    );

    let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM instances WHERE host = $1")
        .bind("sweep.example")
        .fetch_one(&pool)
        .await
        .expect("the row count reads");
    assert_eq!(rows, 0, "the instances row is cleared");
    let (sentence, data) = log_entry(&pool, "delete_instance").await;
    assert_eq!(
        sentence, "@root deleted instance: sweep.example",
        "the write is logged"
    );
    assert_eq!(data["target"], "sweep.example", "with the domain as target");
}

/// The staged sweeps complete on the queue: running the jobs clears the
/// relationships the doomed mirrors name, steps the live follower's
/// counters, and leaves the kept domain's mirror's follow standing.
#[tokio::test]
async fn the_instance_deletion_sweeps_run_on_the_queue() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = federation_app(&pool);
    let (_root, token) = admin(&pool, "root", "admin:write:federation").await;
    let world = sweep_world(&pool).await;
    let kept = follow::follow(&pool, TEST_BASE_URL, &world.alice, &world.erin)
        .await
        .expect("alice also follows the kept mirror");
    let _granted = follow::accept(&pool, kept.id)
        .await
        .expect("that follow stands accepted");

    let deleted = common::TestRequest::delete(&format!("{INSTANCES}/Sweep.Example"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(
        &deleted,
        StatusCode::OK,
        "the mixed-case domain deletes too",
    );
    assert_eq!(
        deleted.json::<String>(),
        "sweep.example",
        "the answer carries the domain as the ledger keys it"
    );
    let queued_elsewhere = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND queue <> 'background'",
    )
    .bind(KIND_DELETE_REMOTE)
    .fetch_one(&pool)
    .await
    .expect("the queue reads");
    assert_eq!(
        queued_elsewhere, 0,
        "every sweep sits on the background queue"
    );

    let ran = common::run_all_jobs(&state).await;
    assert_eq!(ran, 2, "both sweeps ran");
    assert!(
        follow::get(&pool, world.alice.id, world.bob.id)
            .await
            .expect("the follow reads")
            .is_none(),
        "alice's follow of the swept mirror is gone"
    );
    let alice = fresh_user(&pool, world.alice.id).await;
    assert_eq!(
        alice.following_count, 1,
        "only the swept follow stepped back"
    );
    assert!(
        follow::get(&pool, world.alice.id, world.erin.id)
            .await
            .expect("the follow reads")
            .is_some(),
        "the kept mirror's follow stands"
    );
    let kept_rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM instances WHERE host = $1")
        .bind("keep.example")
        .fetch_one(&pool)
        .await
        .expect("the row count reads");
    assert_eq!(kept_rows, 1, "and the neighbouring ledger row stands");
    let done = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND state = 'completed'",
    )
    .bind(KIND_DELETE_REMOTE)
    .fetch_one(&pool)
    .await
    .expect("the job states read");
    assert_eq!(done, 2, "the queue records both sweeps complete");
}

/// This instance's own domain is not a deletion candidate — Pleroma's
/// refusal, verbatim, in any spelling of the host — and an unknown peer
/// domain deletes cleanly as a no-op.
#[tokio::test]
async fn the_local_domain_refuses_deletion_and_a_stranger_is_a_noop() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let (_root, token) = admin(&pool, "root", "admin:write:federation").await;

    for spelling in ["test.example", "TEST.Example"] {
        let path = format!("{INSTANCES}/{spelling}");
        let refused = common::TestRequest::delete(&path)
            .bearer(&token)
            .send(&app)
            .await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, "the local domain");
        common::assert_error(
            &refused,
            "Can't delete the local instance",
            "Pleroma's refusal wording",
        );
    }

    let stranger = common::TestRequest::delete(&format!("{INSTANCES}/never-seen.example"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&stranger, StatusCode::OK, "a domain with no rows");
    assert_eq!(
        stranger.json::<String>(),
        "never-seen.example",
        "the answer is still the domain"
    );
    assert!(
        queued_args(&pool, KIND_DELETE_REMOTE).await.is_empty(),
        "and nothing was queued"
    );
}

// --- the emoji reload ---------------------------------------------------------------------

/// `POST /reload_emoji` re-scans `EMOJI_DIR`: a pack written after
/// startup appears in `custom_emojis` the moment the endpoint runs, and a
/// pack taken off disk loses its rows on the next reload — the sync pass
/// the boot runs, on demand, and the JSON string `"ok"` for an answer.
#[tokio::test]
async fn reload_emoji_resyncs_the_packs_from_disk() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let config = common::test_config_with_emoji_dir(dir.path());
    let state = common::test_app_state(pool.clone(), config);
    let app = wired_be::app(state);
    let (_root, token) = admin(&pool, "root", "admin:write:emoji").await;

    let stored = || async {
        sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT shortcode, image_url, category FROM custom_emojis ORDER BY shortcode",
        )
        .fetch_all(&pool)
        .await
        .expect("the emojis read")
    };

    let first = common::TestRequest::post(RELOAD)
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&first, StatusCode::OK, "reloading an empty root");
    assert_eq!(
        first.json::<String>(),
        "ok",
        "the body is the string \"ok\""
    );
    assert!(stored().await.is_empty(), "nothing on disk, nothing stored");

    let manifest = Pack {
        pack: PackMeta {
            share_files: true,
            ..PackMeta::default()
        },
        files: [("smile", "smile.png"), ("grin", "grin.png")]
            .into_iter()
            .map(|(code, file)| (code.to_owned(), file.to_owned()))
            .collect(),
    };
    emoji_packs::save_pack(dir.path(), "fun", &manifest)
        .await
        .expect("the fixture pack saves");
    for file in manifest.files.values() {
        std::fs::write(dir.path().join("fun").join(file), PNG_BYTES).expect("the image writes");
    }

    let second = common::TestRequest::post(RELOAD)
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&second, StatusCode::OK, "reloading after a new pack");
    assert_eq!(second.json::<String>(), "ok", "again the string \"ok\"");
    assert_eq!(
        stored().await,
        vec![
            (
                "grin".to_owned(),
                format!("{TEST_BASE_URL}/emoji/fun/grin.png"),
                Some("fun".to_owned())
            ),
            (
                "smile".to_owned(),
                format!("{TEST_BASE_URL}/emoji/fun/smile.png"),
                Some("fun".to_owned())
            ),
        ],
        "the newly written pack is synced into custom_emojis"
    );
    let (sentence, _) = log_entry(&pool, "reload_emoji").await;
    assert_eq!(sentence, "@root reloaded emoji", "the reload is logged");

    // The other half of the resync: a pack taken off disk loses its rows.
    emoji_packs::delete_pack(dir.path(), "fun")
        .await
        .expect("the fixture pack deletes");
    let third = common::TestRequest::post(RELOAD)
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&third, StatusCode::OK, "reloading after the deletion");
    assert!(
        stored().await.is_empty(),
        "and the sweep drops the stale rows"
    );
}

// --- the gate ---------------------------------------------------------------------------------

/// The tokens the gate tests hand the routes: the admin's roots, an
/// ordinary user carrying the same scopes, a moderator, an app-only
/// token, and the two narrow relay scopes.
#[derive(Debug)]
struct Tokens {
    /// The admin's `admin:read admin:write`.
    all: String,
    /// A non-admin's same roots.
    plain: String,
    /// A moderator's same roots.
    staff: String,
    /// A user-unbound token with the same roots.
    app_only: String,
    /// An admin holding only `admin:read:relays`.
    read_relays: String,
    /// An admin holding only `admin:write:relays`.
    write_relays: String,
}

/// Mints the [`Tokens`] fixture.
async fn tokens(pool: &PgPool) -> Tokens {
    let (_root, all) = admin(pool, "root", ALL_ADMIN).await;
    let pleb = common::create_local_user(pool, "pleb").await;
    let plain =
        common::create_app_and_token(pool, &pleb, "read write admin:read admin:write").await;
    let mod_user = common::create_local_user_with(pool, "mod", |u| NewUser {
        is_moderator: Flag::new(true),
        ..u
    })
    .await;
    let staff = common::create_app_and_token(pool, &mod_user, ALL_ADMIN).await;
    let app_only = common::create_app_only_token(pool, ALL_ADMIN).await;
    let (_reader, read_relays) = admin(pool, "reader", "admin:read:relays").await;
    let (_writer, write_relays) = admin(pool, "writer", "admin:write:relays").await;
    Tokens {
        all,
        plain,
        staff,
        app_only,
        read_relays,
        write_relays,
    }
}

/// One request to each of the five routes, for the walk of refusals.
fn routes() -> [fn() -> common::TestRequest; 5] {
    [
        || common::TestRequest::get(RELAY),
        || common::TestRequest::post(RELAY).json(&json!({"relay_url": "relay.example"})),
        || common::TestRequest::delete(RELAY).json(&json!({"relay_url": "relay.example"})),
        || common::TestRequest::delete(&format!("{INSTANCES}/peer.example")),
        || common::TestRequest::post(RELOAD),
    ]
}

/// The role half of the gate on every route: no credentials and an
/// unknown token are the 401s, an app-only token, a non-admin and a
/// moderator are the 403s — Pleroma's `UserIsAdminPlug` order, before
/// any scope is consulted.
#[tokio::test]
async fn every_federation_route_demands_an_admin_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let everyone = tokens(&pool).await;

    for route in routes() {
        let unauthenticated = route().send(&app).await;
        common::assert_answer(&unauthenticated, StatusCode::UNAUTHORIZED, "no token");
        common::assert_error(&unauthenticated, "Invalid credentials", "no token");

        let bogus = route().bearer("not-a-real-token").send(&app).await;
        common::assert_answer(&bogus, StatusCode::UNAUTHORIZED, "a bogus token");
        common::assert_error(&bogus, "The access token is invalid", "a bogus token");

        let unbound = route().bearer(&everyone.app_only).send(&app).await;
        common::assert_answer(&unbound, StatusCode::FORBIDDEN, "an app-only token");
        common::assert_error(
            &unbound,
            "This API requires an authenticated user",
            "an app-only token",
        );

        let non_admin = route().bearer(&everyone.plain).send(&app).await;
        common::assert_answer(&non_admin, StatusCode::FORBIDDEN, "a non-admin user");
        common::assert_error(&non_admin, "User is not an admin.", "a non-admin user");

        let moderator = route().bearer(&everyone.staff).send(&app).await;
        common::assert_answer(&moderator, StatusCode::FORBIDDEN, "a moderator");
        common::assert_error(&moderator, "User is not an admin.", "a moderator");
    }
}

/// The scope half of the gate: the wrong class on the right area, and
/// the right class on the wrong area, are 403s naming the class root.
#[tokio::test]
async fn the_federation_scopes_refuse_the_wrong_class_and_area() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let everyone = tokens(&pool).await;

    let write_with_read = common::TestRequest::post(RELAY)
        .bearer(&everyone.read_relays)
        .json(&json!({"relay_url": "nope/nope"}))
        .send(&app)
        .await;
    common::assert_answer(
        &write_with_read,
        StatusCode::FORBIDDEN,
        "a read scope on a write",
    );
    common::assert_error(
        &write_with_read,
        "Insufficient permissions: admin:write.",
        "a read scope on a write",
    );
    let read_with_write = common::TestRequest::get(RELAY)
        .bearer(&everyone.write_relays)
        .send(&app)
        .await;
    common::assert_answer(
        &read_with_write,
        StatusCode::FORBIDDEN,
        "a write scope on a read",
    );
    common::assert_error(
        &read_with_write,
        "Insufficient permissions: admin:read.",
        "a write scope on a read",
    );
    let relays_on_federation = common::TestRequest::delete(&format!("{INSTANCES}/peer.example"))
        .bearer(&everyone.write_relays)
        .send(&app)
        .await;
    common::assert_answer(
        &relays_on_federation,
        StatusCode::FORBIDDEN,
        "the wrong area",
    );
    common::assert_error(
        &relays_on_federation,
        "Insufficient permissions: admin:write.",
        "relay rights do not delete instances",
    );
    let relays_on_emoji = common::TestRequest::post(RELOAD)
        .bearer(&everyone.write_relays)
        .send(&app)
        .await;
    common::assert_answer(
        &relays_on_emoji,
        StatusCode::FORBIDDEN,
        "relays are not emoji",
    );
}

/// The grant half of the gate: each narrow area scope passes its own
/// routes — the relay read lists, the relay write follows for real —
/// and the class roots pass every route of their class.
#[tokio::test]
async fn the_federation_scopes_pass_their_own_routes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let everyone = tokens(&pool).await;
    let mock = MockRelay::start().await;

    let scoped_read = common::TestRequest::get(RELAY)
        .bearer(&everyone.read_relays)
        .send(&app)
        .await;
    common::assert_answer(
        &scoped_read,
        StatusCode::OK,
        "admin:read:relays on the ledger",
    );
    let scoped_follow = common::TestRequest::post(RELAY)
        .bearer(&everyone.write_relays)
        .json(&json!({"relay_url": mock.actor_ap_id()}))
        .send(&app)
        .await;
    common::assert_answer(
        &scoped_follow,
        StatusCode::OK,
        "admin:write:relays on the follow",
    );
    assert_eq!(
        queued_args(&pool, KIND_DELIVER).await.len(),
        1,
        "and the follow really queued its send"
    );
    let roots = common::TestRequest::post(RELOAD)
        .bearer(&everyone.all)
        .send(&app)
        .await;
    common::assert_answer(&roots, StatusCode::OK, "the admin roots on the reload");
}
