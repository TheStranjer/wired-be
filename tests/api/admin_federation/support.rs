//! The mock relay server and the helpers every admin federation test shares.

use std::sync::{Arc, Mutex};

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::Response,
    routing::{get, post},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpListener, task::JoinHandle};
use wired_be::{
    config::TEST_BASE_URL,
    federation::delivery::KIND_DELIVER,
    models::{
        follow, relay,
        user::{Flag, NewUser, User},
    },
    services::instance_actor::ensure_instance_actor,
    state::AppState,
};

use crate::common;

/// The relay ledger route.
pub(super) const RELAY: &str = "/api/v1/pleroma/admin/relay";
/// The instance-deletion route prefix.
pub(super) const INSTANCES: &str = "/api/v1/pleroma/admin/instances";
/// The emoji reload route.
pub(super) const RELOAD: &str = "/api/v1/pleroma/admin/reload_emoji";
/// The admin scopes both halves of every gate here accept, as roots.
pub(super) const ALL_ADMIN: &str = "admin:read admin:write";
/// Bytes for a fixture pack's image files.
pub(super) const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nfake";

// --- the mock relay -------------------------------------------------------------------

/// The mock's shared state: its origin and the activities its inbox
/// recorded.
#[derive(Clone, Debug)]
pub(super) struct Mock {
    /// The `http://{addr}` origin this relay answers on.
    pub(super) base: String,
    /// The `POST` bodies its inbox recorded, oldest first.
    pub(super) posts: Arc<Mutex<Vec<Value>>>,
}

/// The running mock relay; dropping it stops the server.
#[derive(Debug)]
pub(super) struct MockRelay {
    /// The handlers' shared state.
    pub(super) mock: Mock,
    /// The task serving it, aborted on drop.
    pub(super) server: JoinHandle<()>,
}

impl MockRelay {
    /// Serves the relay actor, a `Group` actor, a 404 path and a
    /// capturing inbox on a fresh loopback port.
    pub(super) async fn start() -> Self {
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
    pub(super) fn actor_ap_id(&self) -> String {
        format!("{}/relay", self.mock.base)
    }

    /// The relay actor's inbox URL on this mock.
    pub(super) fn inbox_url(&self) -> String {
        format!("{}/relay/inbox", self.mock.base)
    }

    /// Any other origin path, for the refusal cases.
    pub(super) fn url_at(&self, path: &str) -> String {
        format!("{}{path}", self.mock.base)
    }

    /// The bodies the inbox recorded, oldest first.
    pub(super) fn received(&self) -> Vec<Value> {
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
pub(super) async fn relay_actor(State(mock): State<Mock>) -> Response {
    common::activity_json(&actor_doc(&mock, "relay", "Application"))
}

/// Not a relay: a `Group` is outside the three followable shapes.
pub(super) async fn group_actor(State(mock): State<Mock>) -> Response {
    common::activity_json(&actor_doc(&mock, "group", "Group"))
}

/// A self-naming actor document of `kind` at `{base}/{path}`.
pub(super) fn actor_doc(mock: &Mock, path: &str, kind: &str) -> Value {
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
pub(super) async fn capture(State(mock): State<Mock>, body: axum::body::Bytes) -> StatusCode {
    let parsed = serde_json::from_slice(&body).unwrap_or(Value::Null);
    mock.posts.lock().expect("the capture lock").push(parsed);
    StatusCode::ACCEPTED
}

// --- shared fixtures and reads --------------------------------------------------------

/// The router and its state over the stock test config (the HTTP client
/// reaches the loopback mock, and the job drain rides the same state).
pub(super) fn federation_app(pool: &PgPool) -> (Router, AppState) {
    let state = common::test_app_state(pool.clone(), common::test_config());
    (wired_be::app(state.clone()), state)
}

/// A local admin named `nickname` with a bearer token granting `scopes`.
pub(super) async fn admin(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
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
pub(super) async fn log_entry(pool: &PgPool, action: &str) -> (String, Value) {
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
pub(super) async fn queued_args(pool: &PgPool, kind: &str) -> Vec<Value> {
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
pub(super) async fn follow_back(pool: &PgPool, relay_user: &User) {
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
pub(super) async fn post_relay(
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
pub(super) async fn assert_follow_recorded(pool: &PgPool, mock: &MockRelay) {
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
pub(super) async fn assert_relay_withdrawn(pool: &PgPool, relay_user: &User, actor_ap_id: &str) {
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
