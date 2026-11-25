//! The `remote::MockPeer` harness smoke tests (WBE-224 slice 1): the round
//! trips every later federation scenario stands on, and nothing else.
//!
//! [`remote::MockPeer`] is shared code, so decision 13 asks for it to be
//! covered on its own. What is proved here is the harness working, once per
//! direction:
//!
//! - *resolution*: the instance resolves a mock account through
//!   `GET /api/v1/accounts/lookup?acct=…&resolve=true` — `WebFinger` at the
//!   mock origin, actor dereference, mirror written — and answers the entity
//!   with `acct`, `uri` and `url` naming that origin. Decision 7 stores the
//!   handle as `user@127.0.0.1:{port}`: the mock's own loopback authority,
//!   which is the host [`common::test_config`]'s open `HTTP_ALLOW_PRIVATE_HOSTS`
//!   lets this instance reach.
//! - *outbound*: a local account follows a mock actor and posts, and
//!   [`common::run_all_jobs`] drives a real delivery into the mock's inbox.
//!   The capture holds exactly one `Create`, at the followed account's own
//!   inbox path, whose `keyId` names the local author and whose `Signature`
//!   verifies against the `public_key` stored on her row — transport-level
//!   truth read off the wire, not off a job count.
//! - *inbound*: one signed `Create` of a `Note`, `POST`ed by the mock through
//!   this instance's inbox, folds through the real dispatcher and appears on
//!   `GET /api/v1/timelines/public` under the mock author's loopback handle.
//! - *the served read surface*: what a remote's own client dereferences here —
//!   the self-identifying actor document and the matching `GET /key` mirror,
//!   the paged outbox a test fills with `publish`, and the object route
//!   answering the shaped note `serve_note` put at that id.
//!
//! The ticket scenarios live in their own modules (`e2e_federation_discovery`,
//! `e2e_federation_follow`, `e2e_federation_inbound`,
//! `e2e_federation_outbound`, `e2e_federation_safety`); none of them is here.
//!
//! [`common::run_all_jobs`]: crate::common::run_all_jobs

#![cfg(test)]

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{models::user::User, state::AppState};

use crate::{common, remote};

/// The mock account the local fixture follows and posts to, `bob@127.0.0.1:{port}`.
const BOB: &str = "bob";
/// The mock account the first smoke test resolves, as an actor served under
/// `/users/alice` with a `name` of its own nickname.
const ALICE: &str = "alice";
/// The scopes the local fixtures carry: the resolving lookup, the follow and
/// the status write.
const SCOPES: &str = "read write follow";
/// The `ActivityStreams` public address, as a delivered note names it.
const AS_PUBLIC: &str = remote::PUBLIC;
/// The account reads and lookups.
const ACCOUNTS: &str = "/api/v1/accounts";
/// The legacy follow write keyed by handle or actor URL.
const FOLLOWS: &str = "/api/v1/follows";
/// The status write.
const STATUSES: &str = "/api/v1/statuses";
/// The anonymous public timeline read.
const PUBLIC_TIMELINE: &str = "/api/v1/timelines/public";
/// The media type a federated document is served with and asked for.
const ACTIVITY_JSON: &str = "application/activity+json";
/// The cap on one document the reader's client takes — the actor-document
/// size the federation fetcher itself caps at.
const FETCH_CAP: usize = 1024 * 1024;

/// A local account named `nickname` that can sign its own deliveries, with a
/// bearer token granting [`SCOPES`].
async fn local(pool: &PgPool, nickname: &str) -> (User, String) {
    let created = remote::keyed_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, SCOPES).await;
    (created, token)
}

/// `GET`s `path` as `token` (anonymous when `None`), asserted 200, answered as
/// a JSON array.
async fn list(app: &Router, token: Option<&str>, path: &str) -> Vec<Value> {
    let request = common::TestRequest::get(path);
    let response = match token {
        Some(found) => request.bearer(found).send(app).await,
        None => request.send(app).await,
    };
    common::assert_answer(&response, StatusCode::OK, &format!("the read of `{path}`"));
    response
        .json_value()
        .as_array()
        .unwrap_or_else(|| panic!("`{path}` should answer a JSON array"))
        .clone()
}

/// `POST /api/v1/statuses` with `body`, asserted 200, answered as the created
/// entity.
async fn post_status(app: &Router, token: &str, body: Value) -> Value {
    let response = common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the status write");
    response.json_value()
}

/// `GET /api/v1/timelines/public`, anonymous.
async fn public_timeline(app: &Router) -> Vec<Value> {
    list(app, None, PUBLIC_TIMELINE).await
}

/// The status entity in `entries` with object uri `uri`, refusing to be absent.
fn entry_for<'a>(entries: &'a [Value], uri: &str) -> &'a Value {
    entries
        .iter()
        .find(|entry| entry["uri"] == json!(uri))
        .unwrap_or_else(|| panic!("the listing should carry {uri}"))
}

/// `GET`s `url` through `reader`'s own federation `HttpClient` — the client a
/// remote peer runs, and the reason the mock is served on a real port —
/// asserted 200 and answered as the JSON document it carries.
async fn fetched(reader: &AppState, url: &str) -> Value {
    let response = reader
        .http_client
        .get_limited(url, &[("accept", ACTIVITY_JSON)], FETCH_CAP)
        .await
        .expect("the reader reaches the mock peer on loopback");
    assert_eq!(response.status, 200, "`{url}` answers the reader");
    response.json().expect("the mock serves a JSON document")
}

/// The text of `document[key]`, refusing to be absent: one of the URLs a mock
/// document links to, which a test then follows.
fn link(document: &Value, key: &str) -> String {
    document[key]
        .as_str()
        .unwrap_or_else(|| panic!("the document names its `{key}`"))
        .to_owned()
}

/// Harness smoke: the mock peer is resolvable. The instance fingers the
/// handle at the mock's own `/.well-known/webfinger`, dereferences the actor
/// it names and mirrors it, and the `Account` entity it answers carries the
/// mock's loopback origin in `acct`, `uri` and `url`.
#[tokio::test]
async fn a_mock_handle_resolves_to_an_account_on_the_mock_origin() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (_reader, token) = local(&pool, "smoke-reader").await;

    let response = common::TestRequest::get(&format!(
        "{ACCOUNTS}/lookup?acct={}&resolve=true",
        peer.handle(ALICE)
    ))
    .bearer(&token)
    .send(&app)
    .await;
    common::assert_answer(&response, StatusCode::OK, "the resolving lookup");

    let entity = response.json_value();
    let actor = peer.actor(ALICE);
    assert_eq!(
        entity["acct"],
        json!(peer.handle(ALICE)),
        "a remote account is stored as `user@host` (decision 7)"
    );
    assert_eq!(
        entity["uri"],
        json!(actor),
        "the mirrored account's uri names the mock origin"
    );
    assert_eq!(entity["url"], json!(actor), "and so does its profile url");
}

/// Harness smoke: the instance can deliver. A local account follows a mock
/// actor — a signed `Follow` this instance itself puts on the wire — and
/// posts a note naming that actor, and the mock's capture holds exactly one
/// `Create` that proves its own authenticity: the `keyId` of the author, a
/// `Signature` that verifies against the public key stored on her row, a
/// `Digest` describing the bytes as received, and the mock's own `Host`.
#[tokio::test]
async fn a_local_follow_and_post_deliver_one_create_the_mock_can_verify() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let (author, token) = local(&pool, "smoke-author").await;

    let followed = common::TestRequest::post(FOLLOWS)
        .bearer(&token)
        .json(&json!({"uri": peer.handle(BOB)}))
        .send(&app)
        .await;
    common::assert_answer(&followed, StatusCode::OK, "the follow write");
    let _sent = common::run_all_jobs(&state).await;
    assert_eq!(
        peer.delivered("Follow").len(),
        1,
        "the follow itself reached the mock over HTTP"
    );

    let posted = post_status(
        &app,
        &token,
        json!({"status": format!("hello @{}", peer.handle(BOB))}),
    )
    .await;
    let _delivered = common::run_all_jobs(&state).await;

    let creates = peer.delivered("Create");
    assert_eq!(creates.len(), 1, "exactly one Create reached the mock");
    let create = creates.first().expect("the capture holds a Create");
    let key_id = format!("{}#main-key", author.ap_id);
    let public_key = remote::local_keys().public_pem.as_str();
    assert_eq!(
        create.path(),
        remote::INBOX,
        "a public post reaches the mentioned account's host at its shared inbox"
    );
    assert_eq!(
        create.activity()["object"]["id"],
        posted["uri"],
        "the object is the note the write just made"
    );
    assert_eq!(
        create.key_id().as_deref(),
        Some(key_id.as_str()),
        "the Signature names the local author's key"
    );
    assert!(
        create.signature_verifies(public_key),
        "and verifies against the key that author stores"
    );
    assert!(
        create.digest_matches(),
        "the Digest describes the bytes that arrived"
    );
    assert_eq!(
        create.header("host"),
        Some(peer.host().as_str()),
        "the Host names the mock authority"
    );
    let date = create
        .header("date")
        .expect("a Date is signed into a delivery");
    assert!(date.ends_with(" GMT"), "as an RFC 1123 stamp: {date}");
}

/// Harness smoke: the mock can deliver back. One signed `Create` of a `Note`
/// posted at this instance's shared inbox rides the real router and the real
/// signature check, folds into a mirrored status, and reads back on the
/// anonymous public timeline under its author's loopback handle.
#[tokio::test]
async fn a_mock_signed_create_folds_and_lands_on_the_public_timeline() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);
    let uri = peer.note("smoke-note");
    let note = peer.note_document(
        "smoke-note",
        &actor,
        "<p>hello from the mock peer</p>",
        json!({}),
    );
    let create = peer.activity("Create", "smoke-note", &actor, &note);

    let accepted = peer.deliver(&app, &actor, remote::INBOX, &create).await;
    assert_eq!(
        accepted.status,
        StatusCode::ACCEPTED,
        "the inbox takes the mock's Create"
    );
    let _folded = common::run_all_jobs(&state).await;

    let public = public_timeline(&app).await;
    let entry = entry_for(&public, &uri);
    assert_eq!(
        entry["account"]["acct"],
        json!(peer.handle(BOB)),
        "the author reads under her loopback handle (decision 7)"
    );
    assert_eq!(
        entry["content"],
        json!("<p>hello from the mock peer</p>"),
        "the delivered body"
    );
    assert_eq!(
        entry["visibility"],
        json!("public"),
        "addressed to the public collection"
    );
}

/// Harness smoke: the documents a remote's own reader dereferences. The
/// actor's `Person` document is self-identifying, carries its four
/// collections and a real PEM, and `GET /key` mirrors that same key as a
/// standalone `Key` object owned by an actor this peer serves — the shape a
/// `keyId` that names the key rather than an actor resolves to. Rotating the
/// peer's keys moves what both documents serve, which is the skew a rotation
/// scenario reads back.
#[tokio::test]
async fn the_mock_serves_the_actor_document_and_a_matching_key_mirror() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(ALICE);

    let document = fetched(&state, &actor).await;
    assert_eq!(document["id"], json!(actor), "an actor vouches for itself");
    assert_eq!(document["type"], json!("Person"), "as a `Person`");
    assert_eq!(
        document["inbox"],
        json!(format!("{actor}/inbox")),
        "with its personal inbox"
    );
    assert_eq!(document["outbox"], json!(peer.outbox_url(ALICE)));
    assert_eq!(
        document["followers"],
        json!(format!("{actor}/followers")),
        "and its collections"
    );
    assert_eq!(
        document["following"],
        json!(format!("{actor}/following")),
        "all under its own id"
    );
    assert_eq!(
        document["publicKey"]["id"],
        json!(format!("{actor}#main-key")),
        "the key id is the actor with the Mastodon fragment"
    );
    let pem = peer.served_public_key();
    assert!(
        pem.contains("BEGIN"),
        "and the served key is PEM material: {pem}"
    );
    assert_eq!(document["publicKey"]["publicKeyPem"], json!(pem));

    let key = fetched(&state, &peer.key_url()).await;
    assert_eq!(key["type"], json!("Key"), "the standalone key object");
    assert_eq!(
        key["owner"],
        json!(peer.actor(remote::KEY_OWNER)),
        "owned by an account the peer also serves"
    );
    assert_eq!(
        key["publicKeyPem"],
        json!(pem),
        "the very key the actor document embeds"
    );

    peer.rotate_keys();
    let rotated = fetched(&state, &peer.key_url()).await;
    assert_ne!(
        rotated["publicKeyPem"],
        json!(pem),
        "rotation moves what is served"
    );
    assert_eq!(
        rotated["publicKeyPem"],
        json!(peer.served_public_key()),
        "and the mirror is what the peer now publishes"
    );
}

/// Harness smoke: the paged outbox and the shapeable note. `publish` decides
/// what the collection lists, its pages walk on `first` / `next` / `prev`,
/// `note_document` shapes a `Note` past the addressing its builder defaults
/// to, and `serve_note` decides what that id dereferences to — while a name
/// nothing was shaped for still answers the peer's own public note.
#[tokio::test]
async fn the_mock_pages_its_outbox_and_serves_the_notes_a_test_shapes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_app, state) = remote::app_and_state(pool.clone());
    let peer = remote::MockPeer::start().await;
    let root = peer.outbox_url(ALICE);
    for name in ["one", "two", "three"] {
        peer.publish(ALICE, &peer.activity_id(name));
    }

    let collection = fetched(&state, &root).await;
    assert_eq!(
        collection["type"],
        json!("OrderedCollection"),
        "the outbox root is a collection"
    );
    assert_eq!(
        collection["totalItems"],
        json!(3),
        "counting every published activity"
    );
    let first = fetched(&state, &link(&collection, "first")).await;
    assert_eq!(
        first["type"],
        json!("OrderedCollectionPage"),
        "the first page of the walk"
    );
    assert_eq!(first["partOf"], json!(root), "belonging to that root");
    assert_eq!(
        first["orderedItems"],
        json!([peer.activity_id("one"), peer.activity_id("two")]),
        "one page of ids, oldest first"
    );
    let second = fetched(&state, &link(&first, "next")).await;
    assert_eq!(
        second["orderedItems"],
        json!([peer.activity_id("three")]),
        "the tail page carries the remainder"
    );
    assert_eq!(
        link(&second, "prev"),
        link(&first, "id"),
        "and links back to the page it followed"
    );
    assert!(second.get("next").is_none(), "where the walk ends");

    let actor = peer.actor(ALICE);
    let shaped = peer.note_document(
        "shaped",
        &actor,
        "<p>mind the edge</p>",
        json!({
            "to": [AS_PUBLIC, actor],
            "sensitive": true,
            "summary": "a shaped note",
            "tag": [{"type": "Mention", "href": "https://test.example/users/zed"}],
            "attachment": [{"type": "Document", "url": "https://test.example/x.png"}],
        }),
    );
    peer.serve_note("shaped", shaped.clone());
    assert_eq!(
        fetched(&state, &peer.note("shaped")).await,
        shaped,
        "the shaped note dereferences as built"
    );

    let canned = fetched(&state, &peer.note("unshaped")).await;
    assert_eq!(
        canned["content"],
        json!("<p>a note relayed as unshaped</p>"),
        "a name nothing was shaped for answers the peer's own note"
    );
    assert_eq!(
        canned["attributedTo"],
        json!(peer.actor("author")),
        "by the actor that note names"
    );
}
