//! End-to-end federation refusals (WBE-224 scenario 6): every way an
//! inbound delivery is turned away, asserted by re-reading the public API
//! after the attempt — empty timelines and notification pages, unchanged
//! counters, and the documented `401` / `403` / `404` / `413` — so a
//! refusal is proven to be a refusal with no side effects, over real HTTP
//! from [`remote::MockPeer`].
//!
//! The classes pinned here, each delivered by the mock's own
//! [`sign_request`]-signed traffic or a deliberate defect in it:
//!
//! - the inbox door's refusals — an unsigned `POST`, a signature under an
//!   unknown `keyId`, a `Signature` header that does not parse, and a
//!   signature that verifies against a *different* actor than the
//!   activity's `actor` (the spoofed signer) — every one a 401 with its
//!   documented body and nothing behind it: no mirror of the claimed
//!   actor, no follow row, no counter moved;
//! - the body's integrity — a `Digest` that lies about the bytes and a
//!   body altered after signing are the same 401 `Invalid digest` before
//!   any key is fetched;
//! - the ids — an activity or embedded object whose id lives on another
//!   host than the signer's origin, and a `Create` whose object
//!   `attributedTo` disagrees with the envelope actor, pass the door as
//!   `202` and are dropped inside the dispatcher: re-read, no status
//!   landed, no follow was granted, and the fabricated author was never
//!   mirrored;
//! - the body cap — an over-`INBOX_MAX_BYTES` `POST` is the 413, on a
//!   router mounted with a small [`wired_be::config::Config::inbox_max_bytes`]
//!   (the existing knob, not a new one);
//! - the SSRF guard — with `fetch.http_allow_private_hosts = false` a
//!   `resolve=true` lookup of an actor on a loopback literal is refused
//!   with no row written (search answers empty, lookup and the follow
//!   write answer 404), while `sign_request`-signed inbound traffic from
//!   an already-mirrored peer still walks the whole gate and grants;
//! - the MRF `SimplePolicy` — a domain blocked through
//!   `POST /api/v1/pleroma/admin/config` (the admin endpoint
//!   [`simple`]'s lists are driven from) drops the mock's `Create` and
//!   even its `Follow` with a 403 `Rejected` and no side effects, and
//!   lifting the block lets a later activity through.
//!
//! The signature codec's own accept/reject corners live where they are
//! implemented (`src/federation/signature/verify.rs`, `inbox_auth.rs`,
//! `authorized_fetch.rs`, `domain_blocks.rs`, `ingest.rs`); this module
//! owns the round-trip view and the re-reads that prove each refusal
//! wrote nothing.
//!
//! [`sign_request`]: wired_be::federation::signature::sign_request
//! [`simple`]: wired_be::federation::mrf::simple

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    config::TEST_BASE_URL,
    federation::signature::SignedHeaders,
    models::{
        activitypub_actor::ActivityPubActorWrite,
        user::{self, Flag, NewUser, User, get_by_ap_id, get_by_nickname, insert},
    },
};

use crate::{common, remote};

/// The mock's delivering account, `bob@127.0.0.1:{port}`.
const BOB: &str = "bob";
/// The mock's other account, never a real signer here: the identity a
/// spoofed activity names or an object attributes itself to.
const MALLORY: &str = "mallory";
/// The host the mock answers on, spelled as the reject lists spell it:
/// [`signer_host`] strips the port off the peer's `keyId`.
///
/// [`signer_host`]: wired_be::federation::inbox_auth::signer_host
const LOOPBACK_HOST: &str = "127.0.0.1";
/// The instance's shared inbox, where every refusal is provoked.
const INBOX: &str = "/inbox";
/// The account reads and lookups.
const ACCOUNTS: &str = "/api/v1/accounts";
/// The caller's notification page.
const NOTIFICATIONS: &str = "/api/v1/notifications";
/// The caller's follow-request queue.
const FOLLOW_REQUESTS: &str = "/api/v1/follow_requests";
/// The legacy follow write keyed by handle or actor URL.
const FOLLOWS: &str = "/api/v1/follows";
/// The public timeline read.
const PUBLIC_TIMELINE: &str = "/api/v1/timelines/public";
/// The v2 combined search.
const SEARCH_V2: &str = "/api/v2/search";
/// The admin config store, the HTTP face of the `:mrf` and `:mrf_simple`
/// rows the [`SimplePolicy`](wired_be::federation::mrf::simple) reads.
const ADMIN_CONFIG: &str = "/api/v1/pleroma/admin/config";
/// The scopes every local scenario actor carries.
const SCOPES: &str = "read write follow";
/// The 401 body of the missing or unparsable signature step.
const MISSING_SIGNATURE: &str = "Missing signature";
/// The 401 body of a `Digest` that does not describe the received bytes.
const INVALID_DIGEST: &str = "Invalid digest";
/// The 401 body of a `keyId` that resolves to no readable actor key.
const UNKNOWN_KEY: &str = "Unknown key";
/// The 401 body of an activity whose `actor` is not the authenticated
/// signer.
const ACTOR_MISMATCH: &str = "Signature actor does not match activity actor";
/// The 413 body of a body over the inbox cap.
const PAYLOAD_TOO_LARGE: &str = "Payload too large";
/// The 403 body of a delivery from a rejected domain.
const REJECTED: &str = "Rejected";
/// The shared 404 the client API answers for what it will not name.
const RECORD_NOT_FOUND: &str = "Record not found";
/// The name every fabricated id of this module carries, spelled like a
/// stored id so nothing about its shape — only about its host — is what
/// the refusal turns on.
const SPOOF: &str = "01934567-89ab-7cd0-f00d-fabricated00";

/// A local account named `nickname` that can sign its own deliveries, with
/// a bearer token granting [`SCOPES`].
async fn local(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = remote::keyed_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, SCOPES).await;
    (created, token)
}

// --- the mock's traffic, tampered on purpose ---------------------------------------------

/// The bytes a signed delivery carries: what [`MockPeer::signed_headers`]
/// digests and what the request then sends verbatim.
///
/// [`MockPeer::signed_headers`]: remote::MockPeer::signed_headers
fn signed_bytes(body: &Value) -> Vec<u8> {
    serde_json::to_vec(body).expect("the fixture activity serializes")
}

/// The four auth headers of the peer's actor `actor`'s signed `POST` of
/// `body` to this instance's shared inbox.
fn wire(peer: &remote::MockPeer, actor: &str, body: &Value) -> SignedHeaders {
    remote::MockPeer::signed_headers(&peer.actor(actor), INBOX, &signed_bytes(body))
}

/// The wire headers a [`wire`] answer carries, as the pairs
/// [`post_wire`] sends: host, date, digest, signature.
fn wire_headers(signed: &SignedHeaders) -> [(&'static str, String); 4] {
    [
        ("host", signed.host.clone()),
        ("date", signed.date.clone()),
        (
            "digest",
            signed.digest.clone().expect("a signed post digests"),
        ),
        ("signature", signed.signature.clone()),
    ]
}

/// `POST`s `body` to the shared inbox carrying `headers` verbatim — the
/// shape a delivery arrives in, which the tampering tests build by signing
/// one thing and sending another.
async fn post_wire(app: &Router, body: &Value, headers: &[(&str, String)]) -> common::TestResponse {
    let mut request = common::TestRequest::post(INBOX).json(body);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    request.send(app).await
}

/// A `Follow` by the mock's `BOB` of the local `target`, named `name` on
/// the peer's own host.
fn follow_of(peer: &remote::MockPeer, name: &str, target: &User) -> Value {
    peer.follow_of(name, &peer.actor(BOB), &target.ap_id)
}

/// A `Create` by the mock's `BOB` of a public note named `name` on the
/// peer's own host, authored by `author`.
fn create_of(peer: &remote::MockPeer, name: &str, author: &str) -> Value {
    peer.activity(
        "Create",
        name,
        &peer.actor(BOB),
        &json!({
            "id": peer.note(name),
            "type": "Note",
            "attributedTo": peer.actor(author),
            "content": format!("<p>a note named {name}</p>"),
            "to": [remote::PUBLIC],
        }),
    )
}

/// `POST`s `activity` to this instance's shared inbox as the peer's `BOB`,
/// fully signed, and answers the inbox's reply for the test to read.
async fn deliver(app: &Router, peer: &remote::MockPeer, activity: &Value) -> common::TestResponse {
    peer.deliver(app, &peer.actor(BOB), INBOX, activity).await
}

// --- reading the public surface back ------------------------------------------------------

/// `GET /api/v1/accounts/{id}` as `token`, answered as the `Account`
/// entity: the re-read every "unchanged counters" assertion takes.
async fn account(app: &Router, token: &str, id: &str) -> Value {
    let response = common::TestRequest::get(&format!("{ACCOUNTS}/{id}"))
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the account read");
    response.json_value()
}

/// One denormalised counter off the account read.
async fn counter(app: &Router, token: &str, id: &str, column: &str) -> Value {
    account(app, token, id)
        .await
        .get(column)
        .cloned()
        .unwrap_or(Value::Null)
}

/// The `uris` of the anonymous `GET /api/v1/timelines/public` page: the
/// re-read every "no status landed" assertion takes.
async fn public_uris(app: &Router) -> Vec<String> {
    let response = common::TestRequest::get(PUBLIC_TIMELINE).send(app).await;
    common::assert_answer(&response, StatusCode::OK, "the public timeline");
    response
        .json::<Vec<Value>>()
        .into_iter()
        .map(|entry| {
            entry["uri"]
                .as_str()
                .expect("a timeline entry carries its uri")
                .to_owned()
        })
        .collect()
}

/// `GET /api/v1/notifications` as `token`: the re-read every "no telling
/// landed" assertion takes.
async fn notifications(app: &Router, token: &str) -> Vec<Value> {
    let response = common::TestRequest::get(NOTIFICATIONS)
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the notification page");
    response.json::<Vec<Value>>()
}

/// `GET /api/v1/follow_requests` as `token`: the queue no refused
/// delivery may ever fill.
async fn follow_requests(app: &Router, token: &str) -> Vec<Value> {
    let response = common::TestRequest::get(FOLLOW_REQUESTS)
        .bearer(token)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the follow-request queue");
    response.json::<Vec<Value>>()
}

/// Asserts the refused `Follow` of `target` left nothing anywhere the
/// public API can see: no follower, no queue entry, no telling.
async fn assert_follow_landed_nothing(app: &Router, target: &User, token: &str) {
    assert_eq!(
        counter(app, token, &target.id.to_string(), "followers_count").await,
        json!(0),
        "a refused Follow counts no follower"
    );
    assert!(
        follow_requests(app, token).await.is_empty(),
        "and parks no request in the queue"
    );
    assert!(
        notifications(app, token).await.is_empty(),
        "and tells the account nothing"
    );
}

// --- the inbox door's refusals -------------------------------------------------------------

/// An unsigned `POST` to the shared inbox is the 401 `Missing signature`,
/// and nothing behind it happens: the claimed signer was never even
/// resolved, alice counts no follower, and her queue and notifications
/// stay empty.
#[tokio::test]
async fn an_unsigned_inbox_post_is_refused_and_touches_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "e2fs-unsigned-alice").await;

    let answer = common::TestRequest::post(INBOX)
        .json(&follow_of(&peer, "s-unsigned", &alice))
        .send(&app)
        .await;
    common::assert_answer(&answer, StatusCode::UNAUTHORIZED, "the unsigned delivery");
    common::assert_error(&answer, MISSING_SIGNATURE, "the unsigned delivery");

    assert!(
        get_by_ap_id(&pool, &peer.actor(BOB))
            .await
            .expect("the mirror read runs")
            .is_none(),
        "the claimed signer was never resolved, so nothing was mirrored"
    );
    assert_follow_landed_nothing(&app, &alice, &alice_token).await;
    assert!(
        public_uris(&app).await.is_empty(),
        "and no status came with it"
    );
}

/// A signature under a `keyId` that resolves to no actor — the mock 404s
/// the path — is the 401 `Unknown key`, written nowhere: neither the
/// ghost actor nor any follow row exists afterwards.
#[tokio::test]
async fn a_signature_under_an_unknown_key_is_refused_and_mirrors_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "e2fs-ghostkey-alice").await;

    let ghost = format!("{}/keys/ghost", peer.origin());
    let body = follow_of(&peer, "s-ghostkey", &alice);
    let signed = remote::MockPeer::signed_headers(&ghost, INBOX, &signed_bytes(&body));
    let answer = post_wire(&app, &body, &wire_headers(&signed)).await;
    common::assert_answer(
        &answer,
        StatusCode::UNAUTHORIZED,
        "the unknown-key delivery",
    );
    common::assert_error(&answer, UNKNOWN_KEY, "the unknown-key delivery");

    assert!(
        get_by_ap_id(&pool, &ghost)
            .await
            .expect("the ghost read runs")
            .is_none(),
        "a key resolution that failed wrote no mirror"
    );
    assert_follow_landed_nothing(&app, &alice, &alice_token).await;
}

/// A `Signature` header that is present but does not parse is refused the
/// same 401 `Missing signature` the absent header earns — the parameters
/// describe no proof — with nothing written.
#[tokio::test]
async fn a_malformed_signature_header_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "e2fs-malformed-alice").await;

    let body = follow_of(&peer, "s-malformed", &alice);
    let mut headers = wire_headers(&wire(&peer, BOB, &body));
    headers[3].1 = format!(r#"keyId="{}#main-key""#, peer.actor(BOB));
    let answer = post_wire(&app, &body, &headers).await;
    common::assert_answer(
        &answer,
        StatusCode::UNAUTHORIZED,
        "the malformed-signature delivery",
    );
    common::assert_error(
        &answer,
        MISSING_SIGNATURE,
        "the malformed-signature delivery",
    );

    assert_follow_landed_nothing(&app, &alice, &alice_token).await;
}

/// A signature that verifies against one mock actor cannot speak for
/// another: the delivery signed as `bob` whose `actor` names `mallory` is
/// the 401 `Signature actor does not match activity actor`, and the
/// spoofed identity was never mirrored.
#[tokio::test]
async fn a_signature_by_another_actor_than_the_activity_names_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "e2fs-spoofsigner-alice").await;

    let spoofed = json!({
        "id": peer.activity_id("s-spoofsigner"),
        "type": "Follow",
        "actor": peer.actor(MALLORY),
        "object": alice.ap_id,
    });
    let signed = wire(&peer, BOB, &spoofed);
    let answer = post_wire(&app, &spoofed, &wire_headers(&signed)).await;
    common::assert_answer(
        &answer,
        StatusCode::UNAUTHORIZED,
        "the spoofed-signer delivery",
    );
    common::assert_error(&answer, ACTOR_MISMATCH, "the spoofed-signer delivery");

    assert!(
        get_by_ap_id(&pool, &peer.actor(MALLORY))
            .await
            .expect("the spoofed actor read runs")
            .is_none(),
        "the forged `actor` never became a row"
    );
    assert_follow_landed_nothing(&app, &alice, &alice_token).await;
}

/// The body's integrity is checked before any key is fetched: a `Digest`
/// that lies about the bytes and a body swapped after signing are the same
/// 401 `Invalid digest`, and neither variant leaves a follow behind.
#[tokio::test]
async fn a_body_or_digest_that_disagrees_with_the_signature_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "e2fs-digest-alice").await;

    let signed_a = wire(&peer, BOB, &follow_of(&peer, "s-digest-a", &alice));
    // The body altered after signing: `B` rides `A`'s digest and signature.
    let swapped = post_wire(
        &app,
        &follow_of(&peer, "s-digest-b", &alice),
        &wire_headers(&signed_a),
    )
    .await;
    common::assert_answer(
        &swapped,
        StatusCode::UNAUTHORIZED,
        "the body swapped after signing",
    );
    common::assert_error(&swapped, INVALID_DIGEST, "the body swapped after signing");

    // The `Digest` that simply lies: the right body, a well-formed hash of
    // something else.
    let mut headers = wire_headers(&signed_a);
    headers[2].1 = "SHA-256=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned();
    let lying = post_wire(&app, &follow_of(&peer, "s-digest-a", &alice), &headers).await;
    common::assert_answer(&lying, StatusCode::UNAUTHORIZED, "the lying digest");
    common::assert_error(&lying, INVALID_DIGEST, "the lying digest");

    assert_follow_landed_nothing(&app, &alice, &alice_token).await;
}

// --- behind the door: the dispatcher's own refusals -----------------------------------------

/// An id on another host is a fabrication the door answers `202` to and
/// the dispatcher drops: the `Follow` whose id names `evil.example`
/// grants nothing, and the `Create` whose embedded object claims this
/// instance's own origin lands no status — the public API re-reads as
/// empty on every surface.
#[tokio::test]
async fn an_id_off_the_signer_origin_passes_the_door_and_is_dropped() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "e2fs-spoofid-alice").await;

    let foreign_id = json!({
        "id": format!("https://evil.example/activities/{SPOOF}"),
        "type": "Follow",
        "actor": peer.actor(BOB),
        "object": alice.ap_id,
    });
    let answer = deliver(&app, &peer, &foreign_id).await;
    common::assert_answer(
        &answer,
        StatusCode::ACCEPTED,
        "the foreign-id Follow reaches the queue",
    );
    let _drained = common::run_all_jobs(&state).await;
    assert_follow_landed_nothing(&app, &alice, &alice_token).await;

    assert_planted_object(app, state, pool, peer, alice_token).await;
}

/// The `Create` arm of the foreign-id refusal: the door takes the signed
/// activity, and the dispatcher drops it because the embedded object's id
/// names this instance's own origin, not the signer's — no status lands
/// anywhere and the author's counter stays where it was.
async fn assert_planted_object(
    app: Router,
    state: wired_be::state::AppState,
    pool: PgPool,
    peer: remote::MockPeer,
    alice_token: String,
) {
    let planted = json!({
        "id": peer.activity_id("s-planted"),
        "type": "Create",
        "actor": peer.actor(BOB),
        "object": {
            "id": format!("{TEST_BASE_URL}/objects/{SPOOF}"),
            "type": "Note",
            "attributedTo": peer.actor(BOB),
            "content": "<p>planted into another origin</p>",
            "to": [remote::PUBLIC],
        },
    });
    let planted_uri = planted["object"]["id"]
        .as_str()
        .expect("the fixture object carries its id")
        .to_owned();
    let answer = deliver(&app, &peer, &planted).await;
    common::assert_answer(
        &answer,
        StatusCode::ACCEPTED,
        "the foreign-object Create reaches the queue",
    );
    let _drained = common::run_all_jobs(&state).await;

    assert!(
        !public_uris(&app).await.contains(&planted_uri),
        "the planted object landed no status"
    );
    assert!(
        public_uris(&app).await.is_empty(),
        "and the timeline holds nothing at all"
    );
    let bob = get_by_ap_id(&pool, &peer.actor(BOB))
        .await
        .expect("the signer mirror read runs")
        .expect("the door mirrored bob");
    assert_eq!(
        counter(&app, &alice_token, &bob.id.to_string(), "statuses_count").await,
        json!(0),
        "and the author's account counts no post"
    );
}

/// A `Create` whose object attributes itself to somebody else than the
/// envelope actor is dropped by the handler — a peer cannot post other
/// accounts' notes into this instance: the timeline re-reads empty and
/// the named author was never even mirrored.
#[tokio::test]
async fn an_object_authoring_itself_to_another_actor_is_dropped() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "e2fs-author-alice").await;

    let answer = deliver(&app, &peer, &create_of(&peer, "s-forged-author", MALLORY)).await;
    common::assert_answer(
        &answer,
        StatusCode::ACCEPTED,
        "the forged-author Create reaches the queue",
    );
    let _drained = common::run_all_jobs(&state).await;

    assert!(
        public_uris(&app).await.is_empty(),
        "the forged note landed nothing"
    );
    assert!(
        get_by_ap_id(&pool, &peer.actor(MALLORY))
            .await
            .expect("the forged author read runs")
            .is_none(),
        "and the named author was never fetched or mirrored"
    );
    assert!(
        notifications(&app, &alice_token).await.is_empty(),
        "and nobody was told about it"
    );
}

/// A body over the router's `INBOX_MAX_BYTES` cap is the 413
/// `Payload too large`, refused while streaming — before the key behind
/// the signature is resolved, so nothing is mirrored and nothing folds.
#[tokio::test]
async fn an_oversize_inbox_body_is_refused_with_413() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = common::test_config();
    config.inbox_max_bytes = 512;
    let state = common::test_app_state(pool.clone(), config);
    let app = wired_be::app(state.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "e2fs-size-alice").await;

    let mut body = follow_of(&peer, "s-oversize", &alice);
    body["padding"] = json!("x".repeat(1024));
    let signed = wire(&peer, BOB, &body);
    let answer = post_wire(&app, &body, &wire_headers(&signed)).await;
    common::assert_answer(&answer, StatusCode::PAYLOAD_TOO_LARGE, "the over-cap body");
    common::assert_error(&answer, PAYLOAD_TOO_LARGE, "the over-cap body");

    let _drained = common::run_all_jobs(&state).await;
    assert!(
        get_by_ap_id(&pool, &peer.actor(BOB))
            .await
            .expect("the mirror read runs")
            .is_none(),
        "the 413 arrives before the key resolution"
    );
    assert_follow_landed_nothing(&app, &alice, &alice_token).await;
}

// --- the SSRF guard -------------------------------------------------------------------------

/// Asserts none of the resolving surfaces reaches the mock through the
/// closed guard: the `resolve=true` search answers with an empty accounts
/// arm, the resolving lookup and the follow-by-uri write answer the shared
/// 404, and no row of the mock origin was written by any of them.
async fn assert_resolutions_refused(
    app: &Router,
    peer: &remote::MockPeer,
    pool: &PgPool,
    token: &str,
) {
    let search =
        common::TestRequest::get(&format!("{SEARCH_V2}?q={}&resolve=true", peer.actor(BOB)))
            .bearer(token)
            .send(app)
            .await;
    common::assert_answer(&search, StatusCode::OK, "the guarded search answers");
    assert!(
        search.json_value()["accounts"]
            .as_array()
            .expect("the accounts arm is an array")
            .is_empty(),
        "the loopback actor could not be resolved"
    );

    for query in [
        format!("?q={}&resolve=true", peer.actor(BOB)),
        format!("?q={}&resolve=true", peer.handle(BOB)),
    ] {
        let refused = common::TestRequest::get(&format!("{ACCOUNTS}/lookup{query}"))
            .bearer(token)
            .send(app)
            .await;
        common::assert_answer(&refused, StatusCode::NOT_FOUND, "the guarded lookup");
        common::assert_error(&refused, RECORD_NOT_FOUND, "the guarded lookup");
    }

    let follow = common::TestRequest::post(FOLLOWS)
        .bearer(token)
        .json(&json!({"uri": peer.actor(BOB)}))
        .send(app)
        .await;
    common::assert_answer(&follow, StatusCode::NOT_FOUND, "the guarded follow");

    assert!(
        get_by_ap_id(pool, &peer.actor(BOB))
            .await
            .expect("the mirror read runs")
            .is_none(),
        "the refused resolution wrote no row"
    );
    assert!(
        get_by_nickname(pool, &peer.handle(BOB))
            .await
            .expect("the handle read runs")
            .is_none(),
        "and no mirror under the port-carrying handle either"
    );
}

/// With `fetch.http_allow_private_hosts = false` the guard stands between
/// this instance and the mock's loopback literal: every `resolve=true`
/// lookup is refused with no row written, while a `sign_request`-signed
/// `Follow` from an already-mirrored peer walks the whole gate — no
/// network needed — and grants.
#[tokio::test]
async fn the_ssrf_guard_refuses_resolutions_but_not_signed_deliveries() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = common::test_config();
    config.fetch.http_allow_private_hosts = false;
    let state = common::test_app_state(pool.clone(), config);
    let app = wired_be::app(state.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "e2fs-ssrf-alice").await;

    assert_resolutions_refused(&app, &peer, &pool, &alice_token).await;

    // Inbound signed traffic is unaffected: the peer's mirror stands seeded
    // from a prior life and authenticates without any fetch.
    let _bob = seed_mock_mirror(&pool, &peer, BOB).await;
    let folded = deliver(&app, &peer, &follow_of(&peer, "s-ssrf-follow", &alice)).await;
    common::assert_answer(
        &folded,
        StatusCode::ACCEPTED,
        "the signed delivery still folds",
    );
    let _drained = common::run_all_jobs(&state).await;
    assert_eq!(
        counter(&app, &alice_token, &alice.id.to_string(), "followers_count").await,
        json!(1),
        "and the follow it carries grants"
    );
}

/// The seeded mirror of the peer's account `nick`: the row a fetch would
/// have written, inserted directly so a test can stand behind a closed
/// SSRF guard and still authenticate the peer against
/// [`MockPeer::public_key_pem`].
///
/// [`MockPeer::public_key_pem`]: remote::MockPeer::public_key_pem
async fn seed_mock_mirror(pool: &PgPool, peer: &remote::MockPeer, nick: &str) -> User {
    let ap_id = peer.actor(nick);
    let mirror = insert(
        pool,
        &NewUser {
            nickname: peer.handle(nick),
            ap_id: ap_id.clone(),
            domain: Some(LOOPBACK_HOST.to_owned()),
            name: nick.to_owned(),
            ..NewUser::default()
        },
    )
    .await
    .expect("the seeded mirror inserts");
    let _row = common::write_actor(
        pool,
        &ActivityPubActorWrite {
            inbox: Some(user::inbox(&ap_id)),
            shared_inbox: Some(peer.shared_inbox()),
            outbox: Some(user::outbox(&ap_id)),
            followers_url: Some(user::followers(&ap_id)),
            following_url: Some(user::following(&ap_id)),
            public_key: Some(remote::MockPeer::public_key_pem().to_owned()),
            ..ActivityPubActorWrite::bare(mirror.id)
        },
    )
    .await;
    mirror
}

// --- the MRF domain block ---------------------------------------------------------------------

/// The `{"tuple": [key, value]}` array the config API exchanges keyword
/// lists as (the shape `admin.rs` documents for `POST /config`).
fn wire_tuples(pairs: &[(&str, Value)]) -> Vec<Value> {
    pairs
        .iter()
        .map(|(key, value)| json!({"tuple": [key, value]}))
        .collect()
}

/// `POST /api/v1/pleroma/admin/config` with one `configs` batch.
async fn post_configs(app: &Router, token: &str, items: Value) -> common::TestResponse {
    common::TestRequest::post(ADMIN_CONFIG)
        .bearer(token)
        .json(&json!({"configs": items}))
        .send(app)
        .await
}

/// A local admin with both config scopes, as a bearer token.
async fn config_admin(pool: &PgPool, nickname: &str) -> String {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    common::create_app_and_token(pool, &created, "admin:read:config admin:write:config").await
}

/// Blocks the mock's host through the [`SimplePolicy`](wired_be::federation::mrf::simple)
/// lists, over the admin config endpoint the operator's block rides.
async fn block_mock_host(app: &Router, token: &str) {
    let written = post_configs(
        app,
        token,
        json!([
            {
                "group": ":pleroma",
                "key": ":mrf",
                "value": wire_tuples(&[("policies", json!(["simple"]))]),
            },
            {
                "group": ":pleroma",
                "key": ":mrf_simple",
                "value": wire_tuples(&[("reject", json!([[LOOPBACK_HOST, "spammers"]]))]),
            },
        ]),
    )
    .await;
    common::assert_answer(&written, StatusCode::OK, "the :mrf block writes");
}

/// Lifts the block again: both stored rows go, the chain falls back to the
/// empty env configuration.
async fn unblock_mock_host(app: &Router, token: &str) {
    let deleted = post_configs(
        app,
        token,
        json!([
            { "group": ":pleroma", "key": ":mrf", "delete": true },
            { "group": ":pleroma", "key": ":mrf_simple", "delete": true },
        ]),
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the :mrf unblock deletes");
}

/// A domain blocked via the MRF `SimplePolicy` is shut out at the inbox
/// door: after the mock's own `Follow` was granted as a baseline, the
/// block turns the mock's `Create` and even a fresh `Follow` of a new
/// local account away with the 403 `Rejected` and no side effects, and
/// lifting the block lets a later activity through.
#[tokio::test]
async fn a_domain_blocked_by_simple_policy_is_dropped_and_unblocking_lifts_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "e2fs-block-alice").await;
    let (carol, carol_token) = local(&pool, "e2fs-block-carol").await;
    let admin = config_admin(&pool, "e2fs-block-admin").await;

    let folded = deliver(&app, &peer, &follow_of(&peer, "s-block-baseline", &alice)).await;
    common::assert_answer(&folded, StatusCode::ACCEPTED, "the pre-block Follow folds");
    let _baseline = common::run_all_jobs(&state).await;
    assert_eq!(
        counter(&app, &alice_token, &alice.id.to_string(), "followers_count").await,
        json!(1),
        "the baseline follow stands granted"
    );

    block_mock_host(&app, &admin).await;
    let refused = [
        deliver(&app, &peer, &create_of(&peer, "s-blocked-create", BOB)).await,
        deliver(&app, &peer, &follow_of(&peer, "s-blocked-follow", &carol)).await,
    ];
    common::assert_all_refused(
        &refused,
        StatusCode::FORBIDDEN,
        REJECTED,
        "the blocked deliveries",
    );
    let _nothing_queued = common::run_all_jobs(&state).await;
    assert!(
        public_uris(&app).await.is_empty(),
        "the blocked Create landed no status"
    );
    assert_eq!(
        counter(&app, &alice_token, &alice.id.to_string(), "followers_count").await,
        json!(1),
        "the block wrote nothing to alice"
    );
    assert_follow_landed_nothing(&app, &carol, &carol_token).await;

    unblock_mock_host(&app, &admin).await;
    let refollowed = deliver(&app, &peer, &follow_of(&peer, "s-unblocked-follow", &carol)).await;
    common::assert_answer(
        &refollowed,
        StatusCode::ACCEPTED,
        "the post-block Follow folds",
    );
    let recreated = deliver(&app, &peer, &create_of(&peer, "s-unblocked-create", BOB)).await;
    common::assert_answer(
        &recreated,
        StatusCode::ACCEPTED,
        "the post-block Create reaches the queue",
    );
    let _drained = common::run_all_jobs(&state).await;
    assert_eq!(
        counter(&app, &carol_token, &carol.id.to_string(), "followers_count").await,
        json!(1),
        "the later Follow grants once the block is lifted"
    );
    assert!(
        public_uris(&app)
            .await
            .contains(&peer.note("s-unblocked-create")),
        "and the later Create lands in the public timeline"
    );
}
