//! Tests for the `ActivityPub` routes: `GET /users/{nickname}`, the actor
//! document, the JSON-LD schema document at
//! `GET /schemas/litepub-0.1.jsonld`, the three inbox `POST`s, and the
//! client-to-server four — `GET /users/{nickname}/inbox`,
//! `POST /users/{nickname}/outbox`, `GET /api/ap/whoami` and
//! `POST /api/ap/upload_media`.
//!
//! An `ActivityPub` client (an `Accept` naming `application/activity+json`
//! or `application/ld+json`) gets Pleroma's `user.json` document as
//! `application/activity+json; charset=utf-8`; a browser (`Accept` naming
//! `text/html`, no `ActivityPub` type) gets Pleroma's 302 to the frontend
//! profile URL; anyone else gets 404 `{"error":"Not found"}`. An unknown
//! or remote account gets the same 404 to an `ActivityPub` ask, and a
//! deactivated local account gets 410 Gone. Every answer says it varies
//! on `Accept`.
//!
//! The inboxes (`POST /inbox`, `POST /users/{nickname}/inbox` and
//! `POST /internal/fetch/inbox`) accept a signed delivery with 202 and an
//! empty body and queue exactly one `federator.incoming` job for it —
//! once per activity id however often it is redelivered — while an unknown,
//! remote or deactivated owner is the 404, an activity without the
//! required fields is the 400, an unsigned delivery the extractor's 401,
//! and every delivery a `FEDERATION_ENABLED`-off 403.
//!
//! The renderer's corner cases are covered where they live, in
//! `src/federation/actor.rs`; what these tests own is the route: the
//! negotiation, the headers, the status codes and the document of a stored
//! user end to end. The signature codec is covered in
//! `src/federation/signature/`; what these tests own here is that an
//! authenticated delivery becomes a queue row.

#![cfg(test)]

use axum::{Router, body::Bytes, http::StatusCode};
use chrono::Utc;
use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::io::Cursor;
use tempfile::TempDir;
use uuid::Uuid;
use wired_be::{
    config::Toggle,
    federation::{
        dispatch::KIND_INCOMING,
        signature::{SigningKey, sign_request},
    },
    jobs::QUEUE_FEDERATOR_INCOMING,
    models::{
        instance_setting,
        job::Job,
        status::{self, NewStatus, Status as StoredStatus, Visibility},
        user::{Flag, NewUser, User, followers},
    },
    services::instance_actor::ensure_instance_actor,
    util::time::mastodon_time,
};

use crate::common;

/// The media type of an `ActivityPub` document, charset included.
const ACTIVITY_JSON: &str = "application/activity+json; charset=utf-8";
/// The instance origin of [`common::test_config`].
const BASE: &str = "https://test.example";
/// The host of [`BASE`]: the one a delivery here is signed from, and the
/// one the reject lists below name.
const HOST: &str = "test.example";
/// Public key material a fixture row can be checked against.
const PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----\nMIIB-FIXTURE\n-----END PUBLIC KEY-----\n";
/// Every key of an actor document that carries neither avatar nor banner.
const ACTOR_KEYS: [&str; 21] = [
    "@context",
    "alsoKnownAs",
    "attachment",
    "capabilities",
    "discoverable",
    "endpoints",
    "featured",
    "followers",
    "following",
    "id",
    "inbox",
    "manuallyApprovesFollowers",
    "name",
    "outbox",
    "preferredUsername",
    "publicKey",
    "published",
    "summary",
    "tag",
    "type",
    "url",
];

/// Fetches `path` as an `ActivityPub` client.
async fn fetch_activity_json(app: &Router, path: &str) -> common::TestResponse {
    common::TestRequest::get(path)
        .header("accept", "application/activity+json")
        .send(app)
        .await
}

/// The actor document of `user`, fetched over the route, asserted to be a
/// served actor before the body goes back to the test.
async fn fetch_actor(pool: &PgPool, user: &User) -> Value {
    let app = common::test_app(pool.clone());
    let response = fetch_activity_json(&app, &format!("/users/{}", user.nickname)).await;
    assert_eq!(response.status, StatusCode::OK, "a live local actor");
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

/// The documented key set, extended with `extra` and sorted, for the
/// assertions that add the media fields.
fn keys_with(extra: &[&'static str]) -> Vec<&'static str> {
    let mut keys: Vec<&str> = ACTOR_KEYS.to_vec();
    keys.extend_from_slice(extra);
    keys.sort_unstable();
    keys
}

/// The sorted key set of `document`.
fn key_set(document: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = document
        .as_object()
        .expect("an actor document is a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// `GET /schemas/litepub-0.1.jsonld` — the JSON-LD context document the
/// `@context` of the `Note`/`Question` and activity documents names. It is
/// one fixed document served to anyone, no `Accept` negotiation and no
/// signature, holding the vocabulary terms those documents actually use.
#[tokio::test]
async fn the_litepub_schema_document_is_served() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/schemas/litepub-0.1.jsonld")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the schema is served");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "as JSON, like Pleroma's static file"
    );
    let document = response.json_value();
    let context = document
        .get("@context")
        .and_then(Value::as_object)
        .expect("the document is a term map under @context");
    assert_eq!(
        context.get("conversation"),
        Some(&json!({"@id": "ostatus:conversation", "@type": "@id"})),
        "the ostatus conversation reference every note carries"
    );
    assert_eq!(
        context.get("EmojiReact"),
        Some(&json!("litepub:EmojiReact")),
        "the litepub terms"
    );
    assert_eq!(
        context.get("capabilities"),
        Some(&json!("litepub:capabilities")),
        "…all of them…"
    );
    assert_eq!(
        context.get("quoteUrl"),
        Some(&json!("as:quoteUrl")),
        "…the quote pair…"
    );
    assert_eq!(
        context.get("quoteUri"),
        Some(&json!("fedibird:quoteUri")),
        "…"
    );
    assert_eq!(
        context.get("votersCount"),
        Some(&json!("toot:votersCount")),
        "…the poll count…"
    );
    assert_eq!(
        context.get("sensitive"),
        Some(&json!("as:sensitive")),
        "…the content-warning flag…"
    );
    assert_eq!(
        context.get("Hashtag"),
        Some(&json!("as:Hashtag")),
        "…and the tag type"
    );
}

/// The whole document a provisioned local account is served as: every key
/// and value of Pleroma's `user.json`, end to end over HTTP. The `@context`
/// is taken from the response because its exact terms are the renderer's
/// own test's business.
#[tokio::test]
async fn a_local_user_is_served_as_its_actor_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |new| NewUser {
        name: "Alice".to_owned(),
        bio: "<p>hello</p>".to_owned(),
        raw_bio: "hello".to_owned(),
        discoverable: Flag::new(true),
        public_key: Some(PUBLIC_PEM.to_owned()),
        fields: json!([
            {"name": "site", "value": "<a href=\"https://alice.example\">alice.example</a>"}
        ]),
        ..new
    })
    .await;

    let document = fetch_actor(&pool, &alice).await;
    let context = document["@context"].clone();
    assert_eq!(
        document,
        json!({
            "@context": context,
            "id": format!("{BASE}/users/alice"),
            "type": "Person",
            "preferredUsername": "alice",
            "name": "Alice",
            "summary": "<p>hello</p>",
            "url": format!("{BASE}/users/alice"),
            "inbox": format!("{BASE}/users/alice/inbox"),
            "outbox": format!("{BASE}/users/alice/outbox"),
            "followers": format!("{BASE}/users/alice/followers"),
            "following": format!("{BASE}/users/alice/following"),
            "featured": format!("{BASE}/users/alice/collections/featured"),
            "endpoints": {
                "sharedInbox": format!("{BASE}/inbox"),
                "oauthAuthorizationEndpoint": format!("{BASE}/oauth/authorize"),
                "oauthTokenEndpoint": format!("{BASE}/oauth/token"),
                "oauthRegistrationEndpoint": format!("{BASE}/api/v1/apps"),
            },
            "publicKey": {
                "id": format!("{BASE}/users/alice#main-key"),
                "owner": format!("{BASE}/users/alice"),
                "publicKeyPem": PUBLIC_PEM,
            },
            "manuallyApprovesFollowers": false,
            "discoverable": true,
            "attachment": [{
                "type": "PropertyValue",
                "name": "site",
                "value": "<a href=\"https://alice.example\">alice.example</a>",
            }],
            "published": mastodon_time(&alice.inserted_at),
            "tag": [],
            "alsoKnownAs": [],
            "capabilities": {"acceptsChatMessages": true},
        }),
        "the actor document of a provisioned local user"
    );
}

/// A user with nothing filled in still renders every documented key — and
/// neither `icon` nor `image`.
#[tokio::test]
async fn a_bare_user_has_the_documented_key_set() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let document = fetch_actor(&pool, &alice).await;
    assert_eq!(key_set(&document), ACTOR_KEYS, "exact key set");
    assert_eq!(
        document["endpoints"],
        json!({
            "sharedInbox": format!("{BASE}/inbox"),
            "oauthAuthorizationEndpoint": format!("{BASE}/oauth/authorize"),
            "oauthTokenEndpoint": format!("{BASE}/oauth/token"),
            "oauthRegistrationEndpoint": format!("{BASE}/api/v1/apps"),
        }),
        "the endpoints are the instance's own"
    );
    assert_eq!(
        document["publicKey"],
        json!({
            "id": format!("{BASE}/users/alice#main-key"),
            "owner": format!("{BASE}/users/alice"),
            "publicKeyPem": Value::Null,
        }),
        "a row without key material keeps the key id and owner"
    );
    assert_eq!(
        document["published"],
        mastodon_time(&alice.inserted_at),
        "published is the insert time in the wire format"
    );
    assert_eq!(
        document["capabilities"],
        json!({"acceptsChatMessages": true}),
        "the litepub capabilities object"
    );
}

/// `locked` is what `manuallyApprovesFollowers` means; it changes no key.
#[tokio::test]
async fn a_locked_user_approves_followers_manually() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |new| NewUser {
        locked: Flag::new(true),
        ..new
    })
    .await;
    let document = fetch_actor(&pool, &alice).await;
    assert_eq!(
        document["manuallyApprovesFollowers"], true,
        "the stored flag is the ActivityPub field"
    );
    assert_eq!(key_set(&document), ACTOR_KEYS, "the key set is stable");
}

/// A bot federates as its `actor_type`: Pleroma emits no `bot` key, so the
/// type is what says it.
#[tokio::test]
async fn a_bot_is_federated_by_its_actor_type() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |new| NewUser {
        actor_type: "Service".to_owned(),
        bot: Flag::new(true),
        ..new
    })
    .await;
    let document = fetch_actor(&pool, &alice).await;
    assert_eq!(document["type"], "Service", "the stored actor type");
    assert_eq!(document["preferredUsername"], "alice", "still alice");
    assert!(
        document.get("bot").is_none(),
        "the bot flag itself is not federated"
    );
    assert_eq!(key_set(&document), ACTOR_KEYS, "the key set is stable");
}

/// An avatar becomes `icon` and a banner `image`, each an `Image` object;
/// an account with only an avatar gets only `icon`.
#[tokio::test]
async fn an_avatar_user_carries_icon_and_image() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |new| NewUser {
        avatar_url: Some(format!("{BASE}/avatar.png")),
        banner_url: Some(format!("{BASE}/banner.png")),
        ..new
    })
    .await;
    let document = fetch_actor(&pool, &alice).await;
    assert_eq!(
        document["icon"],
        json!({"type": "Image", "url": format!("{BASE}/avatar.png")}),
        "the avatar is an Image"
    );
    assert_eq!(
        document["image"],
        json!({"type": "Image", "url": format!("{BASE}/banner.png")}),
        "the banner is an Image"
    );
    assert_eq!(
        key_set(&document),
        keys_with(&["icon", "image"]),
        "media adds its keys"
    );

    let carol = common::create_local_user_with(&pool, "carol", |new| NewUser {
        avatar_url: Some(format!("{BASE}/avatar.png")),
        ..new
    })
    .await;
    let carol_document = fetch_actor(&pool, &carol).await;
    assert_eq!(
        key_set(&carol_document),
        keys_with(&["icon"]),
        "an avatar alone adds only icon"
    );
}

/// `application/ld+json` with the `ActivityStreams` profile is an
/// `ActivityPub` client, and so is an `Accept` that names it among others.
#[tokio::test]
async fn ld_json_clients_get_the_actor_too() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    for accept in [
        "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"",
        "application/ld+json",
        "text/html, application/activity+json;q=0.9",
    ] {
        let response = common::TestRequest::get(&format!("/users/{}", alice.nickname))
            .header("accept", accept)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{accept} names an ActivityPub media type"
        );
        assert_eq!(
            response.header("content-type"),
            Some(ACTIVITY_JSON),
            "{accept} gets the ActivityPub media type"
        );
        assert_eq!(
            response.header("vary"),
            Some("Accept"),
            "{accept} sees the negotiation"
        );
        assert_eq!(
            response.json_value()["id"],
            format!("{BASE}/users/alice"),
            "{accept} gets alice's document"
        );
    }
}

/// A browser is not an `ActivityPub` client: it gets Pleroma's redirect to
/// the frontend profile page, 302 with `Location: {BASE_URL}/{nickname}`.
/// A client that asks for neither `ActivityPub` nor HTML still gets the
/// router-level 404 (this backend is API-only and serves no HTML, decision
/// 1).
#[tokio::test]
async fn a_browser_is_redirected_to_the_frontend() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    for accept in ["text/html", "text/html,application/xhtml+xml"] {
        let response = common::TestRequest::get(&format!("/users/{}", alice.nickname))
            .header("accept", accept)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::FOUND,
            "{accept} is redirected, not served a document"
        );
        assert_eq!(
            response.header("location"),
            Some("https://test.example/alice"),
            "{accept} is sent to the frontend profile URL"
        );
        assert_eq!(
            response.header("vary"),
            Some("Accept"),
            "the redirect for {accept} says it varies on Accept"
        );
    }
    for accept in ["*/*", "application/json", "application/activity+xml"] {
        let response = common::TestRequest::get(&format!("/users/{}", alice.nickname))
            .header("accept", accept)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{accept} is neither an ActivityPub client nor a browser"
        );
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "the 404 for {accept} is JSON"
        );
        assert_eq!(
            response.header("vary"),
            Some("Accept"),
            "the 404 for {accept} says it varies on Accept"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Not found"}),
            "the fallback wording for {accept}"
        );
    }
}

/// An unknown nickname and a remote mirror are both 404s: this route serves
/// this instance's actors only (decision 7).
#[tokio::test]
async fn unknown_and_remote_users_are_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let app = common::test_app(pool.clone());
    for path in ["/users/nobody", &format!("/users/{}", bob.nickname)] {
        let response = fetch_activity_json(&app, path).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{path} is not local"
        );
        assert_eq!(
            response.header("vary"),
            Some("Accept"),
            "{path} says it varies on Accept"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Not found"}),
            "{path} keeps the fallback body"
        );
    }
}

/// A deactivated local account is 410 Gone, so a remote instance stops
/// refetching it; a browser still gets the content-negotiation 404.
#[tokio::test]
async fn a_deactivated_user_is_gone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |new| NewUser {
        is_active: Flag::new(false),
        ..new
    })
    .await;
    let app = common::test_app(pool.clone());
    let response = fetch_activity_json(&app, &format!("/users/{}", alice.nickname)).await;
    assert_eq!(
        response.status,
        StatusCode::GONE,
        "a deactivated local account is gone"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the 410 is JSON like every error"
    );
    assert_eq!(
        response.header("vary"),
        Some("Accept"),
        "the 410 says it varies on Accept"
    );
    assert_eq!(
        response.json_value(),
        json!({"error": "Gone"}),
        "the 410 body follows decision 8"
    );

    let browser = common::TestRequest::get(&format!("/users/{}", alice.nickname))
        .header("accept", "text/html")
        .send(&app)
        .await;
    assert_eq!(
        browser.status,
        StatusCode::FOUND,
        "a browser is redirected to the frontend, gone or not — the \
         frontend decides what to show there"
    );
    assert_eq!(
        browser.header("location"),
        Some("https://test.example/alice"),
        "the redirect is the ordinary frontend profile URL"
    );
}

/// The signer of the inbox tests' deliveries: the instance actor, a stored
/// local row carrying the key pair [`ensure_instance_actor`] generated, so
/// the `SignedActivity` extractor resolves the `keyId` from the database
/// and no mock peer is needed. Every delivered activity names its `ap_id`
/// as `actor` — the containment the extractor insists on.
#[derive(Debug)]
struct Signer {
    /// `users.id` of the signing actor.
    id: Uuid,
    /// Its actor id, the `actor` every test activity carries.
    ap_id: String,
    /// The `keyId`: actor id plus Mastodon's key fragment.
    key_id: String,
    /// The private key the deliveries are signed with.
    private_pem: String,
}

/// Provisions the instance actor and hands it over as a delivery signer.
async fn instance_signer(pool: &PgPool) -> Signer {
    let config = common::test_config();
    let actor = ensure_instance_actor(pool, &config)
        .await
        .expect("the instance actor is provisioned");
    Signer {
        id: actor.id,
        key_id: format!("{}#main-key", actor.ap_id),
        ap_id: actor.ap_id,
        private_pem: actor
            .private_key
            .expect("the provisioned actor holds its private key"),
    }
}

/// The `Follow` of alice carrying activity id `id`.
fn follow(signer: &Signer, id: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": id,
        "type": "Follow",
        "actor": signer.ap_id,
        "object": format!("{BASE}/users/alice"),
    })
}

/// `POST`s `activity` to `path` the way a peer delivers: the exact bytes
/// serialized here are digested, signed and sent, and the `host`, `date`,
/// `digest` and `signature` headers are what [`sign_request`] produced for
/// that path as request target.
async fn deliver(
    app: &Router,
    signer: &Signer,
    path: &str,
    activity: &Value,
) -> common::TestResponse {
    let wire = sign_request(
        "POST",
        &format!("{BASE}{path}"),
        Some(&serde_json::to_vec(activity).expect("the test activity serializes")),
        &SigningKey {
            key_id: &signer.key_id,
            private_pem: &signer.private_pem,
        },
        Utc::now(),
    )
    .expect("signing the test delivery");
    common::TestRequest::post(path)
        .json(activity)
        .header("host", &wire.host)
        .header("date", &wire.date)
        .header("digest", wire.digest.as_deref().expect("a body digest"))
        .header("signature", &wire.signature)
        .send(app)
        .await
}

/// Every [`KIND_INCOMING`] job the database holds, oldest first.
async fn incoming_jobs(pool: &PgPool) -> Vec<Job> {
    sqlx::query_as::<_, Job>("SELECT * FROM jobs WHERE kind = $1 ORDER BY id")
        .bind(KIND_INCOMING)
        .fetch_all(pool)
        .await
        .expect("the jobs table reads back")
}

/// The three inbox routes with the `inbox_owner` each queues: the local
/// account's id for the personal inbox, null for the shared ones.
fn inbox_routes(alice: &User) -> [(&'static str, Option<Uuid>); 3] {
    [
        ("/inbox", None),
        ("/users/alice/inbox", Some(alice.id)),
        ("/internal/fetch/inbox", None),
    ]
}

/// A valid signed delivery to each of the three inbox routes answers 202
/// with an empty body and queues exactly one `federator.incoming` job
/// carrying the activity, the signer and the route's owner.
#[tokio::test]
async fn each_inbox_route_queues_one_job_per_delivery() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let signer = instance_signer(&pool).await;
    let app = common::test_app(pool.clone());

    let mut activities = Vec::new();
    for (n, (path, _owner)) in inbox_routes(&alice).into_iter().enumerate() {
        let activity = follow(&signer, &format!("https://peer.example/activities/{n}"));
        let response = deliver(&app, &signer, path, &activity).await;
        assert_eq!(
            response.status,
            StatusCode::ACCEPTED,
            "{path} accepts a signed delivery"
        );
        assert_eq!(response.text(), "", "{path} answers with an empty body");
        activities.push(activity);
    }

    let jobs = incoming_jobs(&pool).await;
    assert_eq!(jobs.len(), 3, "one job per route, no extras");
    for (n, (job, (_path, owner))) in jobs.iter().zip(inbox_routes(&alice)).enumerate() {
        assert_eq!(job.queue, QUEUE_FEDERATOR_INCOMING, "the incoming queue");
        assert_eq!(job.kind, KIND_INCOMING, "the incoming kind");
        assert_eq!(job.max_attempts, 5, "the ticket's attempt budget");
        assert_eq!(
            job.unique_key.as_deref(),
            Some(format!("incoming:https://peer.example/activities/{n}").as_str()),
            "the dedup key names the activity"
        );
        assert_eq!(
            job.args["signer_id"],
            signer.id.to_string(),
            "the authenticated signer is recorded"
        );
        assert_eq!(
            job.args["inbox_owner"],
            owner.map_or(Value::Null, |id| Value::from(id.to_string())),
            "the owner is the route's, or null for a shared inbox"
        );
        assert_eq!(
            job.args["activity"], activities[n],
            "the activity rides along"
        );
    }
}

/// The same activity delivered twice is queued once: both deliveries are
/// accepted, the dedup key leaves exactly one job.
#[tokio::test]
async fn a_redelivered_activity_is_queued_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let signer = instance_signer(&pool).await;
    let app = common::test_app(pool.clone());
    let activity = follow(&signer, "https://peer.example/activities/only-once");

    let first = deliver(&app, &signer, "/inbox", &activity).await;
    let second = deliver(&app, &signer, "/inbox", &activity).await;

    assert_eq!(first.status, StatusCode::ACCEPTED, "the first delivery");
    assert_eq!(
        second.status,
        StatusCode::ACCEPTED,
        "the redelivery is accepted too"
    );
    let jobs = incoming_jobs(&pool).await;
    assert_eq!(jobs.len(), 1, "but queued once: {jobs:?}");
    assert_eq!(
        jobs[0].unique_key.as_deref(),
        Some("incoming:https://peer.example/activities/only-once"),
        "the dedup key is the activity's id"
    );
}

/// A personal inbox exists only for a live local account: an unknown
/// nickname, a remote mirror's handle and a deactivated account's name all
/// answer the plain 404, and queue nothing.
#[tokio::test]
async fn an_inbox_of_a_non_local_or_gone_user_is_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let ex = common::create_local_user_with(&pool, "ex", |new| NewUser {
        is_active: Flag::new(false),
        ..new
    })
    .await;
    let signer = instance_signer(&pool).await;
    let app = common::test_app(pool.clone());

    for path in [
        "/users/nobody/inbox",
        &format!("/users/{}/inbox", bob.nickname),
        &format!("/users/{}/inbox", ex.nickname),
    ] {
        let response = deliver(
            &app,
            &signer,
            path,
            &follow(&signer, "https://peer.example/activities/x"),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{path} names no live local account"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Not found"}),
            "{path} keeps the fallback body"
        );
    }
    assert!(
        incoming_jobs(&pool).await.is_empty(),
        "a refused owner queues no job"
    );
}

/// An authenticated activity must carry a string `id` and a string `type`
/// (the extractor already vouched for the `actor`): either one missing is
/// the 400 `{"error":"Invalid activity"}`, and nothing is queued.
#[tokio::test]
async fn an_activity_without_id_or_type_is_invalid() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let signer = instance_signer(&pool).await;
    let app = common::test_app(pool.clone());

    let no_type = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://peer.example/activities/no-type",
        "actor": signer.ap_id,
        "object": format!("{BASE}/users/alice"),
    });
    let no_id = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Create",
        "actor": signer.ap_id,
        "object": format!("{BASE}/users/alice"),
    });
    for (name, activity) in [("type", &no_type), ("id", &no_id)] {
        let response = deliver(&app, &signer, "/inbox", activity).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "an activity without a {name} is refused"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Invalid activity"}),
            "the 400 body names the problem"
        );
    }
    assert!(
        incoming_jobs(&pool).await.is_empty(),
        "an invalid activity queues no job"
    );
}

/// An unsigned delivery never reaches a handler: the extractor answers 401
/// on all three routes, and nothing is queued.
#[tokio::test]
async fn an_unsigned_delivery_is_not_authenticated() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let signer = instance_signer(&pool).await;
    let app = common::test_app(pool.clone());
    let activity = follow(&signer, "https://peer.example/activities/unsigned");

    for path in ["/inbox", "/users/alice/inbox", "/internal/fetch/inbox"] {
        let response = common::TestRequest::post(path)
            .json(&activity)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{path} demands a signature"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Missing signature"}),
            "{path} says what is missing"
        );
    }
    assert!(
        incoming_jobs(&pool).await.is_empty(),
        "an unsigned delivery queues no job"
    );
}

/// With `FEDERATION_ENABLED=false` every inbox route answers 403
/// `{"error":"Federation is disabled"}` — unsigned deliveries included,
/// because the gate runs before authentication — and queues nothing.
#[tokio::test]
async fn a_disabled_federation_refuses_every_inbox_post() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let signer = instance_signer(&pool).await;
    let mut config = common::test_config();
    config.federation_enabled = Toggle::new(false);
    let app = common::test_app_with_config(pool.clone(), config);

    for path in ["/inbox", "/users/alice/inbox", "/internal/fetch/inbox"] {
        let signed_response = deliver(
            &app,
            &signer,
            path,
            &follow(&signer, "https://peer.example/activities/off"),
        )
        .await;
        assert_eq!(
            signed_response.status,
            StatusCode::FORBIDDEN,
            "{path} is shut while federation is disabled"
        );
        assert_eq!(
            signed_response.json_value(),
            json!({"error": "Federation is disabled"}),
            "{path} says why"
        );

        let unsigned_response = common::TestRequest::post(path)
            .json(&follow(
                &signer,
                "https://peer.example/activities/off-unsigned",
            ))
            .send(&app)
            .await;
        assert_eq!(
            unsigned_response.status,
            StatusCode::FORBIDDEN,
            "{path} refuses unsigned deliveries at the same gate"
        );
    }
    assert!(
        incoming_jobs(&pool).await.is_empty(),
        "a disabled instance queues no activity at all"
    );
}

/// The `mrf` domain lists gate the inbox the way the instance's own policy
/// table does: a delivery signed by an actor whose host `simple` rejects —
/// or one its `accept` allowlist leaves out — is refused with 403
/// `{"error":"Rejected"}` and queues nothing, while the same lists behind a
/// policy that is not enabled let it through.
#[tokio::test]
async fn a_delivery_from_an_mrf_rejected_host_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let signer = instance_signer(&pool).await;

    for lists in [
        json!({"reject": [[HOST, "bots"]]}),
        json!({"accept": [["other.example", "the only peer"]]}),
    ] {
        set_mrf(&pool, &json!({"policies": ["simple"], "simple": lists})).await;
        let app = common::test_app(pool.clone());
        let response = deliver(
            &app,
            &signer,
            "/inbox",
            &follow(&signer, "https://peer.example/activities/blocked"),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "the lists refuse the signer's host: {lists}"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Rejected"}),
            "the same body the policy table gives"
        );
    }
    assert!(
        incoming_jobs(&pool).await.is_empty(),
        "a refused delivery queues no job"
    );

    set_mrf(
        &pool,
        &json!({"policies": ["noop"], "simple": {"reject": [HOST]}}),
    )
    .await;
    let app = common::test_app(pool.clone());
    let allowed = deliver(
        &app,
        &signer,
        "/inbox",
        &follow(&signer, "https://peer.example/activities/allowed"),
    )
    .await;
    assert_eq!(
        allowed.status,
        StatusCode::ACCEPTED,
        "lists behind a policy that is not enabled govern nothing"
    );
    assert_eq!(incoming_jobs(&pool).await.len(), 1, "and the job is queued");
}

/// Writes the `mrf` instance setting: the pipeline reads it (uncached: each
/// phase of the test builds a fresh state) as its policy chain.
async fn set_mrf(pool: &PgPool, setting: &Value) {
    instance_setting::set(pool, "mrf", setting)
        .await
        .expect("the mrf setting writes");
}

// ---------------------------------------------------------------------------
// Client-to-server: the owner's inbox read, outbox writes, whoami and the
// C2S media upload (Pleroma's `read_inbox`, `update_outbox`, `whoami` and
// `upload_media`).
// ---------------------------------------------------------------------------

/// Boundary the C2S upload bodies are framed with.
const C2S_BOUNDARY: &str = "wired-c2s-test-boundary";
/// The `ActivityStreams` public collection.
const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// A local account named `nickname` with a bearer token granting `scopes`.
async fn c2s_account(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, scopes).await;
    (user, token)
}

/// A status by `author` that @-mentions `mentioned` — the fact the C2S
/// inbox pages on — inserted straight through the model.
async fn mention_status(
    pool: &PgPool,
    author: &User,
    mentioned: &User,
    text: &str,
    visibility: Visibility,
) -> StoredStatus {
    let new = NewStatus {
        visibility,
        mentioned_user_ids: vec![mentioned.id],
        ..NewStatus::local(author, text)
    };
    status::insert(pool, BASE, &new)
        .await
        .expect("the mention fixture inserts")
}

/// `GET /users/{nickname}/inbox` as `token`.
async fn read_inbox(
    app: &Router,
    nickname: &str,
    token: Option<&str>,
    query: &str,
) -> common::TestResponse {
    let path = if query.is_empty() {
        format!("/users/{nickname}/inbox")
    } else {
        format!("/users/{nickname}/inbox?{query}")
    };
    let request = common::TestRequest::get(&path);
    let request = match token {
        Some(token) => request.bearer(token),
        None => request,
    };
    request.send(app).await
}

/// Posts `activity` to `/users/{nickname}/outbox` as `token` (none: the
/// anonymous ask).
async fn post_outbox(
    app: &Router,
    nickname: &str,
    token: Option<&str>,
    activity: Value,
) -> common::TestResponse {
    let request = common::TestRequest::post(&format!("/users/{nickname}/outbox"));
    let request = match token {
        Some(token) => request.bearer(token).json(&activity),
        None => request.json(&activity),
    };
    request.send(app).await
}

/// The `Create` of a plain note, addressed as `to` says.
fn create_note(to: &Value, cc: &Value, content: &str) -> Value {
    json!({
        "type": "Create",
        "to": to,
        "cc": cc,
        "object": { "type": "Note", "content": content },
    })
}

/// The status id an outbox answer's `Location` names: the tail of the
/// `{BASE}/activities/{uuid}` address.
fn location_status_id(response: &common::TestResponse) -> Uuid {
    let location = response
        .header("location")
        .expect("a 201 carries a Location");
    let tail = location
        .strip_prefix(&format!("{BASE}/activities/"))
        .unwrap_or(location);
    Uuid::parse_str(tail).expect("the Location tail is the minted id")
}

/// `GET /api/v1/statuses/{id}` as `token`.
async fn show_status(app: &Router, token: &str, id: Uuid) -> common::TestResponse {
    common::TestRequest::get(&format!("/api/v1/statuses/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// `GET /api/v1/accounts/relationships?id={target}` as `token`, answered
/// for the one account it names.
async fn relationship(app: &Router, token: &str, target: &User) -> Value {
    let response =
        common::TestRequest::get(&format!("/api/v1/accounts/relationships?id={}", target.id))
            .bearer(token)
            .send(app)
            .await;
    assert_eq!(response.status, StatusCode::OK, "the relationship reads");
    let list = response.json_value();
    list.as_array()
        .expect("relationships answer a list")
        .first()
        .expect("one relationship was asked for")
        .clone()
}

/// A decodable 8x8 `PNG` of `seed`-shifted colors, as bytes.
fn png_fixture(seed: u32) -> Vec<u8> {
    let channel = |value: u32| u8::try_from(value % 256).expect("a modulo of 256 fits a u8");
    let image: RgbaImage = ImageBuffer::from_fn(8, 8, |x, y| {
        Rgba([channel(x + seed), channel(y * 7), 128, 255])
    });
    let mut out = Vec::new();
    DynamicImage::ImageRgba8(image)
        .write_to(Cursor::new(&mut out), ImageFormat::Png)
        .expect("the fixture encodes");
    out
}

/// A `POST /api/ap/upload_media` with one `file` part and a description.
async fn upload_to_ap(
    app: &Router,
    token: Option<&str>,
    bytes: Vec<u8>,
    description: Option<&'static str>,
) -> common::TestResponse {
    let mut parts = vec![common::MultipartPart::file(
        "file",
        "cat.png",
        "image/png",
        Bytes::from(bytes),
    )];
    if let Some(description) = description {
        parts.push(common::MultipartPart::text("description", description));
    }
    let request = common::TestRequest::post("/api/ap/upload_media");
    let request = match token {
        Some(token) => request.bearer(token),
        None => request,
    };
    request.multipart(C2S_BOUNDARY, &parts).send(app).await
}

/// The C2S inbox's gates: without a token the read answers Pleroma's
/// `EnsureAuthenticatedPlug` 401, a stranger's token and a scope-less one
/// the 403s — the wrong user the ticket's `{"error":"Forbidden"}`, the
/// missing scope Pleroma's `OAuthScopesPlug` wording — while `read` and
/// the `read:statuses` sub-scope both pass.
#[tokio::test]
async fn the_c2s_inbox_only_answers_its_owner() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, alice_read) = c2s_account(&pool, "c2s-inbox-alice", "read").await;
    let (_bob, bob_token) = c2s_account(&pool, "c2s-inbox-bob", "read").await;
    let (_, alice_write) = c2s_account(&pool, "c2s-inbox-carol", "write").await;

    let anonymous = read_inbox(&app, "c2s-inbox-alice", None, "").await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "inbox, no token");
    common::assert_error(&anonymous, "Invalid credentials", "inbox, no token");

    let stranger = read_inbox(&app, "c2s-inbox-alice", Some(&bob_token), "").await;
    common::assert_answer(&stranger, StatusCode::FORBIDDEN, "inbox of another");
    common::assert_error(&stranger, "Forbidden", "inbox of another");

    let unscoped = read_inbox(&app, "c2s-inbox-alice", Some(&alice_write), "").await;
    common::assert_answer(&unscoped, StatusCode::FORBIDDEN, "inbox, no read scope");
    common::assert_error(
        &unscoped,
        "Insufficient permissions: read | read:statuses.",
        "inbox, no read scope",
    );

    let owner = read_inbox(&app, "c2s-inbox-alice", Some(&alice_read), "").await;
    assert_eq!(
        owner.status,
        StatusCode::OK,
        "the owner reads: {}",
        owner.text()
    );
    assert_eq!(
        owner.header("content-type"),
        Some("application/activity+json; charset=utf-8"),
        "the inbox is an ActivityPub document"
    );

    let (dave, dave_sub) = c2s_account(&pool, "c2s-inbox-dave", "read:statuses").await;
    let sub = read_inbox(&app, &dave.nickname, Some(&dave_sub), "").await;
    assert_eq!(
        sub.status,
        StatusCode::OK,
        "read:statuses suffices as well: {}",
        sub.text()
    );
}

/// `GET /users/{nickname}/inbox` without `?page=true` answers the
/// collection root: the inbox's own address, the `first` page link, and
/// no items or totals of its own (Pleroma's `activity_collection.json`).
#[tokio::test]
async fn the_c2s_inbox_root_is_an_ordered_collection() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = c2s_account(&pool, "c2s-inbox-root", "read").await;

    let response = read_inbox(&app, "c2s-inbox-root", Some(&token), "").await;
    assert_eq!(response.status, StatusCode::OK, "the owner reads");
    let root = response.json_value();
    let keys: Vec<&str> = {
        let mut keys: Vec<&str> = root
            .as_object()
            .expect("a collection is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    };
    assert_eq!(
        keys,
        ["@context", "first", "id", "type"],
        "the root carries the address, the type and the first link"
    );
    assert_eq!(root["id"], format!("{}/inbox", alice.ap_id), "its own id");
    assert_eq!(root["type"], "OrderedCollection", "the collection type");
    assert_eq!(
        root["first"],
        format!("{}/inbox?page=true", alice.ap_id),
        "the first page hangs off the same address"
    );
}

/// Seeds `nickname`'s inbox world: twenty-three public mentions by the
/// pager, then a direct mention and one final public mention (the newest
/// facts), and the two posts that must stay out of the listing — a public
/// post mentioning nobody and one by the owner herself. Answers the
/// mentioned status ids, newest first.
async fn seed_inbox_mentions(pool: &PgPool, owner: &User, pager: &User) -> Vec<Uuid> {
    let mut expected: Vec<Uuid> = Vec::new();
    for i in 1..=23 {
        let row = mention_status(
            pool,
            pager,
            owner,
            &format!("mention {i}"),
            Visibility::Public,
        )
        .await;
        expected.push(row.id);
    }
    let dm = mention_status(pool, pager, owner, "a quiet word", Visibility::Direct).await;
    let late = mention_status(pool, pager, owner, "mention 24", Visibility::Public).await;
    expected.push(dm.id);
    expected.push(late.id);
    let _unmentioned =
        common::create_status(pool, pager, "to nobody in particular", Visibility::Public).await;
    let _own = common::create_status(pool, owner, "my own post", Visibility::Public).await;
    expected.sort_unstable();
    expected.reverse();
    expected
}

/// The paged C2S inbox lists the statuses that mention the owner as their
/// `Create` activities, newest first, twenty to a page: the direct
/// messages and mentions in, the unmentioned posts and the owner's own
/// posts out, and the `next`/`prev` cursors of the keyset walk (decision 4
/// keys it on the status id).
#[tokio::test]
async fn the_c2s_inbox_pages_mentions_as_create_activities() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = c2s_account(&pool, "c2s-inbox-page", "read").await;
    let bob = common::create_local_user(&pool, "c2s-inbox-pager").await;
    let expected = seed_inbox_mentions(&pool, &alice, &bob).await;

    let first = read_inbox(&app, "c2s-inbox-page", Some(&token), "page=true").await;
    assert_eq!(first.status, StatusCode::OK, "the first page");
    let page = first.json_value();
    assert_eq!(page["type"], "OrderedCollectionPage", "the page type");
    assert_eq!(
        page["partOf"],
        format!("{}/inbox", alice.ap_id),
        "the page names its root"
    );
    assert_eq!(
        page["id"],
        format!("{}/inbox?page=true", alice.ap_id),
        "and echoes the address it was requested at"
    );
    let items = page["orderedItems"].as_array().expect("ordered items");
    assert_eq!(items.len(), 20, "the page size is the ticket's twenty");
    for item in items {
        assert_eq!(item["type"], "Create", "every entry is a Create");
        assert_eq!(item["actor"], bob.ap_id.as_str(), "by its author");
    }
    let page_ids: Vec<Uuid> = items.iter().map(status_of_item).collect();
    assert_eq!(
        page_ids,
        expected[..20],
        "newest first, and the unmentioned posts are absent"
    );
    assert!(
        items
            .iter()
            .any(|item| item["to"] == json!([alice.ap_id.as_str()])),
        "the direct message reaches the inbox as a Create addressed to the owner alone"
    );
    let oldest = *page_ids.last().expect("a full page has a last entry");
    assert_eq!(
        page["next"],
        json!(format!("{}/inbox?page=true&max_id={oldest}", alice.ap_id)),
        "the next page walks to the oldest id"
    );
    assert_eq!(
        page["prev"],
        json!(format!(
            "{}/inbox?page=true&min_id={}",
            alice.ap_id, expected[0]
        )),
        "and a page back toward the newest end is keyed on its newest"
    );

    let second = read_inbox(
        &app,
        "c2s-inbox-page",
        Some(&token),
        &format!("page=true&max_id={oldest}"),
    )
    .await;
    assert_eq!(
        second.status,
        StatusCode::OK,
        "the second page: {}",
        second.text()
    );
    let page2 = second.json_value();
    let items2 = page2["orderedItems"].as_array().expect("ordered items");
    assert_eq!(items2.len(), 5, "the tail of the walk holds five");
    assert!(page2.get("next").is_none(), "a short page ends the walk");
    let ids2: Vec<Uuid> = items2.iter().map(status_of_item).collect();
    assert_eq!(ids2, expected[20..], "and continues newest-first");
    assert_eq!(ids2[4], expected[24], "the walk ends at the oldest mention");
}

/// The status id a listed `Create` names, read off its object's
/// `{BASE}/objects/{uuid}` id.
fn status_of_item(item: &Value) -> Uuid {
    let uri = item["object"]["id"].as_str().expect("the object's id");
    Uuid::parse_str(uri.rsplit('/').next().expect("a final path segment"))
        .expect("the tail is the status id")
}

/// A malformed `max_id` cursor is the shared 400 every keyset walk gives.
#[tokio::test]
async fn the_c2s_inbox_refuses_a_malformed_cursor() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = c2s_account(&pool, "c2s-inbox-cursor", "read").await;

    let response = read_inbox(
        &app,
        "c2s-inbox-cursor",
        Some(&token),
        "page=true&max_id=nope",
    )
    .await;
    common::assert_answer(&response, StatusCode::BAD_REQUEST, "a bogus cursor");
    common::assert_error(&response, "Invalid id", "a bogus cursor");
}

/// The C2S outbox gates: no token 401, another account's token the 403
/// `{"error":"Forbidden"}`, and a token without `write:statuses` the scope
/// refusal — before a single byte of the activity is looked at.
#[tokio::test]
async fn the_c2s_outbox_only_answers_its_owner() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, read_only) = c2s_account(&pool, "c2s-out-gate-a", "read").await;
    let (_bob, bob_token) = c2s_account(&pool, "c2s-out-gate-b", "read write").await;

    let anonymous = post_outbox(
        &app,
        "c2s-out-gate-a",
        None,
        create_note(&json!([AS_PUBLIC]), &json!([]), "hi"),
    )
    .await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "outbox, no token");

    let stranger = post_outbox(
        &app,
        "c2s-out-gate-a",
        Some(&bob_token),
        create_note(&json!([AS_PUBLIC]), &json!([]), "hi"),
    )
    .await;
    common::assert_answer(&stranger, StatusCode::FORBIDDEN, "outbox of another");
    common::assert_error(&stranger, "Forbidden", "outbox of another");

    let unscoped = post_outbox(
        &app,
        "c2s-out-gate-a",
        Some(&read_only),
        create_note(&json!([AS_PUBLIC]), &json!([]), "hi"),
    )
    .await;
    common::assert_answer(&unscoped, StatusCode::FORBIDDEN, "outbox, no write scope");
    common::assert_error(
        &unscoped,
        "Insufficient permissions: write:statuses.",
        "outbox, no write scope",
    );
}

/// `Create` of a `Note` through the outbox posts end to end: 201 with the
/// `Create` activity's id in `Location` and an empty body, and the status
/// readable at `/api/v1/statuses/{id}` with the note's text, warning and
/// fold, and the upload the note attached.
#[tokio::test]
async fn the_c2s_outbox_create_posts_end_to_end() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let uploads = TempDir::new().expect("temp dir for uploads");
    let app = common::test_app_with_uploads_dir(pool.clone(), uploads.path());
    let (_alice, token) = c2s_account(&pool, "c2s-out-create", "read write").await;

    let upload = upload_to_ap(&app, Some(&token), png_fixture(4), Some("a gradient")).await;
    assert_eq!(upload.status, StatusCode::OK, "the upload lands");
    let media_id = upload.json_value()["id"]
        .as_str()
        .expect("the media id")
        .to_owned();

    let activity = json!({
        "type": "Create",
        "to": [AS_PUBLIC],
        "object": {
            "type": "Note",
            "content": "hello from c2s",
            "summary": "a warning",
            "sensitive": true,
            "attachment": [{"type": "Document", "id": media_id}],
        },
    });
    let created = post_outbox(&app, "c2s-out-create", Some(&token), activity).await;
    assert_eq!(created.status, StatusCode::CREATED, "the create fires");
    assert!(created.body.is_empty(), "and answers no body");
    let location = created.header("location").expect("a Location").to_owned();

    let shown = show_status(&app, &token, location_status_id(&created)).await;
    assert_eq!(
        shown.status,
        StatusCode::OK,
        "the post is visible to its author"
    );
    let entity = shown.json_value();
    assert_eq!(
        entity["uri"],
        format!("{BASE}/objects/{}", entity["id"].as_str().expect("the id")),
        "the stored object id under the test instance"
    );
    assert_eq!(
        entity["content"], "<p>hello from c2s</p>",
        "the text rode through"
    );
    assert_eq!(
        entity["spoiler_text"], "a warning",
        "and the summary became the warning"
    );
    assert_eq!(entity["sensitive"], true, "and the fold its flag");
    assert_eq!(
        entity["visibility"], "public",
        "to the public, as `to` said"
    );
    assert_eq!(
        entity["media_attachments"][0]["id"], media_id,
        "the attachment id claimed the upload"
    );
    assert_eq!(
        location,
        format!(
            "{BASE}/activities/{}",
            entity["id"].as_str().expect("the id")
        ),
        "the Location is the Create activity the post federates as"
    );
}

/// The create's visibility follows the note's and activity's `to`/`cc`
/// exactly as the federation parser reads them: `Public` in `to` is
/// public, in `cc` alone unlisted, the followers collection private,
/// anything else direct.
#[tokio::test]
async fn the_c2s_outbox_create_reads_visibility_from_the_audience() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user_with(&pool, "c2s-out-visibility", |new| NewUser {
        followers_url: Some(followers(&new.ap_id)),
        ..new
    })
    .await;
    let token = common::create_app_and_token(&pool, &alice, "read write").await;
    let followers = followers(&alice.ap_id);

    let cases = [
        (json!([AS_PUBLIC]), json!([]), "public"),
        (json!([]), json!([AS_PUBLIC]), "unlisted"),
        (json!([followers.clone()]), json!([]), "private"),
        (json!([]), json!([]), "direct"),
    ];
    for (index, (to, cc, want)) in cases.iter().enumerate() {
        let activity = create_note(to, cc, &format!("audience case {index}"));
        let created = post_outbox(&app, "c2s-out-visibility", Some(&token), activity).await;
        assert_eq!(created.status, StatusCode::CREATED, "case {index} posts");
        let shown = show_status(&app, &token, location_status_id(&created)).await;
        assert_eq!(
            shown.json_value()["visibility"],
            json!(*want),
            "case {index} stores the audience its addressing spells"
        );
    }
}

/// The create's reply and object arms: an `inReplyTo` naming a stored
/// status lands the answer on it, one naming nothing is the 404, and
/// every shape the outbox does not fold — a non-`Note` object, a bare
/// reference, an unknown type, an empty body — is the 400
/// `{"error":"Unsupported activity type"}`.
#[tokio::test]
async fn the_c2s_outbox_create_replies_and_refuses_other_shapes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = c2s_account(&pool, "c2s-out-shape", "read write").await;
    let parent = common::create_status(&pool, &alice, "a parent", Visibility::Public).await;

    let reply = json!({
        "type": "Create",
        "to": [AS_PUBLIC],
        "object": {
            "type": "Note",
            "content": "an answer",
            "inReplyTo": parent.uri,
        },
    });
    let created = post_outbox(&app, "c2s-out-shape", Some(&token), reply).await;
    assert_eq!(created.status, StatusCode::CREATED, "the answer posts");
    let shown = show_status(&app, &token, location_status_id(&created)).await;
    assert_eq!(
        shown.json_value()["in_reply_to_id"],
        parent.id.to_string(),
        "and lands on the parent it named"
    );

    let stranger = json!({
        "type": "Create",
        "to": [AS_PUBLIC],
        "object": { "type": "Note", "content": "x", "inReplyTo": "https://nowhere.example/objects/1" },
    });
    let refused = post_outbox(&app, "c2s-out-shape", Some(&token), stranger).await;
    common::assert_answer(
        &refused,
        StatusCode::NOT_FOUND,
        "a parent that is no stored status",
    );

    for unsupported in [
        json!({"type": "Create", "object": {"type": "Question", "content": "poll?"}}),
        json!({"type": "Create", "object": format!("{BASE}/objects/whatever")}),
        json!({"type": "Flag", "object": "x"}),
        json!({"type": "Listen"}),
        json!({}),
    ] {
        let denied = post_outbox(&app, "c2s-out-shape", Some(&token), unsupported.clone()).await;
        common::assert_answer(
            &denied,
            StatusCode::BAD_REQUEST,
            &format!("unsupported shape: {unsupported}"),
        );
        common::assert_error(
            &denied,
            "Unsupported activity type",
            "an unfoldable activity",
        );
    }
}

/// `Delete` of the caller's own status by object id: the post goes out of
/// `GET /api/v1/statuses/{id}` with a 201 naming the `Delete`; somebody
/// else's post is the 403 and an unknown reference the 404.
#[tokio::test]
async fn the_c2s_outbox_delete_takes_only_its_own_posts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, alice_token) = c2s_account(&pool, "c2s-out-delete-a", "read write").await;
    let (bob, bob_token) = c2s_account(&pool, "c2s-out-delete-b", "read write").await;

    let activity = create_note(&json!([AS_PUBLIC]), &json!([]), "doomed");
    let created = post_outbox(&app, "c2s-out-delete-a", Some(&alice_token), activity).await;
    assert_eq!(created.status, StatusCode::CREATED, "the post fires");
    let id = location_status_id(&created);
    let uri = format!("{BASE}/objects/{id}");

    let deleted = post_outbox(
        &app,
        "c2s-out-delete-a",
        Some(&alice_token),
        json!({"type": "Delete", "object": uri}),
    )
    .await;
    assert_eq!(
        deleted.status,
        StatusCode::CREATED,
        "the delete is accepted"
    );
    assert!(deleted.body.is_empty(), "and answers no body");
    assert!(
        deleted
            .header("location")
            .expect("a Location")
            .starts_with(&format!("{BASE}/activities/")),
        "the Location names the Delete activity"
    );
    let shown = show_status(&app, &alice_token, id).await;
    common::assert_answer(&shown, StatusCode::NOT_FOUND, "the post after its delete");

    let foreign = common::create_status(&pool, &bob, "bobs own", Visibility::Public).await;
    let refused = post_outbox(
        &app,
        "c2s-out-delete-a",
        Some(&alice_token),
        json!({"type": "Delete", "object": foreign.uri}),
    )
    .await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "a delete of another's post",
    );
    common::assert_error(&refused, "Forbidden", "a delete of another's post");

    let unknown = post_outbox(
        &app,
        "c2s-out-delete-a",
        Some(&alice_token),
        json!({"type": "Delete", "object": format!("{BASE}/objects/{}", Uuid::now_v7())}),
    )
    .await;
    common::assert_answer(&unknown, StatusCode::NOT_FOUND, "a delete of nothing");

    // Bob may of course delete his own — by id text as well as by uri.
    let by_id = post_outbox(
        &app,
        "c2s-out-delete-b",
        Some(&bob_token),
        json!({"type": "Delete", "object": foreign.id.to_string()}),
    )
    .await;
    assert_eq!(
        by_id.status,
        StatusCode::CREATED,
        "the id text names it too"
    );
}

/// `Follow` of an actor id through the outbox: the relationship writes
/// and reads back through the client API, a repeat restates the same
/// fact and the same `Location`, and the `Undo` by the activity id the
/// `Follow` answered takes the follow back. A second `Undo` of the same
/// id names nothing any more and is the 404.
#[tokio::test]
async fn the_c2s_outbox_follows_and_undoes_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = c2s_account(&pool, "c2s-out-follow-a", "read write").await.1;
    let (bob, _) = c2s_account(&pool, "c2s-out-follow-b", "read").await;

    let follow = post_outbox(
        &app,
        "c2s-out-follow-a",
        Some(&token),
        json!({"type": "Follow", "object": bob.ap_id}),
    )
    .await;
    assert_eq!(
        follow.status,
        StatusCode::CREATED,
        "the follow lands: {}",
        follow.text()
    );
    let follow_id = follow.header("location").expect("a Location").to_owned();
    let rel = relationship(&app, &token, &bob).await;
    assert_eq!(
        rel["following"],
        json!(true),
        "and the relationship says so"
    );

    let repeat = post_outbox(
        &app,
        "c2s-out-follow-a",
        Some(&token),
        json!({"type": "Follow", "object": bob.ap_id}),
    )
    .await;
    assert_eq!(repeat.status, StatusCode::CREATED, "a repeat is restated");
    assert_eq!(
        repeat.header("location"),
        Some(follow_id.as_str()),
        "naming the one Follow the pair holds"
    );

    let undone = post_outbox(
        &app,
        "c2s-out-follow-a",
        Some(&token),
        json!({"type": "Undo", "object": follow_id}),
    )
    .await;
    assert_eq!(undone.status, StatusCode::CREATED, "the undo is accepted");
    assert_ne!(
        undone.header("location"),
        Some(follow_id.as_str()),
        "naming the fresh Undo activity instead"
    );
    let gone = relationship(&app, &token, &bob).await;
    assert_eq!(gone["following"], json!(false), "and the follow is gone");

    let again = post_outbox(
        &app,
        "c2s-out-follow-a",
        Some(&token),
        json!({"type": "Undo", "object": follow_id}),
    )
    .await;
    common::assert_answer(&again, StatusCode::NOT_FOUND, "a second undo of one follow");
}

/// The other `Undo` arms: an embedded `Follow` withdraws by the target
/// actor the inner activity names, a follow of an actor this instance has
/// never stored is the 404 (the C2S door reads stored rows, it fetches
/// nobody), and an embedded activity of no withdrawable type is the 400.
#[tokio::test]
async fn the_c2s_outbox_undoes_an_embedded_follow_and_refuses_the_rest() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = c2s_account(&pool, "c2s-out-follow-c", "read write").await.1;
    let (bob, _) = c2s_account(&pool, "c2s-out-follow-d", "read").await;

    let followed = post_outbox(
        &app,
        "c2s-out-follow-c",
        Some(&token),
        json!({"type": "Follow", "object": bob.ap_id}),
    )
    .await;
    assert_eq!(followed.status, StatusCode::CREATED, "followed");
    let embedded_undo = post_outbox(
        &app,
        "c2s-out-follow-c",
        Some(&token),
        json!({"type": "Undo", "object": {"type": "Follow", "object": bob.ap_id}}),
    )
    .await;
    assert_eq!(
        embedded_undo.status,
        StatusCode::CREATED,
        "the embedded undo lands"
    );
    let withdrawn = relationship(&app, &token, &bob).await;
    assert_eq!(
        withdrawn["following"],
        json!(false),
        "and withdrew the follow"
    );

    let unknown = post_outbox(
        &app,
        "c2s-out-follow-c",
        Some(&token),
        json!({"type": "Follow", "object": "https://nowhere.example/users/eve"}),
    )
    .await;
    common::assert_answer(
        &unknown,
        StatusCode::NOT_FOUND,
        "a follow of an unseen actor",
    );

    let dormant = common::create_deactivated_user_with_password(
        &pool,
        "c2s-out-follow-sleeps",
        "correct horse battery staple",
    )
    .await;
    let gone = post_outbox(
        &app,
        "c2s-out-follow-c",
        Some(&token),
        json!({"type": "Follow", "object": dormant.ap_id}),
    )
    .await;
    common::assert_answer(
        &gone,
        StatusCode::NOT_FOUND,
        "a follow of a deactivated account",
    );

    let odd = post_outbox(
        &app,
        "c2s-out-follow-c",
        Some(&token),
        json!({"type": "Undo", "object": {"type": "Flag", "object": bob.ap_id}}),
    )
    .await;
    common::assert_answer(&odd, StatusCode::BAD_REQUEST, "an undo of an odd type");
    common::assert_error(&odd, "Unsupported activity type", "an undo of an odd type");
}

/// `Like` of a status through the outbox: the favourite writes, the
/// entity reads it back, a repeat answers the same activity id, and the
/// `Undo` by that id withdraws it.
#[tokio::test]
async fn the_c2s_outbox_likes_and_undoes_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = c2s_account(&pool, "c2s-out-like-a", "read write").await;
    let (bob, _) = c2s_account(&pool, "c2s-out-like-b", "read").await;
    let post = common::create_status(&pool, &bob, "likeable", Visibility::Public).await;

    let liked = post_outbox(
        &app,
        "c2s-out-like-a",
        Some(&token),
        json!({"type": "Like", "object": post.uri}),
    )
    .await;
    assert_eq!(
        liked.status,
        StatusCode::CREATED,
        "the like lands: {}",
        liked.text()
    );
    let like_id = liked.header("location").expect("a Location").to_owned();
    let shown = show_status(&app, &token, post.id).await;
    let entity = shown.json_value();
    assert_eq!(entity["favourites_count"], 1, "counted");
    assert_eq!(entity["favourited"], true, "and flagged for its viewer");

    let repeat = post_outbox(
        &app,
        "c2s-out-like-a",
        Some(&token),
        json!({"type": "Like", "object": post.uri}),
    )
    .await;
    assert_eq!(repeat.status, StatusCode::CREATED, "a repeat is restated");
    assert_eq!(
        repeat.header("location"),
        Some(like_id.as_str()),
        "naming the one Like the pair holds"
    );

    let undone = post_outbox(
        &app,
        "c2s-out-like-a",
        Some(&token),
        json!({"type": "Undo", "object": like_id}),
    )
    .await;
    assert_eq!(undone.status, StatusCode::CREATED, "the undo is accepted");
    let after_undo = show_status(&app, &token, post.id).await.json_value();
    assert_eq!(after_undo["favourites_count"], 0, "the like came off");
}

/// The like's other arms: an embedded `Undo` withdraws by the status the
/// inner activity names, and what the caller may not see is the 404.
#[tokio::test]
async fn the_c2s_outbox_undoes_an_embedded_like_and_refuses_the_invisible() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = c2s_account(&pool, "c2s-out-like-c", "read write").await;
    let (bob, bob_token) = c2s_account(&pool, "c2s-out-like-d", "read").await;
    let post = common::create_status(&pool, &bob, "likeable too", Visibility::Public).await;
    let hidden = common::create_status(&pool, &bob, "hidden away", Visibility::Private).await;

    let liked = post_outbox(
        &app,
        "c2s-out-like-c",
        Some(&token),
        json!({"type": "Like", "object": post.uri}),
    )
    .await;
    assert_eq!(liked.status, StatusCode::CREATED, "the like lands");
    let embedded_undo = post_outbox(
        &app,
        "c2s-out-like-c",
        Some(&token),
        json!({"type": "Undo", "object": {"type": "Like", "object": post.uri}}),
    )
    .await;
    assert_eq!(
        embedded_undo.status,
        StatusCode::CREATED,
        "the embedded undo lands"
    );
    let as_author = show_status(&app, &bob_token, post.id).await.json_value();
    assert_eq!(as_author["favourites_count"], 0, "and withdrew the like");

    let invisible = post_outbox(
        &app,
        "c2s-out-like-c",
        Some(&token),
        json!({"type": "Like", "object": hidden.uri}),
    )
    .await;
    common::assert_answer(
        &invisible,
        StatusCode::NOT_FOUND,
        "a like of an unseen post",
    );
}

/// `Announce` of a status through the outbox: the reblog row *is* the
/// `Announce` (decision 6), so the `Location` names it and the boost
/// renders over the original, and the `Undo` of that id takes the boost
/// back.
#[tokio::test]
async fn the_c2s_outbox_announces_and_undoes_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = c2s_account(&pool, "c2s-out-boost-a", "read write").await;
    let (bob, _) = c2s_account(&pool, "c2s-out-boost-b", "read").await;
    let post = common::create_status(&pool, &bob, "boost me", Visibility::Public).await;

    let announced = post_outbox(
        &app,
        "c2s-out-boost-a",
        Some(&token),
        json!({"type": "Announce", "object": post.uri}),
    )
    .await;
    assert_eq!(
        announced.status,
        StatusCode::CREATED,
        "the boost lands: {}",
        announced.text()
    );
    let announce_id = announced.header("location").expect("a Location").to_owned();
    assert_eq!(
        announce_id,
        format!("{BASE}/activities/{}", location_status_id(&announced)),
        "the reblog's own activity address"
    );
    let reblog = show_status(&app, &token, location_status_id(&announced)).await;
    let entity = reblog.json_value();
    assert_eq!(
        entity["uri"], announce_id,
        "the row's uri is the Announce id"
    );
    assert_eq!(
        entity["reblog"]["uri"], post.uri,
        "and it carries the original it boosts"
    );
    let original = show_status(&app, &token, post.id).await.json_value();
    assert_eq!(original["reblogs_count"], 1, "the original counts it");
    assert_eq!(original["reblogged"], true, "and flags it for the booster");

    let undone = post_outbox(
        &app,
        "c2s-out-boost-a",
        Some(&token),
        json!({"type": "Undo", "object": announce_id}),
    )
    .await;
    assert_eq!(undone.status, StatusCode::CREATED, "the undo is accepted");
    let after_undo = show_status(&app, &token, post.id).await.json_value();
    assert_eq!(after_undo["reblogs_count"], 0, "the boost came off");
    assert_eq!(
        after_undo["reblogged"], false,
        "and no longer flags it for the booster"
    );
}

/// `Block` of an actor through the outbox and its `Undo` by the activity
/// id the block answered: the relationship flips through the client API's
/// own reads, and an unseen actor is the 404.
#[tokio::test]
async fn the_c2s_outbox_blocks_and_undoes_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = c2s_account(&pool, "c2s-out-block-a", "read write").await;
    let (bob, _) = c2s_account(&pool, "c2s-out-block-b", "read").await;

    let blocked = post_outbox(
        &app,
        "c2s-out-block-a",
        Some(&token),
        json!({"type": "Block", "object": bob.ap_id}),
    )
    .await;
    assert_eq!(blocked.status, StatusCode::CREATED, "the block lands");
    let block_id = blocked.header("location").expect("a Location").to_owned();
    let rel = relationship(&app, &token, &bob).await;
    assert_eq!(
        rel["blocking"],
        json!(true),
        "the relationship says blocked"
    );

    let undone = post_outbox(
        &app,
        "c2s-out-block-a",
        Some(&token),
        json!({"type": "Undo", "object": block_id}),
    )
    .await;
    assert_eq!(undone.status, StatusCode::CREATED, "the undo is accepted");
    let lifted = relationship(&app, &token, &bob).await;
    assert_eq!(lifted["blocking"], json!(false), "and the block is lifted");

    let unknown = post_outbox(
        &app,
        "c2s-out-block-a",
        Some(&token),
        json!({"type": "Block", "object": "https://nowhere.example/users/eve"}),
    )
    .await;
    common::assert_answer(
        &unknown,
        StatusCode::NOT_FOUND,
        "a block of an unseen actor",
    );
}

/// The embedded block withdrawal: the inner `Block`'s object names the
/// target actor, and the undo lifts the pair's block.
#[tokio::test]
async fn the_c2s_outbox_undoes_an_embedded_block() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = c2s_account(&pool, "c2s-out-block-c", "read write").await;
    let (bob, _) = c2s_account(&pool, "c2s-out-block-d", "read").await;

    let reblocked = post_outbox(
        &app,
        "c2s-out-block-c",
        Some(&token),
        json!({"type": "Block", "object": bob.ap_id}),
    )
    .await;
    assert_eq!(reblocked.status, StatusCode::CREATED, "blocked");
    let undone = post_outbox(
        &app,
        "c2s-out-block-c",
        Some(&token),
        json!({"type": "Undo", "object": {"type": "Block", "object": bob.ap_id}}),
    )
    .await;
    assert_eq!(
        undone.status,
        StatusCode::CREATED,
        "the embedded undo lands"
    );
    let lifted_again = relationship(&app, &token, &bob).await;
    assert_eq!(
        lifted_again["blocking"],
        json!(false),
        "and the embedded undo lifted the block"
    );
}

/// The `Announce` refusals: what the caller may not see is the shared
/// 404, and the caller's own `private` post — visible to them and still
/// not boostable — is the client API's own 422, in its own words.
#[tokio::test]
async fn the_c2s_outbox_refuses_an_unboostable_target() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = c2s_account(&pool, "c2s-out-boost-c", "read write").await;
    let (bob, _) = c2s_account(&pool, "c2s-out-boost-d", "read").await;
    let hidden = common::create_status(&pool, &bob, "keep it quiet", Visibility::Private).await;

    let refused = post_outbox(
        &app,
        "c2s-out-boost-c",
        Some(&token),
        json!({"type": "Announce", "object": hidden.uri}),
    )
    .await;
    common::assert_answer(
        &refused,
        StatusCode::NOT_FOUND,
        "an Announce of an unseen post",
    );

    let mine = common::create_status(&pool, &alice, "my private word", Visibility::Private).await;
    let unboostable = post_outbox(
        &app,
        "c2s-out-boost-c",
        Some(&token),
        json!({"type": "Announce", "object": mine.uri}),
    )
    .await;
    common::assert_answer(
        &unboostable,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an Announce of a private post",
    );
    common::assert_error(
        &unboostable,
        "Could not reblog private status",
        "an Announce of a private post",
    );
}

/// `GET /api/ap/whoami` answers the authenticated user's own actor
/// document under the `ActivityPub` media type, and demands a `read`
/// token.
#[tokio::test]
async fn whoami_answers_the_callers_own_actor_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = c2s_account(&pool, "c2s-whoami", "read").await;
    let (_, write_only) = c2s_account(&pool, "c2s-whoami-writer", "write").await;

    let response = common::TestRequest::get("/api/ap/whoami")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the caller reads itself");
    assert_eq!(
        response.header("content-type"),
        Some("application/activity+json; charset=utf-8"),
        "as an ActivityPub document"
    );
    let actor = response.json_value();
    assert_eq!(actor["id"], alice.ap_id, "the actor id of the token's user");
    assert_eq!(actor["type"], "Person", "a person");
    assert_eq!(actor["preferredUsername"], "c2s-whoami", "by its nickname");

    let anonymous = common::TestRequest::get("/api/ap/whoami").send(&app).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "whoami, no token");

    let unscoped = common::TestRequest::get("/api/ap/whoami")
        .bearer(&write_only)
        .send(&app)
        .await;
    common::assert_answer(&unscoped, StatusCode::FORBIDDEN, "whoami, no read scope");
    common::assert_error(&unscoped, "Insufficient permissions: read.", "whoami scope");
}

/// `POST /api/ap/upload_media` runs the `POST /api/v1/media` processing
/// and answers the stored row as an `ActivityStreams` `Document`: the
/// media id for a create to quote, the `Link` url, the alt text, the
/// file's type and the computed `Blurhash`.
#[tokio::test]
async fn the_c2s_upload_answers_a_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let uploads = TempDir::new().expect("temp dir for uploads");
    let app = common::test_app_with_uploads_dir(pool.clone(), uploads.path());
    let (_, token) = c2s_account(&pool, "c2s-upload", "read write").await;

    let response = upload_to_ap(&app, Some(&token), png_fixture(9), Some("a gradient")).await;
    assert_eq!(response.status, StatusCode::OK, "the upload answers 200");
    let document = response.json_value();
    let keys: Vec<&str> = {
        let mut keys: Vec<&str> = document
            .as_object()
            .expect("a document is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    };
    assert_eq!(
        keys,
        ["blurhash", "id", "mediaType", "name", "type", "url"],
        "the documented key set"
    );
    assert_eq!(document["type"], "Document", "the object type");
    assert_eq!(document["name"], "a gradient", "the submitted alt text");
    assert_eq!(document["mediaType"], "image/png", "read from the bytes");
    let media_id = Uuid::parse_str(document["id"].as_str().expect("an id text"))
        .expect("the id is the media id itself");
    let url = &document["url"][0];
    assert_eq!(url["type"], "Link", "a Link entry");
    assert_eq!(url["mediaType"], "image/png", "carrying the file type");
    let href = url["href"].as_str().expect("an href");
    assert!(
        href.starts_with(&format!("{BASE}/media/")),
        "the file is served under the media route: {href}"
    );

    // The row exists as the caller's own unattached upload, readable
    // through the Mastodon media route — one row, two doors.
    let shown = common::TestRequest::get(&format!("/api/v1/media/{media_id}"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(shown.status, StatusCode::OK, "the upload is readable");
    let entity = shown.json_value();
    assert_eq!(entity["id"], media_id.to_string(), "under one id");
    assert_eq!(entity["url"], href, "served at one address");

    // A bare upload carries no name text and still reports its hash.
    let bare = upload_to_ap(&app, Some(&token), png_fixture(3), None).await;
    assert_eq!(
        bare.status,
        StatusCode::OK,
        "the file-less-description upload lands"
    );
    assert_eq!(bare.json_value()["name"], "", "an absent alt text is empty");
}

/// The upload's gates and refusals: no token 401, a token without
/// `write:media` 403, and no `file` part the shared 422 — the same
/// answers `POST /api/v1/media` gives, on the C2S address.
#[tokio::test]
async fn the_c2s_upload_demands_media_write_and_a_file() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let uploads = TempDir::new().expect("temp dir for uploads");
    let app = common::test_app_with_uploads_dir(pool.clone(), uploads.path());
    let (_, read_only) = c2s_account(&pool, "c2s-upload-reader", "read").await;
    let (_, media_token) = c2s_account(&pool, "c2s-upload-writer", "write:media").await;

    let anonymous = upload_to_ap(&app, None, png_fixture(1), None).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "upload, no token");

    let unscoped = upload_to_ap(&app, Some(&read_only), png_fixture(2), None).await;
    common::assert_answer(&unscoped, StatusCode::FORBIDDEN, "upload, no media scope");
    common::assert_error(
        &unscoped,
        "Insufficient permissions: write:media.",
        "upload scope",
    );

    let no_file = common::TestRequest::post("/api/ap/upload_media")
        .bearer(&media_token)
        .multipart(
            C2S_BOUNDARY,
            &[common::MultipartPart::text("description", "none")],
        )
        .send(&app)
        .await;
    common::assert_answer(
        &no_file,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an upload with no file part",
    );
    common::assert_error(
        &no_file,
        "Missing parameter: file",
        "an upload with no file part",
    );
}

/// The upload the create claims: the `Document`'s `id` is exactly what a
/// posted `Create` names in its `attachment` — and the whole round trip
/// from upload to attached post runs through the C2S door.
#[tokio::test]
async fn the_c2s_upload_feeds_the_c2s_create() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let uploads = TempDir::new().expect("temp dir for uploads");
    let app = common::test_app_with_uploads_dir(pool.clone(), uploads.path());
    let (_alice, token) = c2s_account(&pool, "c2s-upload-claim", "read write").await;

    let upload = upload_to_ap(&app, Some(&token), png_fixture(7), None).await;
    let document = upload.json_value();
    let media_id = document["id"].as_str().expect("the media id").to_owned();

    let activity = json!({
        "type": "Create",
        "to": [AS_PUBLIC],
        "object": {
            "type": "Note",
            "content": "look at this",
            "attachment": [{"type": "Document", "id": format!("{BASE}/api/v1/media/{media_id}")}],
        },
    });
    let created = post_outbox(&app, "c2s-upload-claim", Some(&token), activity).await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "the create claims the upload"
    );
    let entity = show_status(&app, &token, location_status_id(&created))
        .await
        .json_value();
    assert_eq!(
        entity["media_attachments"][0]["id"], media_id,
        "the upload id read whether bare or as the media route's URL"
    );
}

/// Somebody else's upload is not the caller's to attach: the create that
/// names a stranger's media id earns the media model's own 422.
#[tokio::test]
async fn the_c2s_create_cannot_claim_another_users_upload() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let uploads = TempDir::new().expect("temp dir for uploads");
    let app = common::test_app_with_uploads_dir(pool.clone(), uploads.path());
    let (_, carol_token) = c2s_account(&pool, "c2s-upload-stranger", "read write").await;
    let (_, token) = c2s_account(&pool, "c2s-upload-thief", "read write").await;

    let upload = upload_to_ap(&app, Some(&carol_token), png_fixture(5), None).await;
    let media_id = upload.json_value()["id"]
        .as_str()
        .expect("the media id")
        .to_owned();
    let activity = json!({
        "type": "Create",
        "to": [AS_PUBLIC],
        "object": {
            "type": "Note",
            "content": "stolen goods",
            "attachment": [{"id": media_id}],
        },
    });
    let refused = post_outbox(&app, "c2s-upload-thief", Some(&token), activity).await;
    common::assert_answer(
        &refused,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a foreign upload",
    );
    common::assert_error(&refused, "Media not found", "a foreign upload");
}
