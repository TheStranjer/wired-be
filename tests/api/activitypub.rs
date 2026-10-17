//! Tests for the `ActivityPub` routes: `GET /users/{nickname}`, the actor
//! document, and the three inbox `POST`s.
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

use axum::{Router, http::StatusCode};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::Toggle,
    federation::{
        dispatch::KIND_INCOMING,
        signature::{SigningKey, sign_request},
    },
    jobs::QUEUE_FEDERATOR_INCOMING,
    models::{
        job::Job,
        user::{Flag, NewUser, User},
    },
    services::instance_actor::ensure_instance_actor,
    util::time::mastodon_time,
};

use crate::common;

/// The media type of an `ActivityPub` document, charset included.
const ACTIVITY_JSON: &str = "application/activity+json; charset=utf-8";
/// The instance origin of [`common::test_config`].
const BASE: &str = "https://test.example";
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
