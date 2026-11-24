//! The delivery helpers, fixtures and mock peer the inbound interaction tests
//! share. The test modules are its children, so they reach every helper
//! through `use super::*`.

mod announces;
mod likes_reactions;
mod visibility;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Router, response::Response};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow,
        notification::{TYPE_EMOJI_REACTION, TYPE_FAVOURITE, TYPE_REBLOG},
        status::{self, NewStatus, Status, Visibility},
        user::User,
    },
};

use crate::common::{self, deliver, delivering_state};

/// The remote actors the fixtures deliver as, and the public address.
const BOB: &str = "https://remote.example/users/bob";
const MALLORY: &str = "https://remote.example/users/mallory";
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

// --- the delivered activities --------------------------------------------------

/// An interaction of `kind` by `actor` against `object`, ids named by `n`,
/// with the activity fields `extra` merged over the fixture.
fn interaction(kind: &str, actor: &str, object: &Value, n: u32, extra: Value) -> Value {
    let host = actor.split('/').nth(2).expect("the fixture actor is a url");
    let act = json!({
        "id": format!("https://{host}/activities/{}-{n}", kind.to_lowercase()),
        "type": kind,
        "actor": actor,
        "object": object,
    });
    merge(act, extra)
}

/// A wrapped-inner `Undo` of `inner` by `actor`.
fn undo(actor: &str, inner: &Value, n: u32) -> Value {
    let host = actor.split('/').nth(2).expect("the fixture actor is a url");
    json!({
        "id": format!("https://{host}/activities/undo-{n}"),
        "type": "Undo",
        "actor": actor,
        "object": inner,
    })
}

/// The activity id [`interaction`] would mint for this kind, actor and `n`.
fn interaction_id(kind: &str, actor: &str, n: u32) -> String {
    let host = actor.split('/').nth(2).expect("the fixture actor is a url");
    format!("https://{host}/activities/{}-{n}", kind.to_lowercase())
}

/// The fixture status's federation-facing id, what every fold resolves.
fn target_of(post: &Status) -> Value {
    json!(post.uri)
}

/// Merge `extra`'s fields over `document`.
fn merge(document: Value, extra: Value) -> Value {
    let Value::Object(fields) = extra else {
        return document;
    };
    let mut map = document.as_object().expect("the fixture is a map").clone();
    map.extend(fields);
    Value::Object(map)
}

// --- reading the state back -----------------------------------------------------

/// A read token for `who` and an app to spend it on.
struct Viewer {
    app: Router,
    token: String,
}

async fn viewer(pool: &PgPool, who: &User) -> Viewer {
    Viewer {
        app: common::test_app(pool.clone()),
        token: common::create_app_and_token(pool, who, "read").await,
    }
}

impl Viewer {
    /// `GET /api/v1/statuses/:id` as this viewer.
    async fn status(&self, id: Uuid) -> Value {
        let response = common::TestRequest::get(&format!("/api/v1/statuses/{id}"))
            .bearer(&self.token)
            .send(&self.app)
            .await;
        assert_eq!(response.status, StatusCode::OK, "reading the status");
        response.json_value()
    }

    /// `GET /api/v1/notifications` as this viewer.
    async fn notifications(&self) -> Vec<Value> {
        let response = common::TestRequest::get("/api/v1/notifications")
            .bearer(&self.token)
            .send(&self.app)
            .await;
        assert_eq!(response.status, StatusCode::OK, "reading notifications");
        response.json_value().as_array().expect("an array").clone()
    }

    /// This viewer's notifications of `kind`, in delivery order.
    async fn told_of(&self, kind: &str) -> Vec<Value> {
        self.notifications()
            .await
            .into_iter()
            .filter(|n| n["type"] == json!(kind))
            .collect()
    }
}

/// `(reactor ap_id, status uri, activity id)` for every favourite row.
async fn favourite_rows(pool: &PgPool) -> Vec<(String, String, String)> {
    sqlx::query_as::<_, (String, String, String)>(
        "SELECT u.ap_id, s.uri, f.ap_id FROM favourites f \
         JOIN users u ON u.id = f.user_id \
         JOIN statuses s ON s.id = f.status_id ORDER BY f.inserted_at, f.id",
    )
    .fetch_all(pool)
    .await
    .expect("the favourites read")
}

/// `(reactor ap_id, status uri, emoji, url, activity id)` per reaction row.
async fn reaction_rows(pool: &PgPool) -> Vec<(String, String, String, Option<String>, String)> {
    sqlx::query_as::<_, (String, String, String, Option<String>, String)>(
        "SELECT u.ap_id, s.uri, r.emoji, r.url, r.ap_id FROM emoji_reactions r \
         JOIN users u ON u.id = r.user_id \
         JOIN statuses s ON s.id = r.status_id ORDER BY r.inserted_at, r.id",
    )
    .fetch_all(pool)
    .await
    .expect("the reactions read")
}

/// `(uri, visibility, inserted-at day, original uri, booster ap_id)` per
/// live boost row, oldest first.
async fn boost_rows(pool: &PgPool) -> Vec<(String, String, String, String, String)> {
    sqlx::query_as::<_, (String, String, String, String, String)>(
        "SELECT s.uri, s.visibility, to_char(s.inserted_at AT TIME ZONE 'UTC', 'YYYY-MM-DD'), \
         o.uri, u.ap_id \
         FROM statuses s \
         JOIN statuses o ON o.id = s.reblog_of_id \
         JOIN users u ON u.id = s.user_id \
         WHERE s.deleted_at IS NULL ORDER BY s.inserted_at, s.id",
    )
    .fetch_all(pool)
    .await
    .expect("the boosts read")
}

/// Alice and her fresh public `text`: the interaction target of most tests.
async fn alice_post(pool: &PgPool, text: &str) -> (User, Status) {
    let alice = common::create_local_user(pool, "alice").await;
    let posted = common::create_status(pool, &alice, text, Visibility::Public).await;
    (alice, posted)
}

// --- the mock peer ------------------------------------------------------------------

/// The mock peer of the fetch test: alice's note at `/objects/n1`, its
/// author at `/users/nobody`, and a 404 at `/missing`. Serves on a loopback
/// port; the stock test state's client is allowed to reach it.
async fn spawn_peer() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the loopback peer binds");
    let base = format!("http://{}", listener.local_addr().expect("an addr"));
    let app = Router::new()
        .route("/objects/{name}", get(serve_object))
        .route("/users/{name}", get(serve_actor))
        .route("/missing", get(|| async { StatusCode::NOT_FOUND }))
        .with_state(base.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("the peer serves");
    });
    (base, server)
}

/// A self-naming remote actor document: enough for [`fetch_actor`]-style
/// ingest (the fixture's public key is inert — ingest stores, it does not
/// verify).
///
/// [`fetch_actor`]: wired_be::federation::fetch_actor
async fn serve_actor(State(base): State<String>, Path(name): Path<String>) -> Response {
    let id = format!("{base}/users/{name}");
    common::activity_json(&json!({
        "type": "Person",
        "id": id,
        "preferredUsername": name,
        "inbox": format!("{id}/inbox"),
        "followers": format!("{id}/followers"),
        "publicKey": {"id": format!("{id}#main-key"), "owner": id, "publicKeyPem": "PEM"},
    }))
}

/// The note the interaction names: public, authored by `nobody` at this
/// same base.
async fn serve_object(State(base): State<String>, Path(name): Path<String>) -> Response {
    common::activity_json(&json!({
        "id": format!("{base}/objects/{name}"),
        "type": "Note",
        "attributedTo": format!("{base}/users/nobody"),
        "to": [PUBLIC],
        "content": "<p>a note from the peer</p>",
        "published": "2026-11-02T00:00:00Z",
    }))
}
