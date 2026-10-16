//! Tests for the instance actor: `GET /internal/fetch` and the listings
//! it must stay out of (`src/services/instance_actor.rs`,
//! `src/api/activitypub.rs`).
//!
//! The actor is the local `Application` account named `internal.fetch` at
//! `{BASE_URL}/internal/fetch` that signs this instance's fetches and owns
//! server-level activities. Its document comes from the same builder as a
//! user's, so it carries the same key set; it is provisioned on first use
//! and never listed: neither the instance statistics nor the `NodeInfo`
//! counters count it, while handle lookups may still find it.
//!
//! The row shape (`locked`, undiscoverable, no password) and the
//! create-once logic are covered where they live, in
//! `src/services/instance_actor.rs`; what these tests own is the route,
//! the document as served over HTTP, and the exclusions as the public
//! endpoints see them.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    models::user::INSTANCE_ACTOR_NICKNAME, services::instance_actor::INSTANCE_ACTOR_PATH,
};

/// The instance origin of [`common::test_config`].
const BASE: &str = "https://test.example";
/// The media type of an `ActivityPub` document, charset included.
const ACTIVITY_JSON: &str = "application/activity+json; charset=utf-8";
/// The actor id of the instance actor on the test instance.
const AP_ID: &str = "https://test.example/internal/fetch";
/// Every key of an actor document that carries neither avatar nor banner:
/// the same set `GET /users/{nickname}` serves, because the instance
/// actor is built by the same renderer.
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

/// `GET /internal/fetch` on `app` and the document it answers.
async fn fetch_instance_actor(app: &Router) -> Value {
    let response = common::TestRequest::get(INSTANCE_ACTOR_PATH)
        .header("accept", "application/activity+json")
        .send(app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the instance actor document is served"
    );
    assert_eq!(
        response.header("content-type"),
        Some(ACTIVITY_JSON),
        "as an ActivityPub document"
    );
    response.json_value()
}

/// The document, the create-once behaviour and the row the ticket
/// prescribes, end to end over HTTP: an `Application` typed as such,
/// locked and undiscoverable, named after the instance, with a generated
/// key — and two requests still create exactly one row.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_instance_actor_document_is_served_and_created_once(pool: PgPool) {
    let app = common::test_app(pool.clone());

    let document = fetch_instance_actor(&app).await;
    let second = fetch_instance_actor(&app).await;

    assert_eq!(document["id"], AP_ID, "the internal fetch URI");
    assert_eq!(document["type"], "Application", "the instance actor type");
    assert_eq!(
        document["preferredUsername"], INSTANCE_ACTOR_NICKNAME,
        "the reserved nickname"
    );
    assert_eq!(
        document["name"], "Wired",
        "the display name is the instance name"
    );
    assert_eq!(
        document["manuallyApprovesFollowers"], true,
        "the actor is locked"
    );
    assert_eq!(document["discoverable"], false, "and undiscoverable");
    assert_eq!(document["url"], AP_ID, "the url is the actor id");
    assert_eq!(document["inbox"], format!("{AP_ID}/inbox"), "inbox");
    assert_eq!(
        document["endpoints"],
        json!({
            "sharedInbox": format!("{BASE}/inbox"),
            "oauthAuthorizationEndpoint": format!("{BASE}/oauth/authorize"),
            "oauthTokenEndpoint": format!("{BASE}/oauth/token"),
            "oauthRegistrationEndpoint": format!("{BASE}/api/v1/apps"),
        }),
        "the endpoints are the instance's own, like every local actor's"
    );
    assert_eq!(
        document["publicKey"],
        json!({
            "id": format!("{AP_ID}#main-key"),
            "owner": AP_ID,
            "publicKeyPem": document["publicKey"]["publicKeyPem"],
        }),
        "the publicKey names the instance actor's main key"
    );
    assert!(
        document["publicKey"]["publicKeyPem"]
            .as_str()
            .is_some_and(|pem| pem.contains("-----BEGIN PUBLIC KEY-----")),
        "the key is generated on first use"
    );
    assert_eq!(
        key_set(&document),
        ACTOR_KEYS,
        "the same builder as the user documents"
    );

    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM users WHERE nickname = $1")
        .bind(INSTANCE_ACTOR_NICKNAME)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "two requests, one row");
    assert_eq!(
        second["publicKey"]["publicKeyPem"], document["publicKey"]["publicKeyPem"],
        "the second request re-read the row; the key never changed"
    );

    let (password_hash,): (Option<String>,) =
        sqlx::query_as("SELECT password_hash FROM users WHERE nickname = $1")
            .bind(INSTANCE_ACTOR_NICKNAME)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        password_hash.is_none(),
        "the instance actor has no password and cannot log in"
    );
}

/// The public listings that count or list users never see the instance
/// actor: the statistics and `NodeInfo` counters count the one real local
/// account only, and it is no one's staff. The `acct` resolver still
/// finds it — lookups are allowed to.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_instance_actor_stays_out_of_the_public_listings(pool: PgPool) {
    let _alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    let served = common::TestRequest::get(INSTANCE_ACTOR_PATH)
        .send(&app)
        .await;
    assert_eq!(
        served.status,
        StatusCode::OK,
        "the route needs no ActivityPub Accept to answer"
    );

    let stats = common::TestRequest::get("/api/v1/instance")
        .send(&app)
        .await
        .json_value();
    assert_eq!(
        stats["stats"]["user_count"], 1,
        "the instance statistics count alice, not the instance actor"
    );
    assert_eq!(
        stats["stats"]["domain_count"], 0,
        "and the local actor never lands in the domain count either"
    );

    for path in ["/nodeinfo/2.0", "/nodeinfo/2.1"] {
        let document = common::TestRequest::get(path).send(&app).await.json_value();
        assert_eq!(
            document["usage"]["users"]["total"], 1,
            "{path} counts alice alone"
        );
        assert_eq!(
            document["metadata"]["staffAccounts"],
            json!([]),
            "{path} lists no staff besides the real accounts"
        );
    }

    let config = common::test_config();
    let found = wired_be::models::user::get_by_acct(&pool, &config, INSTANCE_ACTOR_NICKNAME)
        .await
        .unwrap();
    assert!(
        found.is_some(),
        "the acct lookup is allowed to find the instance actor"
    );
}
