//! `GET /api/v1/accounts/{id}` with a handle in place of the id: a stored
//! nickname, `nick@LOCAL_DOMAIN` and a stored remote `nick@host` answer the
//! same body as the id, a miss is the plain 404 without any outbound
//! request, and the visibility, restriction and scope rules are the id
//! path's.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{Router, extract::State, http::StatusCode};
use tokio::{net::TcpListener, task::JoinHandle};
use wired_be::models::user::Flag;

use super::support::{NO_SCOPE, NOT_FOUND, USER_REQUIRED, show};
use crate::common;

/// A loopback peer that answers every request `404` and counts them: a
/// handle that was fetched rather than read from the database shows up in
/// [`CountingPeer::hits`].
struct CountingPeer {
    /// The `host:port` authority a handle on this peer would carry.
    authority: String,
    /// Requests received, whatever the path.
    hits: Arc<AtomicUsize>,
    /// The task serving it, aborted when the test is done.
    server: JoinHandle<()>,
}

/// Counts the request and answers `404`, whatever the path.
async fn count_hit(State(counter): State<Arc<AtomicUsize>>) -> StatusCode {
    let _seen = counter.fetch_add(1, Ordering::SeqCst);
    StatusCode::NOT_FOUND
}

impl CountingPeer {
    /// Binds a loopback port and starts counting.
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock peer binds a port");
        let authority = listener.local_addr().expect("a bound address").to_string();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let server = tokio::spawn(async move {
            let app = Router::new().fallback(count_hit).with_state(counter);
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("counting peer stopped: {err}");
            }
        });
        Self {
            authority,
            hits,
            server,
        }
    }

    /// The requests received so far.
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for CountingPeer {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// A local nickname answers 200 with exactly the body the id does.
#[tokio::test]
async fn show_resolves_a_local_nickname_like_its_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool);

    let by_id = show(&app, &alice.id.to_string()).await;
    assert_eq!(by_id.status, StatusCode::OK, "the id path is unchanged");
    for handle in ["alice", "ALICE", "alice@test.example"] {
        let by_handle = show(&app, handle).await;
        assert_eq!(by_handle.status, StatusCode::OK, "`{handle}` resolves");
        assert_eq!(
            by_handle.json_value(),
            by_id.json_value(),
            "`{handle}` renders the same account as its id"
        );
    }
}

/// On a split-domain config both the handle domain and the web host strip
/// to the local nickname.
#[tokio::test]
async fn show_resolves_a_nickname_on_either_domain_of_a_split_config() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mut config = common::test_config();
    config.local_domain = "whatever.com".to_owned();
    let app = common::test_app_with_config(pool, config);

    let by_id = show(&app, &alice.id.to_string()).await.json_value();
    for handle in ["alice@whatever.com", "alice@test.example"] {
        let response = show(&app, handle).await;
        common::assert_answer(&response, StatusCode::OK, handle);
        assert_eq!(response.json_value(), by_id, "`{handle}` is the same row");
    }
    let foreign = show(&app, "alice@other.example").await;
    assert_eq!(
        foreign.status,
        StatusCode::NOT_FOUND,
        "a domain that is not ours names no local account"
    );
}

/// A stored remote `nick@host` answers 200 with the id path's body.
#[tokio::test]
async fn show_resolves_a_stored_remote_handle() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let app = common::test_app(pool);

    let by_id = show(&app, &bob.id.to_string()).await.json_value();
    let response = show(&app, "bob@remote.example").await;
    common::assert_answer(&response, StatusCode::OK, "the stored remote handle");
    assert_eq!(response.json_value(), by_id, "the same mirror as its id");
}

/// A handle nobody stored is a 404 and nothing is asked of the network, not
/// even for a host that would answer.
#[tokio::test]
async fn show_an_unknown_handle_is_a_404_without_a_fetch() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let peer = CountingPeer::start().await;
    let app = common::test_app(pool);

    for handle in [
        "nobody".to_owned(),
        "nobody@test.example".to_owned(),
        format!("ghost@{}", peer.authority),
    ] {
        let anonymous = show(&app, &handle).await;
        assert_eq!(anonymous.status, StatusCode::NOT_FOUND, "`{handle}`");
        assert_eq!(anonymous.text(), NOT_FOUND, "the plain miss body");
        let signed = common::TestRequest::get(&format!("/api/v1/accounts/{handle}"))
            .bearer(&token)
            .send(&app)
            .await;
        assert_eq!(signed.status, StatusCode::NOT_FOUND, "`{handle}` signed in");
        assert_eq!(signed.text(), NOT_FOUND, "the plain miss body, signed in");
    }
    assert_eq!(peer.hits(), 0, "no WebFinger, no actor fetch");
}

/// A deactivated account is hidden by nickname as it is by id.
#[tokio::test]
async fn show_a_deactivated_account_by_nickname_is_a_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let ghost = common::create_local_user_with(&pool, "ghost", |mut new| {
        new.is_active = Flag::new(false);
        new
    })
    .await;
    let app = common::test_app(pool);

    let by_id = show(&app, &ghost.id.to_string()).await;
    let by_nick = show(&app, "ghost").await;
    assert_eq!(by_nick.status, StatusCode::NOT_FOUND, "hidden by nickname");
    assert_eq!(by_nick.text(), NOT_FOUND, "the plain miss body");
    assert_eq!(by_nick.text(), by_id.text(), "the id path's answer");
}

/// An account awaiting approval is invisible to a stranger by nickname, as
/// by id.
#[tokio::test]
async fn show_an_unapproved_account_by_nickname_is_a_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let pending = common::create_local_user_with(&pool, "pending", |mut new| {
        new.approval_pending = Flag::new(true);
        new
    })
    .await;
    let app = common::test_app(pool);

    let by_id = show(&app, &pending.id.to_string()).await;
    let by_nick = show(&app, "pending").await;
    assert_eq!(by_nick.status, by_id.status, "the same status as by id");
    assert_eq!(by_nick.status, StatusCode::NOT_FOUND, "hidden by nickname");
    assert_eq!(by_nick.text(), NOT_FOUND, "the plain miss body");
}

/// A closed profile half refuses an anonymous handle request with the id
/// path's 403, and a user token still reads it.
#[tokio::test]
async fn show_by_handle_keeps_the_restricted_unauthenticated_refusal() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app = common::test_app_with_restricted_areas(pool, &["profiles_local", "profiles_remote"]);

    for (handle, id) in [
        ("alice", alice.id.to_string()),
        ("bob@remote.example", bob.id.to_string()),
    ] {
        let by_id = show(&app, &id).await;
        let by_handle = show(&app, handle).await;
        assert_eq!(by_handle.status, StatusCode::FORBIDDEN, "`{handle}`");
        assert_eq!(
            by_handle.text(),
            USER_REQUIRED,
            "the authenticated-user 403"
        );
        assert_eq!(by_handle.text(), by_id.text(), "the id path's body");

        let signed = common::TestRequest::get(&format!("/api/v1/accounts/{handle}"))
            .bearer(&token)
            .send(&app)
            .await;
        assert_eq!(signed.status, StatusCode::OK, "`{handle}` with a token");
    }
}

/// A user token without `read:accounts` is refused before the handle is
/// read; one with `read` resolves it.
#[tokio::test]
async fn show_by_handle_requires_read_accounts_on_a_presented_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let narrow = common::create_app_and_token(&pool, &alice, "write:statuses").await;
    let broad = common::create_app_and_token(&pool, &alice, "read").await;
    let app = common::test_app(pool);

    for handle in ["alice", "nobody"] {
        let refused = common::TestRequest::get(&format!("/api/v1/accounts/{handle}"))
            .bearer(&narrow)
            .send(&app)
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "`{handle}`");
        assert_eq!(refused.text(), NO_SCOPE, "Pleroma's wording");
    }
    let allowed = common::TestRequest::get("/api/v1/accounts/alice")
        .bearer(&broad)
        .send(&app)
        .await;
    assert_eq!(
        allowed.status,
        StatusCode::OK,
        "`read` covers read:accounts"
    );
}
