//! Integration tests for the object reads: `GET /objects/{uuid}`, its
//! nickname-scoped twin `GET /users/{nickname}/statuses/{uuid}` and
//! `GET /notice/{id}` (`src/api/activitypub.rs`, the counterpart of
//! Pleroma's `OStatusController.object/2`, `ActivityPubController.object/2`
//! and `OStatusController.notice/2`).
//!
//! The first two are one shared core: an `Accept` naming an `ActivityPub`
//! media type gets the status' `Note` (or `Question`) document as
//! `application/activity+json; charset=utf-8`, keyed by the status uuid
//! the object id embeds; everything else — a browser included, since an
//! object is a document and not a page — is the 404 `{"error":"Not found"}`.
//! `/notice/{id}` runs the same core for an `ActivityPub` ask and sends a
//! browser's `text/html` to `{FRONTEND_BASE_URL}/notice/{id}`. Within the
//! core a remote row, a reblog row (an `Announce`), a nickname that is not
//! the live local author, and an unparsable uuid are all the same 404; a
//! soft-deleted status answers 410 with its `Tombstone`; a `local` status
//! is 404 always; and `private`/`direct` are served only to a request
//! whose HTTP Signature verifies and whose reader the visibility model
//! admits. Every answer carries `Vary: Accept`, and `AUTHORIZED_FETCH`
//! gates the reads like it gates the actor document.
//!
//! The document rendering itself is covered where it lives, in
//! `src/federation/note.rs`; what these tests own is the route: the
//! negotiation, the gates, the status codes and the headers end to end.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::{Config, Toggle},
    federation::signature::{SignedHeaders, SigningKey, sign_request},
    models::{
        follow::{self, FollowState},
        poll::{self, PollParams},
        status::{self, NewStatus, Status, Visibility},
        user::{NewUser, User},
    },
    services::instance_actor::ensure_instance_actor,
    util::{keys::generate_keypair_async, time::mastodon_time},
};

use crate::common;

/// The media type of an `ActivityPub` document, charset included.
const ACTIVITY_JSON: &str = "application/activity+json; charset=utf-8";
/// The instance origin of [`common::test_config`].
const BASE: &str = "https://test.example";
/// The refusal body the `AUTHORIZED_FETCH` gate answers with.
const REFUSAL: &str = "This instance requires authenticated fetches";
/// The plain 404 body every miss on these routes answers with.
const NOT_FOUND: &str = "Not found";
/// Every field of a bare public `Note` — no poll, no mentions, no media —
/// in sorted order, the exact key-set assertion the golden pairs with.
const NOTE_KEYS: [&str; 18] = [
    "@context",
    "actor",
    "attachment",
    "attributedTo",
    "cc",
    "content",
    "context",
    "conversation",
    "id",
    "inReplyTo",
    "published",
    "sensitive",
    "source",
    "summary",
    "tag",
    "to",
    "type",
    "url",
];

/// `GET`s `path` as an `ActivityPub` client.
async fn fetch_ap(app: &Router, path: &str) -> common::TestResponse {
    common::TestRequest::get(path)
        .header("accept", "application/activity+json")
        .send(app)
        .await
}

/// `GET`s `path` as an `ActivityPub` client whose request `signed` proves.
async fn fetch_signed(app: &Router, path: &str, signed: &SignedHeaders) -> common::TestResponse {
    common::TestRequest::get(path)
        .header("accept", "application/activity+json")
        .header("host", &signed.host)
        .header("date", &signed.date)
        .header("signature", &signed.signature)
        .send(app)
        .await
}

/// The object path of `status` — the URL its `uri` embeds.
fn object_path(status: &Status) -> String {
    format!("/objects/{}", status.id)
}

/// Signs a `GET` of `https://test.example{path}` as actor `user`, with
/// `user`'s stored key pair, the way a peer's authorized fetch would.
///
/// # Panics
///
/// If `user` holds no private key — the calling fixture forgot to give it
/// one.
fn signed_get_as(user: &User, path: &str) -> SignedHeaders {
    let key_id = format!("{}#main-key", user.ap_id);
    sign_request(
        "GET",
        &format!("{BASE}{path}"),
        None,
        &SigningKey {
            key_id: &key_id,
            private_pem: user
                .private_key
                .as_deref()
                .expect("the fixture user holds its private key"),
        },
        Utc::now(),
    )
    .expect("signing the test fetch")
}

/// The signer of the `AUTHORIZED_FETCH` test: the instance actor, a stored
/// row with the key pair [`ensure_instance_actor`] generated, exactly the
/// reader a gate-on instance expects from a peer that knows this instance's
/// actor document.
#[derive(Debug)]
struct InstanceSigner {
    /// Its actor id.
    ap_id: String,
    /// The private key the fetches are signed with.
    private_pem: String,
}

/// Provisions the instance actor and hands it over as a fetch signer.
async fn instance_signer(pool: &PgPool) -> InstanceSigner {
    let actor = ensure_instance_actor(pool, &common::test_config())
        .await
        .expect("the instance actor is provisioned");
    InstanceSigner {
        ap_id: actor.ap_id,
        private_pem: actor
            .private_key
            .expect("the provisioned actor holds its private key"),
    }
}

/// Signs a `GET` of `https://test.example{path}` as [`InstanceSigner`].
fn signed_get_as_instance(signer: &InstanceSigner, path: &str) -> SignedHeaders {
    let key_id = format!("{}#main-key", signer.ap_id);
    sign_request(
        "GET",
        &format!("{BASE}{path}"),
        None,
        &SigningKey {
            key_id: &key_id,
            private_pem: &signer.private_pem,
        },
        Utc::now(),
    )
    .expect("signing the test fetch")
}

/// A local [`User`] named `nickname` with a real RSA key pair stored, so it
/// can sign an authorized fetch: [`create_local_user_with`] plus the pair
/// [`generate_keypair_async`] mints.
///
/// [`create_local_user_with`]: common::create_local_user_with
///
/// # Panics
///
/// If key generation or the insert fails.
async fn signer_user(pool: &PgPool, nickname: &str) -> User {
    let keys = generate_keypair_async()
        .await
        .expect("the fixture key pair generates");
    common::create_local_user_with(pool, nickname, |new| NewUser {
        public_key: Some(keys.public_pem.clone()),
        private_key: Some(keys.private_pem.clone()),
        ..new
    })
    .await
}

/// `follower` follows `target` through [`wired_be::models::follow`], with
/// the accepted state the visibility model reads.
///
/// # Panics
///
/// If the follow write fails or is not granted on the spot.
async fn accepted_follow(pool: &PgPool, follower: &User, target: &User) {
    let opened = follow::follow(pool, BASE, follower, target)
        .await
        .expect("the fixture follow is written");
    assert_eq!(
        opened.state,
        FollowState::Accepted,
        "the follow of `{}` is granted",
        target.nickname
    );
}

/// Inserts `author`'s status of `visibility` @-mentioning everyone in
/// `mentioned` — the `direct` row a mentioned party reaches and a plain
/// follower does not.
async fn addressed(
    pool: &PgPool,
    author: &User,
    mentioned: &[&User],
    visibility: Visibility,
    text: &str,
) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            visibility,
            mentioned_user_ids: mentioned.iter().map(|user| user.id).collect(),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("the addressed fixture inserts")
}

/// A `remote` mirror of a peer's status: stored with the remote's own
/// `uri` and `local` clear, the shape [`wired_be::models::status::insert`]
/// writes for an activity this instance received rather than posted.
async fn remote_status(pool: &PgPool, author: &User, id: &str) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            uri: Some(format!("https://remote.example/objects/{id}")),
            user_id: author.id,
            content: "a mirrored post".to_owned(),
            raw_text: "a mirrored post".to_owned(),
            ..NewStatus::default()
        },
    )
    .await
    .expect("the remote status fixture inserts")
}

/// A poll of `options` on `post`, single choice and never expiring — the
/// minimum that turns the document into a `Question`.
///
/// # Panics
///
/// If the poll write fails.
async fn attach_poll(pool: &PgPool, post: &Status, options: &[&str]) {
    let params = PollParams {
        options: options.iter().map(|title| (*title).to_owned()).collect(),
        ..PollParams::default()
    };
    let mut conn = pool.acquire().await.expect("slot connection");
    let created = poll::create(&mut conn, post.id, &params)
        .await
        .expect("the fixture poll creates");
    assert_eq!(created.status_id, post.id, "the poll rides the status");
}

/// The sorted key set of `document`.
fn document_keys(document: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = document
        .as_object()
        .expect("a document is a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// A public note of `alice`'s, served over `app` by the object path and
/// asserted to be a 200 document before its JSON goes back to the test.
async fn served_note(app: &Router, status: &Status) -> Value {
    let response = fetch_ap(app, &object_path(status)).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "alice's public note is served"
    );
    assert_eq!(
        response.header("content-type"),
        Some(ACTIVITY_JSON),
        "the document is ActivityPub JSON"
    );
    assert_eq!(
        response.header("vary"),
        Some("Accept"),
        "the route negotiated on Accept"
    );
    response.json_value()
}

/// Asserts `response` is the plain 404 the ticket prescribes —
/// `{"error":"Not found"}` with `Vary: Accept` — for the case `who` names.
fn assert_not_found(response: &common::TestResponse, who: &str) {
    assert_eq!(response.status, StatusCode::NOT_FOUND, "{who}");
    assert_eq!(
        response.json_value(),
        json!({"error": NOT_FOUND}),
        "the 404 body for {who}"
    );
    assert_eq!(
        response.header("vary"),
        Some("Accept"),
        "the 404 for {who} varies on Accept"
    );
}

/// `GET /objects/{uuid}` serves a public note as exactly the `Note` the
/// federation layer builds for it: the 200, the content type, the key set,
/// and the `id` naming the status' own object uri.
#[tokio::test]
async fn a_public_note_is_served_as_its_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let note = common::create_status(&pool, &alice, "hello fediverse", Visibility::Public).await;
    let app = common::test_app(pool);

    let document = served_note(&app, &note).await;

    assert_eq!(
        document_keys(&document),
        NOTE_KEYS,
        "the documented field set"
    );
    assert_eq!(document["id"], note.uri, "the id is the object uri");
    assert_eq!(document["type"], "Note", "a plain status is a Note");
    assert_eq!(document["attributedTo"], alice.ap_id, "the author is named");
    assert_eq!(
        document["to"],
        json!(["https://www.w3.org/ns/activitystreams#Public"]),
        "a public note addresses the world"
    );
    assert_eq!(
        document["url"],
        note.url.expect("url"),
        "the page URL rides along"
    );
}

/// A status carrying a poll answers the same read as a `Question` with its
/// choices.
#[tokio::test]
async fn a_poll_status_answers_a_question() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let note = common::create_status(&pool, &alice, "tea or coffee?", Visibility::Public).await;
    attach_poll(&pool, &note, &["tea", "coffee"]).await;
    let app = common::test_app(pool);

    let document = served_note(&app, &note).await;

    assert_eq!(document["type"], "Question", "a poll is a Question");
    let one_of = document["oneOf"]
        .as_array()
        .expect("a single-choice poll answers oneOf");
    assert_eq!(one_of.len(), 2, "both options are there");
    assert_eq!(one_of[0]["name"], "tea", "the first option is named");
    assert_eq!(
        document["id"], note.uri,
        "and the id is still the object uri"
    );
}

/// An `unlisted` note is an ordinary public document: no signature, no
/// relationship, just the read.
#[tokio::test]
async fn an_unlisted_note_is_served_anonymously() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let note = common::create_status(&pool, &alice, "quietly public", Visibility::Unlisted).await;
    let app = common::test_app(pool);

    let document = served_note(&app, &note).await;

    assert_eq!(document["id"], note.uri, "unlisted is served like public");
    assert_eq!(document_keys(&document), NOTE_KEYS, "the same field set");
}

/// A `private` note: the anonymous read is the 404, a signed fetch by an
/// accepted follower is the document, and a signed fetch by a stranger who
/// follows nobody is the same 404 — no existence leak either way.
#[tokio::test]
async fn a_private_note_reaches_followers_that_sign() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let carol = signer_user(&pool, "carol").await;
    let mallory = signer_user(&pool, "mallory").await;
    accepted_follow(&pool, &carol, &alice).await;
    let note = common::create_status(&pool, &alice, "followers only", Visibility::Private).await;
    let app = common::test_app(pool);
    let path = object_path(&note);

    let anonymous = fetch_ap(&app, &path).await;
    assert_not_found(&anonymous, "an anonymous read of a private note");

    let follower = signed_get_as(&carol, &path);
    let granted = fetch_signed(&app, &path, &follower).await;
    assert_eq!(
        granted.status,
        StatusCode::OK,
        "an accepted follower's signed fetch is served"
    );
    assert_eq!(
        granted.json_value()["id"],
        note.uri,
        "and the document is the note"
    );

    let stranger = signed_get_as(&mallory, &path);
    assert_not_found(
        &fetch_signed(&app, &path, &stranger).await,
        "a signed fetch by a stranger",
    );
}

/// A `direct` note: a signed fetch by a mentioned user is served, and a
/// signed fetch by an accepted follower the post does not name is the
/// same 404 — the audience is the mentions, not the followers.
#[tokio::test]
async fn a_direct_note_reaches_the_mentioned_signer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let carol = signer_user(&pool, "carol").await;
    let bob = signer_user(&pool, "bob").await;
    accepted_follow(&pool, &bob, &alice).await;
    let note = addressed(&pool, &alice, &[&carol], Visibility::Direct, "psst, carol").await;
    let app = common::test_app(pool);
    let path = object_path(&note);

    let mentioned = signed_get_as(&carol, &path);
    let served = fetch_signed(&app, &path, &mentioned).await;
    assert_eq!(
        served.status,
        StatusCode::OK,
        "a mentioned signer reaches the note"
    );
    assert_eq!(served.json_value()["id"], note.uri, "the note itself");

    let follower = signed_get_as(&bob, &path);
    assert_not_found(
        &fetch_signed(&app, &path, &follower).await,
        "an unmentioned follower of a direct note",
    );
}

/// A `local`-visibility status is never an `ActivityPub` document: not
/// even a signed accepted follower's read escapes the 404.
#[tokio::test]
async fn a_local_status_is_never_a_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let carol = signer_user(&pool, "carol").await;
    accepted_follow(&pool, &carol, &alice).await;
    let note = common::create_status(&pool, &alice, "stay at home", Visibility::Local).await;
    let app = common::test_app(pool);
    let path = object_path(&note);

    assert_not_found(
        &fetch_ap(&app, &path).await,
        "an anonymous read of a local status",
    );

    let follower = signed_get_as(&carol, &path);
    assert_not_found(
        &fetch_signed(&app, &path, &follower).await,
        "a signed follower's read of a local status",
    );
}

/// A soft-deleted status answers 410, and the body is the `Tombstone`
/// document — the reserved id, the type it lived as, and the moment it
/// went — not the generic `{"error":"Gone"}` JSON.
#[tokio::test]
async fn a_deleted_status_is_a_tombstone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let note = common::create_status(&pool, &alice, "here and gone", Visibility::Public).await;
    let tombstone = status::soft_delete(&pool, note.id)
        .await
        .expect("the soft delete runs")
        .expect("the live row tombstones");
    let app = common::test_app(pool);

    let response = fetch_ap(&app, &object_path(&note)).await;

    assert_eq!(response.status, StatusCode::GONE, "the tombstone is a 410");
    assert_eq!(
        response.header("content-type"),
        Some(ACTIVITY_JSON),
        "the tombstone is an ActivityPub document"
    );
    assert_eq!(
        response.header("vary"),
        Some("Accept"),
        "the 410 varies on Accept"
    );
    let deleted = tombstone.deleted_at.expect("the row recorded its deletion");
    assert_eq!(
        response.json_value(),
        json!({
            "id": note.uri,
            "type": "Tombstone",
            "formerType": "Note",
            "deleted": mastodon_time(&deleted),
        }),
        "the tombstone body"
    );
}

/// The 404 arms of the core: a nickname that is not the live local author,
/// an unknown uuid, a non-uuid path segment, a row this instance does not
/// own, and a reblog row — every one answers `{"error":"Not found"}`.
#[tokio::test]
async fn the_core_misses_are_the_plain_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let note = common::create_status(&pool, &alice, "alice's own", Visibility::Public).await;
    let (reblog, _written) = status::create_reblog(&pool, BASE, &alice, &note, Visibility::Public)
        .await
        .expect("the reblog fixture writes");
    let mirror = remote_status(&pool, &bob, "mirror-1").await;
    let app = common::test_app(pool);

    assert_not_found(
        &fetch_ap(&app, &format!("/users/bob/statuses/{}", note.id)).await,
        "alice's note under bob's nickname",
    );
    assert_not_found(
        &fetch_ap(&app, &format!("/users/mallory/statuses/{}", note.id)).await,
        "a note under a nickname no account answers to",
    );
    assert_not_found(
        &fetch_ap(&app, &format!("/objects/{}", Uuid::now_v7())).await,
        "a uuid no row carries",
    );
    assert_not_found(
        &fetch_ap(&app, "/objects/not-a-uuid").await,
        "a path segment that is no uuid",
    );
    assert_not_found(
        &fetch_ap(&app, &format!("/objects/{}", mirror.id)).await,
        "a status this instance does not own",
    );
    assert_not_found(
        &fetch_ap(&app, &object_path(&reblog)).await,
        "a reblog row, which is an Announce, not a Note",
    );
}

/// The nickname-scoped twin serves the same document as the plain object
/// path when the nickname names the live local author.
#[tokio::test]
async fn the_scoped_path_serves_the_same_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let note =
        common::create_status(&pool, &alice, "through the nickname", Visibility::Public).await;
    let app = common::test_app(pool);

    let response = fetch_ap(&app, &format!("/users/alice/statuses/{}", note.id)).await;

    assert_eq!(
        response.status,
        StatusCode::OK,
        "the author's own scoped path serves the note"
    );
    assert_eq!(
        response.header("content-type"),
        Some(ACTIVITY_JSON),
        "the same document type"
    );
    assert_eq!(response.json_value()["id"], note.uri, "the same document");
}

/// `GET /notice/{id}` on the same core: an `ActivityPub` `Accept` gets the
/// note JSON, a browser's `text/html` gets the 302 to the same path on the
/// frontend origin, anything else — and an absent `Accept` — gets the 404.
/// Every answer varies on `Accept`.
#[tokio::test]
async fn the_notice_route_serves_or_redirects_by_accept() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let note = common::create_status(&pool, &alice, "a notice to serve", Visibility::Public).await;
    let app = common::test_app(pool);
    let path = format!("/notice/{}", note.id);

    let document = common::TestRequest::get(&path)
        .header("accept", "application/activity+json")
        .send(&app)
        .await;
    assert_eq!(
        document.status,
        StatusCode::OK,
        "an ActivityPub ask gets the note JSON"
    );
    assert_eq!(
        document.header("content-type"),
        Some(ACTIVITY_JSON),
        "as the same document"
    );
    assert_eq!(
        document.json_value()["id"],
        note.uri,
        "the object id, not the notice path"
    );

    let browser = common::TestRequest::get(&path)
        .header(
            "accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .send(&app)
        .await;
    assert_eq!(
        browser.status,
        StatusCode::FOUND,
        "a browser is redirected to the frontend"
    );
    assert_eq!(
        browser.header("location"),
        Some(format!("https://test.example/notice/{}", note.id).as_str()),
        "the same path on the frontend origin"
    );
    assert_eq!(
        browser.header("vary"),
        Some("Accept"),
        "the 302 varies on Accept"
    );

    assert_not_found(
        &common::TestRequest::get(&path)
            .header("accept", "application/json")
            .send(&app)
            .await,
        "a bare JSON ask on /notice",
    );

    let deleted = format!("/notice/{}", Uuid::now_v7());
    assert_not_found(
        &fetch_ap(&app, &deleted).await,
        "a /notice ask for an unknown status",
    );
}

/// `Accept: text/html` is a browser on `/notice/{id}` and nothing at all
/// on the object paths: an object read never redirects, the 404 answers.
/// An absent `Accept` states nothing and gets the same 404.
#[tokio::test]
async fn browsers_are_not_redirected_from_the_object_paths() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let note = common::create_status(&pool, &alice, "no browser here", Visibility::Public).await;
    let app = common::test_app(pool);
    let path = object_path(&note);

    assert_not_found(
        &common::TestRequest::get(&path)
            .header("accept", "text/html")
            .send(&app)
            .await,
        "a browser's read of an object",
    );
    assert_not_found(
        &common::TestRequest::get(&format!("/users/alice/statuses/{}", note.id))
            .header("accept", "text/html")
            .send(&app)
            .await,
        "a browser's read on the scoped path",
    );
    assert_not_found(
        &common::TestRequest::get(&path).send(&app).await,
        "an object read with no Accept at all",
    );
}

/// With `AUTHORIZED_FETCH` on the object reads demand the signature the
/// actor route already demands: the unsigned read of a public note is the
/// 401 refusal, a signed one is served. The notice redirect is not gated —
/// the negotiation answers before the gate does.
#[tokio::test]
async fn the_authorized_fetch_gate_gates_the_object_reads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = common::test_config();
    config.fetch.authorized_fetch = Toggle::new(true);
    let alice = common::create_local_user(&pool, "alice").await;
    let note = common::create_status(&pool, &alice, "gated note", Visibility::Public).await;
    let app = common::test_app_with_config(pool.clone(), config);
    let path = object_path(&note);

    let unsigned = fetch_ap(&app, &path).await;
    assert_eq!(
        unsigned.status,
        StatusCode::UNAUTHORIZED,
        "the unsigned read is refused"
    );
    assert_eq!(
        unsigned.json_value()["error"],
        REFUSAL,
        "with the gate's refusal wording"
    );
    assert_eq!(
        unsigned.header("vary"),
        Some("Accept"),
        "even the 401 varies on Accept"
    );

    let actor = instance_signer(&pool).await;
    let signed = signed_get_as_instance(&actor, &path);
    let response = fetch_signed(&app, &path, &signed).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "a signed read unlocks the document"
    );
    assert_eq!(response.json_value()["id"], note.uri, "and it is the note");

    let browser = common::TestRequest::get(&format!("/notice/{}", note.id))
        .header("accept", "text/html")
        .send(&app)
        .await;
    assert_eq!(
        browser.status,
        StatusCode::FOUND,
        "the content negotiation answers before the gate does, and the \
         frontend redirect needs no signature"
    );
    assert_eq!(
        browser.header("location"),
        Some(format!("https://test.example/notice/{}", note.id).as_str()),
        "the redirect is the ordinary frontend notice URL"
    );
}

/// A frontend on its own origin: with `FRONTEND_BASE_URL` configured,
/// the browser redirect names that origin, not `BASE_URL`.
#[tokio::test]
async fn the_notice_redirect_follows_the_frontend_origin() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = common::test_config();
    config.frontend_base_url = "https://frontend.example".to_owned();
    let alice = common::create_local_user(&pool, "alice").await;
    let note = common::create_status(&pool, &alice, "a frontend notice", Visibility::Public).await;
    let app = common::test_app_with_config(pool, config);

    let response = common::TestRequest::get(&format!("/notice/{}", note.id))
        .header("accept", "text/html")
        .send(&app)
        .await;

    assert_eq!(response.status, StatusCode::FOUND, "a browser redirects");
    assert_eq!(
        response.header("location"),
        Some(format!("https://frontend.example/notice/{}", note.id).as_str()),
        "to the configured frontend origin"
    );
}

/// The instance's own test configuration defaults the frontend origin onto
/// the base URL, which every redirect test above relies on.
#[test]
fn the_test_frontend_origin_is_the_test_origin() {
    let config = Config::for_tests();
    assert_eq!(
        config.frontend_base_url, config.base_url,
        "FRONTEND_BASE_URL defaults to BASE_URL"
    );
    assert_eq!(
        config.base_url, BASE,
        "and the fixture origin is the one the assertions above name"
    );
}
