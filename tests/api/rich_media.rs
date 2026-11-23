//! Rich media cards over HTTP: the `card` key of the [`Status`] entity and
//! the deprecated `GET /api/v1/statuses/{id}/card` endpoint.
//!
//! The fetch job caches the card a status's first link describes in the
//! [`link_card`] tables and points the status row at it; this file drives
//! the rendering half of the feature. A status whose row links an `ok`
//! card renders the cached card JSON — the full `card_json` entity shape
//! with `pleroma.opengraph` — in `GET /api/v1/statuses/{id}`; a status
//! with no linked card, or only a failed fetch, renders `card: null`; and
//! the standalone card endpoint answers the same stored JSON with 200,
//! JSON `null` with 200 for a cardless status, and the shared status
//! gate's 404 for the malformed, unknown, deleted and invisible ids —
//! with the scope gate's 403 for a user token that lacks `read:statuses`,
//! exactly like its neighbouring status reads.
//!
//! The cache rows are seeded through [`link_card`]'s own writes — the two
//! a completed fetch performs — because no HTTP path fetches a page
//! inside these tests.
//!
//! [`Status`]: wired_be::entities::status::Status
//! [`link_card`]: wired_be::models::link_card
//! [`card_json`]: wired_be::services::rich_media::card_json

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{link_card, status::Visibility, user::User};

use crate::common;

/// The 404 body every miss on these routes carries (decision 8).
const NOT_FOUND: &str = "Record not found";
/// The 403 body the scope gate renders for a user token without the scope.
const NO_SCOPE: &str = "Insufficient permissions: read:statuses.";
/// The 401 body `MaybeAuthUser` renders for a token that does not resolve.
const INVALID_TOKEN: &str = "The access token is invalid";
/// The scope both reads ask a presented user token for.
const SCOPES: &str = "read:statuses";
/// The id of a status that is not stored here, for the miss probes.
const UNKNOWN_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// The URL the card fixtures cache under.
const CARD_URL: &str = "https://example.test/post";
/// The URL of a fetch the failed-card fixture caches as dead.
const DEAD_URL: &str = "https://dead.test/";

/// The card JSON a completed fetch stores: [`card_json`]'s output, the
/// raw Open Graph pairs under `pleroma.opengraph` included.
fn card_entity() -> Value {
    json!({
        "url": CARD_URL,
        "title": "Example Domain",
        "description": "A page for examples",
        "type": "link",
        "author_name": "",
        "author_url": "",
        "provider_name": "Example",
        "provider_url": "",
        "html": "",
        "width": 0,
        "height": 0,
        "image": "https://example.test/og.png",
        "embed_url": "",
        "blurhash": null,
        "pleroma": { "opengraph": { "og:title": "Example Domain" } },
    })
}

/// Caches [`card_entity`] for [`CARD_URL`] as an `ok` card and links it to
/// `status_id` — the two writes a completed fetch leaves behind.
async fn link_ok_card(pool: &PgPool, status_id: Uuid) {
    let card = card_entity();
    link_card::store(pool, CARD_URL, "ok", Some(&card))
        .await
        .expect("the fixture card stores");
    link_card::link_status(pool, status_id, CARD_URL)
        .await
        .expect("the fixture status links its card");
}

/// Caches `DEAD_URL` as a failed fetch and links it to `status_id` — the
/// two writes a refused fetch leaves behind.
async fn link_failed_card(pool: &PgPool, status_id: Uuid) {
    link_card::store(pool, DEAD_URL, "failed", None)
        .await
        .expect("the fixture failure stores");
    link_card::link_status(pool, status_id, DEAD_URL)
        .await
        .expect("the fixture status links its failure");
}

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn user_with_token(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// `GET /api/v1/statuses/{id}` as `token` (`None`: anonymous).
async fn show(app: &Router, token: Option<&str>, id: &str) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("/api/v1/statuses/{id}"));
    match token {
        Some(token) => request.bearer(token),
        None => request,
    }
    .send(app)
    .await
}

/// `GET /api/v1/statuses/{id}/card` as `token` (`None`: anonymous).
async fn card_of(app: &Router, token: Option<&str>, id: &str) -> common::TestResponse {
    let request = common::TestRequest::get(&format!("/api/v1/statuses/{id}/card"));
    match token {
        Some(token) => request.bearer(token),
        None => request,
    }
    .send(app)
    .await
}

/// A status whose row links an `ok` card renders the stored card JSON in
/// the entity's `card` key: the full `card_json` shape, Open Graph pairs
/// under `pleroma.opengraph` included, verbatim.
#[tokio::test]
async fn a_linked_card_renders_in_the_status_entity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(
        &pool,
        &alice,
        "look https://example.test/post",
        Visibility::Public,
    )
    .await;
    link_ok_card(&pool, post.id).await;

    let response = show(&app, None, &post.id.to_string()).await;
    common::assert_answer(&response, StatusCode::OK, "show of a carded status");
    assert_eq!(
        response.json_value()["card"],
        card_entity(),
        "the cached card renders key for key, pleroma.opengraph included"
    );
}

/// A status with no linked card renders `card: null` — both the one the
/// cache holds nothing for and the one linked only to a failed fetch.
#[tokio::test]
async fn a_status_without_a_card_renders_null() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let plain = common::create_status(&pool, &alice, "no card here", Visibility::Public).await;
    let dead = common::create_status(&pool, &alice, "a dead link", Visibility::Public).await;
    link_failed_card(&pool, dead.id).await;

    for (status, who) in [(&plain, "nothing cached"), (&dead, "a failed link")] {
        let response = show(&app, None, &status.id.to_string()).await;
        common::assert_answer(&response, StatusCode::OK, who);
        assert_eq!(
            response.json_value()["card"],
            Value::Null,
            "{who}: the card key is present and null"
        );
    }
}

/// The card endpoint answers the stored card object with 200 — the same
/// JSON the entity renders — anonymously and for a scoped token alike.
#[tokio::test]
async fn the_card_endpoint_answers_the_stored_card() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let post = common::create_status(
        &pool,
        &alice,
        "look https://example.test/post",
        Visibility::Public,
    )
    .await;
    link_ok_card(&pool, post.id).await;
    let id = post.id.to_string();

    for (token, who) in [
        (None, "an anonymous caller"),
        (Some(alice_token.as_str()), "the author"),
    ] {
        let response = card_of(&app, token, &id).await;
        common::assert_answer(&response, StatusCode::OK, who);
        assert_eq!(
            response.json_value(),
            card_entity(),
            "{who}: the stored card, verbatim"
        );
    }
}

/// With the media proxy on, the card endpoint and the status entity both
/// answer the card image as a signed proxy link that decodes back to the
/// image the page named, while the stored row keeps the original URL.
#[tokio::test]
async fn the_card_image_is_proxied_when_the_media_proxy_is_enabled() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let config = common::test_media_proxy_config(1024);
    let app = common::test_app_with_config(pool.clone(), config.clone());
    let (alice, _token) = user_with_token(&pool, "alice", SCOPES).await;
    let post = common::create_status(
        &pool,
        &alice,
        "look https://example.test/post",
        Visibility::Public,
    )
    .await;
    let mut card = card_entity();
    card["image"] = json!("https://remote.example/og.png");
    link_card::store(&pool, CARD_URL, "ok", Some(&card))
        .await
        .expect("the fixture card stores");
    link_card::link_status(&pool, post.id, CARD_URL)
        .await
        .expect("the fixture status links its card");
    let id = post.id.to_string();

    let on_card = card_of(&app, None, &id).await;
    common::assert_answer(&on_card, StatusCode::OK, "the card endpoint");
    let on_status = show(&app, None, &id).await;
    common::assert_answer(&on_status, StatusCode::OK, "the status endpoint");
    for (value, who) in [
        (on_card.json_value(), "the card endpoint"),
        (on_status.json_value()["card"].clone(), "the status entity"),
    ] {
        let image = value["image"].as_str().expect("the image is a string");
        assert!(
            image.starts_with(&format!("{}/proxy/", config.base_url)),
            "{who}: the image is a proxy link: {image}"
        );
        let (_, tail) = image.split_once("/proxy/").expect("a proxy path");
        let mut segments = tail.split('/');
        let sig = segments.next().expect("a signature segment");
        let encoded = segments.next().expect("an encoded url segment");
        assert_eq!(
            wired_be::services::media_proxy::decode(&config, sig, encoded).as_deref(),
            Ok("https://remote.example/og.png"),
            "{who}: the link decodes to the stored image"
        );
    }
}

/// A public status with no card is an answer, not a miss: the endpoint
/// returns JSON `null` with 200, and a linked-but-failed row the same.
#[tokio::test]
async fn the_card_endpoint_answers_null_without_a_card() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let plain = common::create_status(&pool, &alice, "cardless", Visibility::Public).await;
    let dead = common::create_status(&pool, &alice, "a dead link", Visibility::Public).await;
    link_failed_card(&pool, dead.id).await;

    for (status, who) in [(&plain, "nothing cached"), (&dead, "a failed link")] {
        let response = card_of(&app, None, &status.id.to_string()).await;
        common::assert_answer(&response, StatusCode::OK, who);
        assert_eq!(response.json_value(), Value::Null, "{who}: JSON null");
    }
}

/// The endpoint's misses are the shared status gate's: a malformed id, an
/// unknown id and a `private` status the caller may not see — anonymously
/// or as an unrelated scoped user — are one indistinguishable 404. A card
/// the cache never held is no miss at all: a cardless public status got
/// its 200 `null` in the test above.
#[tokio::test]
async fn the_card_endpoint_404s_every_miss() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let (_, stranger_token) = user_with_token(&pool, "mallory", SCOPES).await;
    let hidden = common::create_status(&pool, &alice, "whisper", Visibility::Private).await;

    common::assert_all_refused(
        &[
            card_of(&app, None, UNKNOWN_ID).await,
            card_of(&app, None, "not-an-uuid").await,
            card_of(&app, None, &hidden.id.to_string()).await,
            card_of(&app, Some(&stranger_token), &hidden.id.to_string()).await,
        ],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "a card read of a status this caller may not see",
    );
}

/// The auth gate is the show route's: a user token that lacks
/// `read:statuses` is the 403 even for a carded public status, and a
/// token that does not resolve is the extractor's 401.
#[tokio::test]
async fn the_card_endpoint_gates_auth() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let (_narrow, narrow_token) = user_with_token(&pool, "bob", "read:accounts").await;
    let post = common::create_status(
        &pool,
        &alice,
        "look https://example.test/post",
        Visibility::Public,
    )
    .await;
    link_ok_card(&pool, post.id).await;
    let id = post.id.to_string();

    let refused = card_of(&app, Some(&narrow_token), &id).await;
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "a token without the scope");
    common::assert_error(&refused, NO_SCOPE, "a token without the scope");

    let unauthorized = card_of(&app, Some("not-a-token"), &id).await;
    common::assert_answer(&unauthorized, StatusCode::UNAUTHORIZED, "a bad token");
    common::assert_error(&unauthorized, INVALID_TOKEN, "a bad token");
}
