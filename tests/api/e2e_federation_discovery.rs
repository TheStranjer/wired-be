//! End-to-end federation discovery and remote resolution (WBE-224
//! scenarios 1 and 5): what a remote actually consumes, read over real
//! HTTP or through the local router and asserted on the JSON-LD keys, not
//! just the status codes.
//!
//! Everything here runs against [`remote::MockPeer`] — a loopback instance
//! with its own keys, actor documents, `WebFinger` and capturing inboxes —
//! and against this instance's own documents, read twice: through the
//! in-process router, and over a real loopback socket by a reader that
//! knows nothing but the handle it resolves. What the scenarios pin:
//!
//! - `GET /.well-known/webfinger` (the `acct:` form a remote fingers for a
//!   `user@host` handle and its bare spelling), `GET /.well-known/host-meta`,
//!   the `NodeInfo` discovery pair and `GET /users/{nickname}` with
//!   `Accept: application/activity+json` answer the shapes a Mastodon-style
//!   remote consumes — the actor's `@context`, `id`, `preferredUsername`,
//!   `inbox`, `outbox`, `followers`, `following`, `publicKey.id` /
//!   `publicKeyPem` and `endpoints.sharedInbox`, and the JRD `links[]` —
//!   and a reader fetching this instance's actor over real HTTP mirrors it
//!   with the port in its handle, exactly as decision 7 stores remotes;
//! - `AUTHORIZED_FETCH` on: the mock's unsigned actor read is the 401 with
//!   the documented body, its `sign_request`-signed read answers 200,
//!   `GET /internal/fetch` — the document a peer reads to learn a key —
//!   stays open, and the same gate stands on the object and activity
//!   reads; the toggle rides a [`wired_be::config::Config`] built on
//!   [`common::test_config`] and mounted with
//!   [`common::test_app_with_config`], never the environment;
//! - resolution (scenario 5): `GET /api/v2/search?q=@user@host:port&resolve=true`,
//!   `GET /api/v1/accounts/lookup?q=…&resolve=true` and the
//!   `GET /api/v1/statuses/{id}` URI read with `resolve=true` all reach the
//!   mock over HTTP and answer entities whose `uri` / `url` name the mock
//!   origin, and `POST /api/v1/follows` with only a `uri=` resolves the
//!   mock actor, mirrors it and leaves a signed `Follow` in the mock's
//!   inbox.
//!
//! The per-endpoint refusals and codec corners belong to the area files
//! (`well_known.rs`, `nodeinfo.rs`, `authorized_fetch.rs`,
//! `activitypub.rs`, `search.rs`, `follow_by_uri.rs`); this module owns
//! the round-trip-over-real-HTTP view.

#![cfg(test)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::net::TcpListener;
use wired_be::{
    config::Toggle,
    federation::fetch_actor::get_or_fetch_by_acct,
    models::{
        activitypub_actor::ActivityPubActorWrite,
        user::{NewUser, User, get_by_ap_id, insert, local_ap_id},
    },
    state::AppState,
};

use crate::{common, remote};

/// The mock's account the authorized reads are signed as, `bob@127.0.0.1:{port}`.
const BOB: &str = "bob";
/// The instance origin of [`common::test_config`].
const BASE: &str = "https://test.example";
/// The media type an `ActivityPub` document is served with and asked for.
const AP_ACCEPT: &str = "application/activity+json";
/// The refusal body the `AUTHORIZED_FETCH` gate answers with.
const REFUSAL: &str = "This instance requires authenticated fetches";
/// The account reads and lookups.
const ACCOUNTS: &str = "/api/v1/accounts";
/// The legacy follow write keyed by handle or actor URL.
const FOLLOWS: &str = "/api/v1/follows";
/// The status read and write.
const STATUSES: &str = "/api/v1/statuses";
/// The v2 combined search.
const SEARCH_V2: &str = "/api/v2/search";
/// The scopes the local scenario actors carry.
const SCOPES: &str = "read write follow";
/// The cap on one document the reader-side HTTP client fetches — the
/// actor-document size the federation fetcher itself caps at.
const FETCH_CAP: usize = 1024 * 1024;

/// A local account named `nickname` that can sign its own deliveries, with
/// a bearer token granting [`SCOPES`].
async fn local(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = remote::keyed_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, SCOPES).await;
    (created, token)
}

// --- the instance behind a real socket -------------------------------------------------

/// The running local instance behind a real loopback socket: the router
/// [`wired_be::app`] serves on a bound port and the `http://{host}:{port}`
/// origin it self-identifies as. A scenario points a reader at this origin
/// and every federation lookup the reader runs — `WebFinger`, actor
/// dereference, plain fetch — rides the wire, not `oneshot`.
struct Served {
    /// The instance origin, scheme and port included.
    origin: String,
    /// The task serving it, aborted on drop.
    server: tokio::task::JoinHandle<()>,
}

impl Served {
    /// Binds a port and serves the real router over `pool`, with the
    /// served instance's `BASE_URL` set to the bound origin so its
    /// documents vouch for the URL a reader fetched them from.
    async fn start(pool: PgPool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the served instance binds a port");
        let origin = format!("http://{}", listener.local_addr().expect("a bound address"));
        let mut served_config = common::test_config();
        served_config.base_url = origin.clone();
        let state = common::test_app_state(pool, served_config);
        let app = wired_be::app(state);
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("served instance stopped: {err}");
            }
        });
        Self { origin, server }
    }

    /// The `host:port` authority of this instance's origin: the domain
    /// half of every handle that resolves to it (decision 7).
    fn authority(&self) -> String {
        self.origin
            .split_once("://")
            .map_or_else(String::new, |(_scheme, rest)| rest.to_owned())
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// `GET`s `url` with `accept` through `reader`'s own federation HTTP
/// client — the client a remote runs — answered as the JSON document it
/// carries, asserting the plain 200 on the way.
async fn fetched(reader: &AppState, url: &str, accept: &str) -> Value {
    let response = reader
        .http_client
        .get_limited(url, &[("accept", accept)], FETCH_CAP)
        .await
        .expect("the remote reader reaches the served instance");
    assert_eq!(response.status, 200, "{url} answers the reader");
    response.json().expect("the served document is JSON")
}

/// Asserts `actor` is an actor document a remote can federate with: the
/// `ActivityStreams` context, the self-vouching `id`, the nickname, the
/// four collections under the actor id, the `#main-key` public key and the
/// instance-wide shared inbox.
fn assert_actor_document(actor: &Value, ap_id: &str, nickname: &str, origin: &str) {
    assert_eq!(actor["type"], json!("Person"), "an actor is a Person");
    assert_eq!(actor["id"], json!(ap_id), "the document vouches for itself");
    assert_eq!(
        actor["preferredUsername"],
        json!(nickname),
        "the nickname a handle ends with"
    );
    assert_eq!(actor["inbox"], json!(format!("{ap_id}/inbox")));
    assert_eq!(actor["outbox"], json!(format!("{ap_id}/outbox")));
    assert_eq!(actor["followers"], json!(format!("{ap_id}/followers")));
    assert_eq!(actor["following"], json!(format!("{ap_id}/following")));
    assert_eq!(
        actor["publicKey"]["id"],
        json!(format!("{ap_id}#main-key")),
        "the key id is the actor id with the Mastodon fragment"
    );
    assert!(
        actor["publicKey"]["publicKeyPem"]
            .as_str()
            .is_some_and(|pem| pem.contains("BEGIN")),
        "and the key material is a PEM"
    );
    assert_eq!(
        actor["endpoints"]["sharedInbox"],
        json!(format!("{origin}/inbox")),
        "the shared inbox hangs off the instance origin"
    );
    assert!(
        actor["@context"].as_array().is_some_and(|contexts| contexts
            .iter()
            .any(|context| context == "https://www.w3.org/ns/activitystreams")),
        "the JSON-LD context names ActivityStreams"
    );
}

/// Asserts the JRD `reader` fingers for `efdmirror@{authority}` over real
/// HTTP names that exact acct — port included — and points its `self` link
/// at the actor `ap_id` with the `ActivityPub` media type.
async fn assert_fingered_jrd(reader: &AppState, origin: &str, authority: &str, ap_id: &str) {
    let finger = fetched(
        reader,
        &format!("{origin}/.well-known/webfinger?resource=acct:efdmirror@{authority}"),
        "application/jrd+json",
    )
    .await;
    assert_eq!(
        finger["subject"],
        json!(format!("acct:efdmirror@{authority}")),
        "the JRD vouches for the asked acct, port included"
    );
    let self_link = finger["links"]
        .as_array()
        .expect("the JRD carries its links")
        .iter()
        .find(|link| link["rel"] == json!("self"))
        .expect("one self link names the actor");
    assert_eq!(
        self_link["type"],
        json!(AP_ACCEPT),
        "the self link's media type is the one a federation client asks with"
    );
    assert_eq!(
        self_link["href"],
        json!(ap_id),
        "and the href is the actor id"
    );
}

/// Scenario 1 over the wire: a remote reader resolves `efdmirror@host:port`
/// through this instance's own `WebFinger` and actor routes over real HTTP
/// — no router in between — and the stored mirror carries the port in its
/// handle exactly as decision 7 spells remotes, with every federation fact
/// the document named stored beside it.
#[tokio::test]
async fn a_remote_reader_fingers_fetches_and_mirrors_the_local_actor_over_http() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let served = Served::start(pool.clone()).await;
    let authority = served.authority();

    // The actor the served instance self-identifies as, on its own origin.
    let ap_id = local_ap_id(&served.origin, "efdmirror");
    let keys = remote::local_keys().clone();
    let public_pem = keys.public_pem.clone();
    let alice = insert(&pool, &NewUser::local("efdmirror", &ap_id))
        .await
        .expect("the served fixture inserts");
    let _row = common::write_actor(
        &pool,
        &ActivityPubActorWrite::for_local_account(&alice, &served.origin, keys),
    )
    .await;

    // The reader: a second instance, on its own leased slot, that has never
    // heard of alice. Every request it makes rides the socket.
    let reader_db = common::test_db().await;
    let reader = common::test_app_state(reader_db.pool().clone(), common::test_config());

    assert_fingered_jrd(&reader, &served.origin, &authority, &ap_id).await;

    // The actor document, read off the socket: the JSON-LD keys a remote
    // federation client builds its view of this instance from.
    let actor = fetched(&reader, &ap_id, AP_ACCEPT).await;
    assert_actor_document(&actor, &ap_id, "efdmirror", &served.origin);
    assert_eq!(
        actor["publicKey"]["publicKeyPem"],
        json!(public_pem),
        "the served PEM is the one the row carries"
    );

    // The resolve-and-mirror round trip: WebFinger, actor fetch, vouch
    // check and the write — all over the socket.
    let mirror = get_or_fetch_by_acct(&reader, &format!("efdmirror@{authority}"))
        .await
        .expect("the reader resolves and mirrors the served actor");
    assert_eq!(
        mirror.nickname,
        format!("efdmirror@{authority}"),
        "the stored handle carries the port, decision 7's spelling"
    );
    assert_eq!(mirror.ap_id, ap_id, "the mirror is keyed on the fetched id");
    assert!(!mirror.local.get(), "and it is stored as a remote");
    let mirrored = common::actor_of(reader_db.pool(), &mirror).await;
    assert_eq!(
        mirrored.shared_inbox.as_deref(),
        Some(format!("{}/inbox", served.origin).as_str()),
        "the endpoints object's sharedInbox landed on the actor row"
    );
    assert_eq!(
        mirrored.followers_url.as_deref(),
        Some(format!("{ap_id}/followers").as_str()),
        "and so did the collections the document named"
    );
}

/// Asserts the in-process actor read of the fixture account serves the
/// full JSON-LD document an anonymous federation client expects, with the
/// row's own key material in `publicKeyPem`.
async fn assert_served_actor(app: &axum::Router, alice: &User) {
    let actor = common::TestRequest::get("/users/e2fd-alice")
        .header("accept", AP_ACCEPT)
        .send(app)
        .await;
    common::assert_answer(&actor, StatusCode::OK, "the actor read");
    assert_eq!(
        actor.header("content-type"),
        Some("application/activity+json; charset=utf-8"),
        "the document is served as ActivityPub JSON"
    );
    let document = actor.json_value();
    assert_actor_document(&document, &alice.ap_id, "e2fd-alice", BASE);
    assert_eq!(
        document["publicKey"]["publicKeyPem"],
        json!(remote::local_keys().public_pem),
        "the served PEM is the one the actor row carries"
    );
}

/// Asserts `WebFinger` answers both the `acct:` form a remote fingers for
/// a `user@host` handle and its bare spelling — the same JRD, the same
/// three `links[]` a Mastodon-style client consumes.
async fn assert_webfinger_pair(app: &axum::Router, ap_id: &str) {
    for resource in [
        "acct%3Ae2fd-alice%40test.example",
        "e2fd-alice%40test.example",
    ] {
        let response =
            common::TestRequest::get(&format!("/.well-known/webfinger?resource={resource}"))
                .send(app)
                .await;
        common::assert_answer(&response, StatusCode::OK, "the WebFinger read");
        assert_eq!(
            response.header("content-type"),
            Some("application/jrd+json"),
            "the answer is a JRD"
        );
        let finger = response.json_value();
        assert_eq!(finger["subject"], json!("acct:e2fd-alice@test.example"));
        assert_eq!(
            finger["aliases"],
            json!([ap_id]),
            "the actor id is an alias of the account"
        );
        let links = finger["links"]
            .as_array()
            .expect("the JRD carries its links");
        assert_eq!(
            links[0],
            json!({
                "rel": "http://webfinger.net/rel/profile-page",
                "type": "text/html",
                "href": ap_id,
            }),
            "the profile link a client shows its user"
        );
        assert_eq!(
            links[1],
            json!({ "rel": "self", "type": AP_ACCEPT, "href": ap_id }),
            "the self link a federation client dereferences"
        );
        assert_eq!(
            links[2],
            json!({
                "rel": "http://ostatus.org/schema/1.0/subscribe",
                "template": format!("{BASE}/authorize_interaction?uri={{uri}}"),
            }),
            "the subscribe template a remote follow-intent flows through"
        );
    }
}

/// Asserts the discovery pair a Mastodon-style client walks before it ever
/// asks about an account: host-meta's lrdd template and the `NodeInfo`
/// documents behind the well-known link.
async fn assert_host_meta_and_nodeinfo(app: &axum::Router) {
    let host_meta = common::TestRequest::get("/.well-known/host-meta")
        .send(app)
        .await;
    common::assert_answer(&host_meta, StatusCode::OK, "the host-meta read");
    assert_eq!(
        host_meta.header("content-type"),
        Some("application/xrd+xml"),
        "host-meta is the XRD document RFC 7033 spells"
    );
    assert!(
        host_meta.text().contains(&format!(
            r#"rel="lrdd" template="{BASE}/.well-known/webfinger?resource={{uri}}""#
        )),
        "the lrdd template points clients at WebFinger: {}",
        host_meta.text()
    );

    let discovery = common::TestRequest::get("/.well-known/nodeinfo")
        .send(app)
        .await;
    common::assert_answer(&discovery, StatusCode::OK, "the NodeInfo discovery read");
    assert_eq!(
        discovery.json_value()["links"][0],
        json!({
            "rel": "http://nodeinfo.diaspora.software/ns/schema/2.0",
            "href": format!("{BASE}/nodeinfo/2.0.json"),
        }),
        "the 2.0 link a Mastodon-style remote follows first"
    );

    let nodeinfo = common::TestRequest::get("/nodeinfo/2.0").send(app).await;
    common::assert_answer(&nodeinfo, StatusCode::OK, "the NodeInfo 2.0 read");
    let info = nodeinfo.json_value();
    assert_eq!(
        info["version"],
        json!("2.0"),
        "the document names its schema"
    );
    assert_eq!(info["software"]["name"], json!("wired"), "who serves it");
    assert_eq!(
        info["protocols"],
        json!(["activitypub"]),
        "how peers speak to it"
    );
    assert!(
        info["openRegistrations"].is_boolean(),
        "the registration flag a remote weighs before fetching"
    );
    assert!(
        info["metadata"]["federation"].is_object(),
        "and the federation block Mastodon-style remotes read"
    );
}

/// Scenario 1 through the local router: the actor read a federation client
/// starts from, the `WebFinger` pair with the `links[]` a Mastodon client
/// expects, the host-meta template, and the `NodeInfo` discovery pair.
#[tokio::test]
async fn the_discovery_surface_answers_a_mastodon_style_reader() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = remote::keyed_local_user(&pool, "e2fd-alice").await;
    let app = common::test_app(pool);

    assert_served_actor(&app, &alice).await;
    assert_webfinger_pair(&app, &alice.ap_id).await;
    assert_host_meta_and_nodeinfo(&app).await;
}

/// `GET`s `path` as an `ActivityPub` client carrying the mock peer's actor
/// `actor`'s signature over exactly that path, answered as the document.
async fn signed_read(
    app: &axum::Router,
    peer: &remote::MockPeer,
    actor: &str,
    path: &str,
) -> Value {
    let signed = remote::MockPeer::signed_get(&peer.actor(actor), path);
    let response = common::TestRequest::get(path)
        .header("accept", AP_ACCEPT)
        .header("host", &signed.host)
        .header("date", &signed.date)
        .header("signature", &signed.signature)
        .send(app)
        .await;
    common::assert_answer(
        &response,
        StatusCode::OK,
        &format!("the signed read of {path}"),
    );
    response.json_value()
}

/// Asserts an unsigned `ActivityPub` `GET` of `path` is the gate's 401 with
/// the documented refusal body.
async fn assert_gate_refuses(app: &axum::Router, path: &str) {
    let refused = common::TestRequest::get(path)
        .header("accept", AP_ACCEPT)
        .send(app)
        .await;
    common::assert_answer(&refused, StatusCode::UNAUTHORIZED, "the unsigned read");
    common::assert_error(&refused, REFUSAL, "the unsigned read");
}

/// Scenario 1, `AUTHORIZED_FETCH` on: the mock's unsigned actor read is the
/// documented 401, a `sign_request`-signed fetch under an accepted remote
/// key answers 200, `GET /internal/fetch` stays open for the peer that
/// must learn a key before it can sign, and the same gate stands on the
/// object and activity reads.
#[tokio::test]
async fn authorized_fetch_gates_the_actor_object_and_activity_reads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = common::test_config();
    config.fetch.authorized_fetch = Toggle::new(true);
    let app = common::test_app_with_config(pool.clone(), config);
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "e2fd-gated-alice").await;

    assert_gate_refuses(&app, "/users/e2fd-gated-alice").await;
    let document = signed_read(&app, &peer, BOB, "/users/e2fd-gated-alice").await;
    assert_eq!(
        document["id"],
        json!(alice.ap_id),
        "the gate answers the signed reader the very actor document"
    );

    let internal = common::TestRequest::get("/internal/fetch")
        .header("accept", AP_ACCEPT)
        .send(&app)
        .await;
    common::assert_answer(
        &internal,
        StatusCode::OK,
        "the key-discovery document stays open",
    );

    let post = common::TestRequest::post(STATUSES)
        .bearer(&alice_token)
        .json(&json!({"status": "gated under authorized fetch"}))
        .send(&app)
        .await;
    common::assert_answer(&post, StatusCode::OK, "the fixture post");
    let id = post.json_value()["id"]
        .as_str()
        .expect("the entity carries its id")
        .to_owned();

    for (path, kind) in [
        (format!("/objects/{id}"), "Note"),
        (format!("/activities/{id}"), "Create"),
    ] {
        assert_gate_refuses(&app, &path).await;
        let served = signed_read(&app, &peer, BOB, &path).await;
        assert_eq!(
            served["type"],
            json!(kind),
            "the gated {path} read answers the {kind} document"
        );
    }
}

/// Scenario 5: the `resolve=true` lookups. The v2 account search, the
/// account lookup's `q` + `resolve` arm and the status show route's URI
/// read all reach the mock over HTTP — fingered, dereferenced, mirrored —
/// and answer entities whose `uri` / `url` name the mock origin.
#[tokio::test]
async fn resolution_reaches_the_mock_and_answers_mock_origin_entities() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_alice, alice_token) = local(&pool, "e2fd-res-alice").await;

    // The handle search: `@bob@host:port` fingers at the mock and the
    // resolved account rides first in the accounts arm.
    let found = common::TestRequest::get(&format!(
        "{SEARCH_V2}?q=%40{}&resolve=true",
        peer.handle(BOB)
    ))
    .bearer(&alice_token)
    .send(&app)
    .await;
    common::assert_answer(&found, StatusCode::OK, "the resolving account search");
    let resolved = &found.json_value()["accounts"][0];
    assert_eq!(resolved["acct"], json!(peer.handle(BOB)), "the mock handle");
    assert_eq!(
        resolved["uri"],
        json!(peer.actor(BOB)),
        "the actor id names the mock origin"
    );
    assert_eq!(
        resolved["url"],
        json!(peer.actor(BOB)),
        "and so does the profile url"
    );
    assert!(
        get_by_ap_id(&pool, &peer.actor(BOB))
            .await
            .expect("the mirror read runs")
            .is_some(),
        "the round trip stored the mirror it resolved"
    );

    // The lookup's `q` + `resolve` arm, for a handle no search asked about.
    let looked = common::TestRequest::get(&format!(
        "{ACCOUNTS}/lookup?q={}&resolve=true",
        peer.handle("carol")
    ))
    .bearer(&alice_token)
    .send(&app)
    .await;
    common::assert_answer(&looked, StatusCode::OK, "the resolving lookup");
    let carol = looked.json_value();
    assert_eq!(carol["acct"], json!(peer.handle("carol")));
    assert_eq!(
        carol["uri"],
        json!(peer.actor("carol")),
        "and the looked-up account names the mock origin too"
    );

    // The status read by its object URI: dereferenced at the mock, folded,
    // and answered as the entity the note became.
    let note = peer.note("resolved");
    let shown = common::TestRequest::get(&format!("{STATUSES}/{}?resolve=true", encode_uri(&note)))
        .bearer(&alice_token)
        .send(&app)
        .await;
    common::assert_answer(&shown, StatusCode::OK, "the resolving status read");
    let entity = shown.json_value();
    assert_eq!(
        entity["uri"],
        json!(note),
        "the resolved post's uri names the mock origin"
    );
    assert_eq!(
        entity["content"],
        json!("<p>a note relayed as resolved</p>"),
        "and it is the mock's own note"
    );
    assert_eq!(
        entity["account"]["acct"],
        json!(peer.handle("author")),
        "authored by the actor the object names"
    );
}

/// Scenario 5: `POST /api/v1/follows` with only a `uri=` resolves the mock
/// actor over `WebFinger`, mirrors it, waits on the peer's answer, and the
/// mock's inbox records this instance's own signed `Follow`.
#[tokio::test]
async fn following_by_uri_resolves_the_mock_and_delivers_a_signed_follow() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (alice, alice_token) = local(&pool, "e2fd-follow-alice").await;

    let response = common::TestRequest::post(FOLLOWS)
        .bearer(&alice_token)
        .json(&json!({"uri": peer.handle("dave")}))
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the follow-by-uri write");
    let target = response.json_value();
    assert_eq!(target["acct"], json!(peer.handle("dave")));
    assert_eq!(
        target["uri"],
        json!(peer.actor("dave")),
        "the entity names the mock origin"
    );
    assert_eq!(
        target["pleroma"]["relationship"]["requested"],
        json!(true),
        "a follow of a remote account waits for their answer"
    );
    assert!(
        get_by_ap_id(&pool, &peer.actor("dave"))
            .await
            .expect("the mirror read runs")
            .is_some(),
        "the resolve wrote the mirror the follow keys on"
    );

    let _drained = common::run_all_jobs(&state).await;
    let follows = peer.delivered("Follow");
    assert_eq!(follows.len(), 1, "the mock's inbox recorded the Follow");
    let target_actor = peer.actor("dave");
    assert_outbound_follow(
        &follows[0],
        &alice,
        &target_actor,
        &peer.inbox_path(&target_actor),
    );
}

/// Asserts `delivery` is this instance's own signed `Follow` of `target`
/// by `actor`: arrived at the followee's inbox path, made and addressed as
/// the model documents, the `Digest` true to the bytes that arrived and
/// the `Signature` verifying against the key the acting row publishes.
fn assert_outbound_follow(delivery: &remote::Delivery, actor: &User, target: &str, path: &str) {
    let body = delivery.activity();
    assert_eq!(
        delivery.path(),
        path,
        "the Follow arrived at the followee's inbox"
    );
    assert_eq!(body["type"], json!("Follow"), "the delivered activity type");
    assert_eq!(
        body["actor"],
        json!(actor.ap_id),
        "made by this instance's actor"
    );
    assert_eq!(
        body["object"],
        json!(target),
        "naming the mock actor as its object"
    );
    assert_eq!(
        delivery.key_id().as_deref(),
        Some(format!("{}#main-key", actor.ap_id).as_str()),
        "the signer is the actor the activity names"
    );
    assert!(
        delivery.digest_matches(),
        "the digest describes the delivered bytes"
    );
    let pem = remote::local_keys().public_pem.as_str();
    assert!(
        delivery.signature_verifies(pem),
        "and the signature verifies against that key"
    );
}

/// The `ActivityPub` URI percent-encoded as one path segment: the spelling
/// a client sends to `GET /api/v1/statuses/{id}` when `{id}` names an
/// object by its URI.
fn encode_uri(uri: &str) -> String {
    uri.replace(':', "%3A").replace('/', "%2F")
}
