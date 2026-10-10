//! Tests for `GET /users/{nickname}`, the `ActivityPub` actor document.
//!
//! An `ActivityPub` client (an `Accept` naming `application/activity+json`
//! or `application/ld+json`) gets Pleroma's `user.json` document as
//! `application/activity+json; charset=utf-8`; anyone else gets 404
//! `{"error":"Not found"}` for now, an unknown or remote account gets the
//! same 404, and a deactivated local account gets 410 Gone. Every answer
//! says it varies on `Accept`.
//!
//! The renderer's corner cases are covered where they live, in
//! `src/federation/actor.rs`; what these tests own is the route: the
//! negotiation, the headers, the status codes and the document of a stored
//! user end to end.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    models::user::{Flag, NewUser, User},
    util::time::mastodon_time,
};

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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_local_user_is_served_as_its_actor_document(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_bare_user_has_the_documented_key_set(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_locked_user_approves_followers_manually(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_bot_is_federated_by_its_actor_type(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn an_avatar_user_carries_icon_and_image(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn ld_json_clients_get_the_actor_too(pool: PgPool) {
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

/// A client that does not ask for `ActivityPub` gets the router-level 404:
/// this backend is API-only and has no HTML profile page to answer with.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_browser_is_not_an_activitypub_client(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    for accept in [
        "*/*",
        "text/html",
        "text/html,application/xhtml+xml",
        "application/json",
        "application/activity+xml",
    ] {
        let response = common::TestRequest::get(&format!("/users/{}", alice.nickname))
            .header("accept", accept)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{accept} is not an ActivityPub client"
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn unknown_and_remote_users_are_not_found(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_deactivated_user_is_gone(pool: PgPool) {
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
        StatusCode::NOT_FOUND,
        "a browser is not an ActivityPub client, gone or not"
    );
}
