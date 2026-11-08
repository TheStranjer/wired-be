//! Integration tests for `POST /api/v1/follows` (WBE-165): the legacy
//! Mastodon follow write keyed by `uri` instead of by id. A handle —
//! `user@domain`, `@user@domain`, bare `nick` or one spelled with this
//! instance's own domain — resolves through
//! [`get_or_fetch_by_acct`](wired_be::federation::fetch_actor::get_or_fetch_by_acct)
//! (stored rows first), and an actor URL through
//! [`get_or_fetch_by_ap_id`](wired_be::federation::fetch_actor::get_or_fetch_by_ap_id),
//! which fetches and mirrors an unseen one from a mock peer over loopback.
//! Whatever the `uri` names, the write is `POST …/accounts/{id}/follow`'s:
//! the same granted/requested/self/blocked outcomes, the same toggles from
//! the query string, a form or a JSON body, and the same
//! `write:follows | follow` gate in the limiter's `relations_actions`
//! bucket. The answer is the target's `Account` entity rendered for the
//! caller — their relationship embedded in `pleroma.relationship` — not a
//! bare `Relationship`. An absent, blank or `null` `uri` is the 400
//! "Missing parameter: uri"; a `uri` resolving to no live account is the
//! shared 404 "Record not found".

#![cfg(test)]

use axum::{
    Router,
    extract::State,
    http::{StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse as _, Response},
    routing::get,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpListener, task::JoinHandle};
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, Follow, FollowState},
        user::{Flag, User, get_by_ap_id, get_by_id},
    },
};

use crate::common;

/// The endpoint under test.
const ROUTE: &str = "/api/v1/follows";
/// The id route that shares the bucket, for the mixed-budget test.
fn follow_by_id(target: &User) -> String {
    format!("/api/v1/accounts/{}/follow", target.id)
}
/// The 404 body every account route shares (decision 8).
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;
/// The 400 body of the absent parameter.
const MISSING_URI: &str = r#"{"error":"Missing parameter: uri"}"#;
/// The 401 body `AuthUser` renders when no token was presented at all.
const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body for a token without either accepted scope.
const NO_SCOPE: &str = r#"{"error":"Insufficient permissions: write:follows | follow."}"#;
/// The 422 body of the self guard, the model's (Pleroma's) wording.
const SELF_FOLLOW: &str = r#"{"error":"Can not follow yourself"}"#;
/// The 429 body of the spent bucket.
const THROTTLED: &str = r#"{"error":"Throttled"}"#;
/// A handle `WebFinger` refuses before any request (single-label domain),
/// so the unknown-target tests stay network-free.
const UNRESOLVABLE: &str = "ghost@invalid";

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// The row of user `id` as it stands now, for reading counters back.
async fn fresh_user(pool: &PgPool, id: uuid::Uuid) -> User {
    get_by_id(pool, id).await.unwrap().expect("the user exists")
}

/// The stored relationship of the pair, which the test expects to exist.
async fn relation(pool: &PgPool, follower: &User, target: &User) -> Follow {
    follow::get(pool, follower.id, target.id)
        .await
        .unwrap()
        .expect("the relationship exists")
}

/// The number of `follows` rows in the table, for the refusals that must
/// have written nothing.
async fn row_count(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM follows")
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// `POST`s the follows route as `token` with `uri` as a JSON body.
async fn follow_uri(app: &Router, token: &str, uri: &str) -> common::TestResponse {
    common::TestRequest::post(ROUTE)
        .bearer(token)
        .json(&json!({"uri": uri}))
        .send(app)
        .await
}

/// The caller's relationship embedded in a rendered account.
fn embedded_relationship(body: &Value) -> &Value {
    &body["pleroma"]["relationship"]
}

/// Asserts the answered entity is `target`'s account, and that
/// `relationship` carries the caller's flags over it: `flags` is the
/// expected `(following, requested)` pair as the write left them.
fn assert_followed_account(response: &common::TestResponse, target: &User, flags: (bool, bool)) {
    let (following, requested) = flags;
    assert_eq!(response.status, StatusCode::OK, "the follow is answered");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the account is JSON"
    );
    let body = response.json_value();
    assert_eq!(body["id"], target.id.to_string(), "the target's account");
    assert_eq!(body["uri"], target.ap_id, "`uri` is the actor id");
    assert_eq!(
        body["username"],
        target.nickname.split('@').next().unwrap_or_default(),
        "`username` is the nickname part"
    );
    let relationship = embedded_relationship(&body);
    assert_eq!(relationship["id"], target.id.to_string(), "one's own");
    assert_eq!(
        (
            relationship["following"].as_bool(),
            relationship["requested"].as_bool(),
        ),
        (Some(following), Some(requested)),
        "the answer states what the write stored"
    );
}

/// Following a local account answers its `Account` entity, granted on the
/// spot, from every handle spelling: the bare nickname, the `@`-prefixed
/// one, and the one spelled with this instance's own domain (the
/// `get_or_fetch_by_acct` stored-row branch, with and without prefixes).
#[tokio::test]
async fn follows_local_accounts_by_handle_forms() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:follows").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let erin = common::create_local_user(&pool, "erin").await;
    let app = common::test_app(pool.clone());

    for (who, uri, target) in [
        ("bare nick", "bob", &bob),
        ("@-prefixed", "@dave", &dave),
        ("own domain", "erin@test.example", &erin),
    ] {
        let response = follow_uri(&app, &token, uri).await;
        assert_eq!(response.status, StatusCode::OK, "the {who} form follows");
        assert_followed_account(&response, target, (true, false));
        assert_eq!(
            response.json_value()["acct"],
            target.nickname,
            "a local account's `acct` is its stored nickname"
        );
        assert_eq!(
            response.json_value()["fqn"],
            format!("{}@test.example", target.nickname),
            "and `fqn` carries this instance's domain"
        );
        assert_eq!(
            relation(&pool, &alice, target).await.state,
            FollowState::Accepted,
            "the {who} follow was granted"
        );
    }
    assert_eq!(
        fresh_user(&pool, alice.id).await.following_count,
        3,
        "three follows, three counts"
    );
}

/// The URL form on a local actor id resolves through the stored row
/// without any fetch and follows exactly like the handle forms.
#[tokio::test]
async fn follows_a_local_account_by_actor_url() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "follow").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let response = follow_uri(&app, &token, &bob.ap_id).await;
    assert_followed_account(&response, &bob, (true, false));
    assert_eq!(
        (
            fresh_user(&pool, bob.id).await.followers_count,
            fresh_user(&pool, alice.id).await.following_count,
        ),
        (1, 1),
        "both counters moved"
    );
}

/// A mirrored remote account resolves from the stored mirror by handle,
/// with the `@` prefix folded away, and the follow waits: `requested`,
/// not `following`, with nothing counted.
#[tokio::test]
async fn follows_a_mirrored_remote_account_by_handle() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "follow").await;
    let erin = common::create_remote_user(&pool, "erin", "remote.example").await;
    let app = common::test_app(pool.clone());

    let response = follow_uri(&app, &token, "@erin@remote.example").await;
    assert_followed_account(&response, &erin, (false, true));
    assert_eq!(
        response.json_value()["acct"],
        "erin@remote.example",
        "the mirror's full handle"
    );
    assert_eq!(
        relation(&pool, &alice, &erin).await.state,
        FollowState::Pending,
        "the answer belongs to `remote.example`"
    );
    assert_eq!(
        (
            fresh_user(&pool, erin.id).await.followers_count,
            fresh_user(&pool, alice.id).await.following_count,
        ),
        (0, 0),
        "a waiting request counts for nothing"
    );
}

/// A mock `ActivityPub` peer on loopback, serving one actor document.
struct MockPeer {
    /// The `http://{addr}` origin it answers on.
    base: String,
    /// The task serving it, aborted when the test is done.
    server: JoinHandle<()>,
}

/// The document the peer serves for `id`, the way a Mastodon peer serves
/// one: everything the actor parser requires is present.
fn actor_document(id: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Person",
        "id": id,
        "preferredUsername": "zoe",
        "name": "Zoe",
        "inbox": format!("{id}/inbox"),
        "publicKey": {
            "id": format!("{id}#main-key"),
            "owner": id,
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
        },
    })
}

/// Answers the self-identifying actor document.
async fn actor(State(document): State<String>) -> Response {
    ([(CONTENT_TYPE, "application/activity+json")], document).into_response()
}

impl MockPeer {
    /// Binds a port and serves `zoe`'s actor at `{base}/users/zoe`.
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock peer binds a port");
        let base = format!("http://{}", listener.local_addr().expect("a bound address"));
        let document = actor_document(&format!("{base}/users/zoe")).to_string();
        let server = tokio::spawn(async move {
            let app = Router::new()
                .route("/users/zoe", get(actor))
                .with_state(document);
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock peer stopped: {err}");
            }
        });
        Self { base, server }
    }

    /// The actor URL this peer serves.
    fn actor_url(&self) -> String {
        format!("{}/users/zoe", self.base)
    }
}

impl Drop for MockPeer {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// The URL form on an actor this instance has never seen runs the full
/// federation round trip: the mock peer's document is fetched, mirrored,
/// and the mirror is followed (pending, its answer not ours to give), and
/// the followed account comes back as the entity.
#[tokio::test]
async fn follows_a_remote_account_fetched_from_a_mock_actor_server() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:follows").await;
    let peer = MockPeer::start().await;
    let url = peer.actor_url();
    let app = common::test_app(pool.clone());

    let response = follow_uri(&app, &token, &url).await;
    let mirror = get_by_ap_id(&pool, &url)
        .await
        .unwrap()
        .expect("the fetched actor is mirrored");
    assert_followed_account(&response, &mirror, (false, true));
    assert_eq!(
        response.json_value()["username"],
        "zoe",
        "from its document"
    );
    assert_eq!(
        response.json_value()["acct"],
        mirror.nickname,
        "under the `user@host` handle decision 7 stores"
    );
    assert_eq!(
        relation(&pool, &alice, &mirror).await.state,
        FollowState::Pending,
        "a remote follow waits for the remote answer"
    );
}

/// Every `uri` that names no live account is the shared 404 — a handle
/// `WebFinger` refuses before any request, an unseen bare nickname with no
/// stored row, an actor URL on this instance's own origin that has no row,
/// and the handle of a deactivated account — and none of them writes.
#[tokio::test]
async fn unknown_targets_are_the_shared_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "follow").await;
    let ghost = common::create_local_user_with(&pool, "ghost", |mut new| {
        new.is_active = Flag::new(false);
        new
    })
    .await;
    let app = common::test_app(pool.clone());

    for uri in [
        UNRESOLVABLE,
        "nobody",
        "@nobody@elsewhere.invalid",
        &format!("{TEST_BASE_URL}/users/phantom"),
        "ghost",
        ghost.ap_id.as_str(),
    ] {
        let response = follow_uri(&app, &token, uri).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "`{uri}` resolves to no live account"
        );
        assert_eq!(response.text(), NOT_FOUND, "decision 8's 404 body");
    }
    assert_eq!(row_count(&pool).await, 0, "the six 404s wrote nothing");
    assert_eq!(
        fresh_user(&pool, alice.id).await.following_count,
        0,
        "and counted nothing"
    );
}

/// A request whose body carries no usable `uri` — none, an empty object,
/// a JSON `null`, blank text, the empty form spelling — is the 400
/// "Missing parameter: uri", before any resolution is ever attempted.
#[tokio::test]
async fn a_missing_uri_is_the_documented_400() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "follow").await;
    let app = common::test_app(pool.clone());

    let bare = common::TestRequest::post(ROUTE)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(bare.status, StatusCode::BAD_REQUEST, "no body at all");
    assert_eq!(bare.text(), MISSING_URI, "the Mastodon wording");

    for body in [
        json!({}),
        json!({"uri": null}),
        json!({"uri": ""}),
        json!({"uri": "   "}),
    ] {
        let response = common::TestRequest::post(ROUTE)
            .bearer(&token)
            .json(&body)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "`{body}` carries no uri"
        );
        assert_eq!(response.text(), MISSING_URI, "the Mastodon wording");
    }

    let form = common::TestRequest::post(ROUTE)
        .bearer(&token)
        .form(&[("uri", "")])
        .send(&app)
        .await;
    assert_eq!(form.status, StatusCode::BAD_REQUEST, "the empty form value");
    assert_eq!(form.text(), MISSING_URI, "is the same 400");
    assert_eq!(row_count(&pool).await, 0, "and none of them wrote a row");
}

/// The `uri` naming the caller's own account — by nickname, with the `@`,
/// by full local handle or by actor URL — is the model's 422, exactly as
/// on the id-keyed route; nothing is filed.
#[tokio::test]
async fn following_yourself_by_uri_is_the_documented_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:follows").await;
    let app = common::test_app(pool.clone());

    for uri in ["alice", "@alice", "alice@test.example", &alice.ap_id] {
        let response = follow_uri(&app, &token, uri).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{uri}` names the caller"
        );
        assert_eq!(response.text(), SELF_FOLLOW, "the model's wording");
    }
    assert_eq!(row_count(&pool).await, 0, "and neither refusal wrote a row");
}

/// A locked local target files a request instead of a grant: `requested`,
/// the waiting row, nothing counted — the model's own answer, reached
/// through the handle form.
#[tokio::test]
async fn a_locked_target_waits_as_a_request() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:follows").await;
    let carol = common::create_local_user_with(&pool, "carol", |mut new| {
        new.locked = Flag::new(true);
        new
    })
    .await;
    let app = common::test_app(pool.clone());

    let response = follow_uri(&app, &token, "@carol").await;
    assert_followed_account(&response, &carol, (false, true));
    assert_eq!(
        relation(&pool, &alice, &carol).await.state,
        FollowState::Pending,
        "the row waits in the queue"
    );
    assert_eq!(
        (
            fresh_user(&pool, carol.id).await.followers_count,
            fresh_user(&pool, alice.id).await.following_count,
        ),
        (0, 0),
        "a request counts for nothing"
    );
}

/// The two toggles ride the `uri` spelling from every shape the extractor
/// speaks, and are stored on the created row exactly as on the id-keyed
/// route.
#[tokio::test]
async fn the_toggles_arrive_with_the_uri_in_any_shape() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "follow").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let app = common::test_app(pool.clone());

    let form = common::TestRequest::post(ROUTE)
        .bearer(&token)
        .form(&[("uri", "bob"), ("reblogs", "false"), ("notify", "true")])
        .send(&app)
        .await;
    assert_eq!(form.status, StatusCode::OK, "the form body is accepted");
    assert_eq!(
        embedded_relationship(&form.json_value())["showing_reblogs"],
        json!(false),
        "the answer states the stored flags"
    );

    let query = common::TestRequest::post(&format!("{ROUTE}?uri=dave&reblogs=0&notify=1"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(query.status, StatusCode::OK, "the query string is read");

    for target in [&bob, &dave] {
        let row = relation(&pool, &alice, target).await;
        assert!(
            !row.show_reblogs && row.notify,
            "the {} row holds both columns",
            target.nickname
        );
    }
}

/// The route demands a user token: no token is the 401 "Invalid
/// credentials", an invalid one the 401 "The access token is invalid", and
/// an app-only credential the 403 — all before the `uri` is ever read.
#[tokio::test]
async fn the_route_requires_a_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app_only = common::create_app_only_token(&pool, "write:follows").await;
    let app = common::test_app(pool.clone());

    let anonymous = common::TestRequest::post(ROUTE)
        .json(&json!({"uri": "anyone"}))
        .send(&app)
        .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "no token at all"
    );
    assert_eq!(anonymous.text(), NO_TOKEN, "`AuthUser`'s no-token body");

    let invalid = common::TestRequest::post(ROUTE)
        .bearer("bogus-token")
        .json(&json!({"uri": "anyone"}))
        .send(&app)
        .await;
    assert_eq!(
        invalid.status,
        StatusCode::UNAUTHORIZED,
        "an invalid token is not ignored"
    );
    assert_eq!(invalid.text(), INVALID_TOKEN, "the invalid-token body");

    let app_token = common::TestRequest::post(ROUTE)
        .bearer(&app_only)
        .json(&json!({"uri": "anyone"}))
        .send(&app)
        .await;
    assert_eq!(app_token.status, StatusCode::FORBIDDEN, "an app-only token");
    assert_eq!(app_token.text(), USER_REQUIRED, "the user-required body");
}

/// The scope gate matches the id-keyed route's: `write:follows`, the
/// `follow` root and the `write` root pass, anything else is Pleroma's
/// insufficient-permissions 403 naming both accepted alternatives.
#[tokio::test]
async fn the_route_gates_on_the_follow_scopes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", "follow").await;
    let app = common::test_app(pool.clone());

    for (n, scopes) in ["write:follows", "follow", "write"].into_iter().enumerate() {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let target = common::create_local_user(&pool, &format!("granted{n}")).await;
        let response = follow_uri(&app, &token, &target.nickname).await;
        common::assert_answer(
            &response,
            StatusCode::OK,
            &format!("{scopes} follows by uri"),
        );
        assert_eq!(
            embedded_relationship(&response.json_value())["following"],
            json!(true),
            "{scopes} got the account with the relationship embedded"
        );
    }

    for scopes in ["read:follows", "read", "write:accounts", "read:accounts"] {
        let token = common::create_app_and_token(&pool, &alice, scopes).await;
        let denied = follow_uri(&app, &token, "whoever").await;
        common::assert_answer(
            &denied,
            StatusCode::FORBIDDEN,
            &format!("{scopes} on follows"),
        );
        assert_eq!(denied.text(), NO_SCOPE, "the denial joins both scopes");
    }
}

/// The route runs in the id-keyed follow's one `relations_actions` bucket:
/// the budget counts down across both paths, and the request that spends
/// it is refused 429 before the write — while the default test
/// configuration carries no limiter headers at all.
#[tokio::test]
async fn the_route_shares_the_relations_actions_bucket() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "follow").await;
    let _bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let app = common::test_app_with_config(
        pool.clone(),
        common::test_config_with_rate_limits("relations_actions=2:600"),
    );

    let first = follow_uri(&app, &token, "bob").await;
    common::assert_answer(&first, StatusCode::OK, "inside the budget");
    assert_eq!(first.header("x-ratelimit-limit"), Some("2"), "the bucket");
    assert_eq!(
        first.header("x-ratelimit-remaining"),
        Some("1"),
        "one spend counted"
    );

    let second = common::TestRequest::post(&follow_by_id(&carol))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(
        &second,
        StatusCode::OK,
        "the id route spends the same bucket",
    );
    assert_eq!(
        second.header("x-ratelimit-remaining"),
        Some("0"),
        "and the shared window is spent"
    );

    let throttled = follow_uri(&app, &token, "carol").await;
    common::assert_answer(
        &throttled,
        StatusCode::TOO_MANY_REQUESTS,
        "the spent window",
    );
    assert_eq!(throttled.text(), THROTTLED, "decision 8's body");

    let plain = common::test_app(pool.clone());
    let uncounted = follow_uri(&plain, &token, "carol").await;
    common::assert_answer(&uncounted, StatusCode::OK, "the limiter-off app");
    assert_eq!(
        uncounted.header("x-ratelimit-limit"),
        None,
        "and no headers when the limiter is off"
    );
}
