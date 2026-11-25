//! The move gate against a remote target whose cached mirror predates the
//! alias: the move refetches the target's actor document, and what the
//! peer serves now decides the alias precondition.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    extract::{Path, State},
    http::{StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse as _, Response},
    routing::get as route_get,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpListener, task::JoinHandle};
use wired_be::models::user::{NewUser, User, insert};

use super::support::{PASSWORD, fixture, move_account, move_v1, stored};
use crate::common;

/// The refusal both routes render when the target lacks the origin.
const ALIAS_REQUIRED: &str = "Target account must have the origin in `alsoKnownAs`";

/// A loopback peer serving one actor document whose `alsoKnownAs` and
/// status the test chose, counting the requests it answered.
struct Peer {
    /// The `http://{addr}` origin.
    base: String,
    /// Requests answered.
    hits: Arc<AtomicUsize>,
    /// The task serving it, aborted when the test is done.
    server: JoinHandle<()>,
}

/// What the peer's handler answers with.
#[derive(Clone)]
struct PeerState {
    /// The `http://{addr}` origin the document names itself with.
    base: String,
    /// The `alsoKnownAs` list the document carries.
    aliases: Vec<String>,
    /// The status of the answer; anything but 200 has no document.
    status: StatusCode,
    /// Requests answered.
    hits: Arc<AtomicUsize>,
}

/// Counts the hit and answers `nick`'s actor document (or the status).
async fn serve_actor(State(peer): State<PeerState>, Path(nick): Path<String>) -> Response {
    let _hits = peer.hits.fetch_add(1, Ordering::SeqCst);
    if peer.status != StatusCode::OK {
        return peer.status.into_response();
    }
    let id = format!("{}/users/{nick}", peer.base);
    let document = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Person",
        "id": id,
        "preferredUsername": nick,
        "name": "New home",
        "inbox": format!("{id}/inbox"),
        "alsoKnownAs": peer.aliases,
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

impl Peer {
    /// Starts a peer whose `newhome` actor lists `aliases` and answers
    /// `status`.
    async fn start(aliases: &[&str], status: StatusCode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the peer binds a port");
        let address = listener.local_addr().expect("a bound address");
        let state = PeerState {
            base: format!("http://{address}"),
            aliases: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
            status,
            hits: Arc::new(AtomicUsize::new(0)),
        };
        let (base, hits) = (state.base.clone(), Arc::clone(&state.hits));
        let server = tokio::spawn(async move {
            let app = Router::new()
                .route("/users/{nick}", route_get(serve_actor))
                .with_state(state);
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("peer stopped: {err}");
            }
        });
        Self { base, hits, server }
    }

    /// The actor id of `newhome` on this peer.
    fn actor_id(&self) -> String {
        format!("{}/users/newhome", self.base)
    }

    /// The requests answered so far.
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    /// Stores a mirror of `newhome` as the old instance cached it, with
    /// `cached_aliases` and a fresh fetch stamp (so only a forced refetch
    /// can change it).
    async fn mirror(&self, pool: &PgPool, cached_aliases: &[&str]) -> User {
        let authority = self
            .base
            .strip_prefix("http://")
            .expect("the peer origin is http loopback")
            .to_owned();
        let ap_id = self.actor_id();
        let new = NewUser {
            nickname: format!("newhome@{authority}"),
            ap_id: ap_id.clone(),
            domain: Some(authority),
            name: "New home".to_owned(),
            inbox: Some(format!("{ap_id}/inbox")),
            public_key: Some(
                "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n".to_owned(),
            ),
            also_known_as: cached_aliases
                .iter()
                .map(|alias| (*alias).to_owned())
                .collect(),
            ..NewUser::default()
        };
        let row = insert(pool, &new).await.expect("the mirror inserts");
        let _stamped = sqlx::query("UPDATE users SET last_fetched_at = now() WHERE id = $1")
            .bind(row.id)
            .execute(pool)
            .await
            .expect("the fetch stamp is set");
        let _mirrored =
            sqlx::query("UPDATE activitypub_actors SET last_fetched_at = now() WHERE user_id = $1")
                .bind(row.id)
                .execute(pool)
                .await
                .expect("the actor row's fetch stamp is set alike");
        stored(pool, row.id).await
    }
}

impl Drop for Peer {
    /// Stops the serving task.
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// The v1 body naming `target` by id.
fn by_id(target: &User) -> Value {
    json!({"to_account": target.id.to_string(), "password": PASSWORD})
}

/// A cached target that predates the alias moves once the peer serves the
/// alias, on both routes; the mirror is updated by the refetch.
#[tokio::test]
async fn a_stale_cached_target_is_refetched_before_the_alias_check() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let peer = Peer::start(&[alice.ap_id.as_str()], StatusCode::OK).await;
    let cached = peer.mirror(&pool, &[]).await;
    let app = common::test_app(pool.clone());

    let moved = move_account(&app, &token, PASSWORD, &cached.ap_id).await;
    assert_eq!(
        moved.json_value(),
        json!({"status": "success"}),
        "the refetched document lists the origin, so the move passes"
    );
    assert!(peer.hits() >= 1, "the peer was asked for the document");
    assert_eq!(
        stored(&pool, cached.id).await.also_known_as,
        vec![alice.ap_id.clone()],
        "the mirror now carries the alias"
    );
    assert_eq!(
        stored(&pool, alice.id).await.moved_to_ap_id.as_deref(),
        Some(cached.ap_id.as_str()),
        "and the account is marked moved"
    );
}

/// The Mastodon route refetches the same way.
#[tokio::test]
async fn the_mastodon_route_refetches_the_target_too() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let peer = Peer::start(&[alice.ap_id.as_str()], StatusCode::OK).await;
    let cached = peer.mirror(&pool, &[]).await;
    let app = common::test_app(pool.clone());

    let moved = move_v1(&app, &token, by_id(&cached)).await;
    assert_eq!(moved.status, StatusCode::OK, "the refetched alias passes");
    assert!(
        stored(&pool, alice.id).await.last_move_at.is_some(),
        "the account moved"
    );
}

/// When the served document still lacks the alias, the refetch does not
/// rescue the move: both routes refuse with the alias error.
#[tokio::test]
async fn a_refetched_target_without_the_alias_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let peer = Peer::start(&["https://elsewhere.example/users/x"], StatusCode::OK).await;
    let cached = peer.mirror(&pool, &[]).await;
    let app = common::test_app(pool.clone());

    let pleroma = move_account(&app, &token, PASSWORD, &cached.ap_id).await;
    assert_eq!(
        pleroma.json_value(),
        json!({"error": ALIAS_REQUIRED}),
        "Pleroma route"
    );
    let mastodon = move_v1(&app, &token, by_id(&cached)).await;
    assert_eq!(mastodon.status, StatusCode::UNPROCESSABLE_ENTITY, "422");
    assert_eq!(
        mastodon.json_value(),
        json!({"error": ALIAS_REQUIRED}),
        "Mastodon route"
    );
    assert!(peer.hits() >= 2, "each attempt refetched");
    assert!(
        stored(&pool, alice.id).await.last_move_at.is_none(),
        "nothing was marked moved"
    );
}

/// A transient refetch failure keeps the cached mirror as the answer: a
/// mirror that already lists the origin still moves.
#[tokio::test]
async fn a_failed_refetch_falls_back_to_the_cached_target() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let peer = Peer::start(&[], StatusCode::INTERNAL_SERVER_ERROR).await;
    let cached = peer.mirror(&pool, &[alice.ap_id.as_str()]).await;
    let app = common::test_app(pool.clone());

    let moved = move_account(&app, &token, PASSWORD, &cached.ap_id).await;
    assert_eq!(
        moved.json_value(),
        json!({"status": "success"}),
        "the cached alias list answers when the peer errors"
    );
    assert!(peer.hits() >= 1, "the refetch was attempted");
}

/// A target its host reports gone is the endpoint's not-found, not a move
/// on a stale mirror.
#[tokio::test]
async fn a_gone_target_is_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let peer = Peer::start(&[], StatusCode::GONE).await;
    let cached = peer.mirror(&pool, &[alice.ap_id.as_str()]).await;
    let app = common::test_app(pool.clone());

    let pleroma = move_account(&app, &token, PASSWORD, &cached.ap_id).await;
    assert_eq!(pleroma.status, StatusCode::NOT_FOUND, "404");
    assert_eq!(
        pleroma.json_value(),
        json!({"error": "Target account not found."}),
        "the endpoint's own message"
    );
    let mastodon = move_v1(&app, &token, by_id(&cached)).await;
    assert_eq!(mastodon.status, StatusCode::NOT_FOUND, "the twin agrees");
    assert!(
        stored(&pool, alice.id).await.last_move_at.is_none(),
        "nothing was marked moved"
    );
}
