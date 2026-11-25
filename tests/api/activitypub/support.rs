//! Shared consts and helpers for every submodule of the `activitypub`
//! area: the media type and instance origin the routes answer with, the
//! actor key-set consts and document readers, and the client-to-server
//! helpers the C2S files share — the fixture account, the mention and
//! status writers, the outbox poster and the media upload. Everything
//! shared here is `pub(super)`, and a submodule imports exactly the
//! members it calls; a helper only one submodule uses lives there instead.

use axum::{Router, body::Bytes, http::StatusCode};
use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::io::Cursor;
use uuid::Uuid;
use wired_be::models::{
    status::{self, NewStatus, Status as StoredStatus, Visibility},
    user::User,
};

use crate::common;

/// The media type of an `ActivityPub` document, charset included.
pub(super) const ACTIVITY_JSON: &str = "application/activity+json; charset=utf-8";
/// The instance origin of [`common::test_config`].
pub(super) const BASE: &str = "https://test.example";
/// The host of [`BASE`]: the one a delivery here is signed from, and the
/// one the reject lists below name.
pub(super) const HOST: &str = "test.example";
/// Public key material a fixture row can be checked against.
pub(super) const PUBLIC_PEM: &str =
    "-----BEGIN PUBLIC KEY-----\nMIIB-FIXTURE\n-----END PUBLIC KEY-----\n";
/// Every key of an actor document that carries neither avatar nor banner.
pub(super) const ACTOR_KEYS: [&str; 22] = [
    "@context",
    "alsoKnownAs",
    "attachment",
    "capabilities",
    "discoverable",
    "endpoints",
    "featured",
    "featuredTags",
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
pub(super) async fn fetch_activity_json(app: &Router, path: &str) -> common::TestResponse {
    common::TestRequest::get(path)
        .header("accept", "application/activity+json")
        .send(app)
        .await
}

/// The actor document of `user`, fetched over the route, asserted to be a
/// served actor before the body goes back to the test.
pub(super) async fn fetch_actor(pool: &PgPool, user: &User) -> Value {
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
pub(super) fn keys_with(extra: &[&'static str]) -> Vec<&'static str> {
    let mut keys: Vec<&str> = ACTOR_KEYS.to_vec();
    keys.extend_from_slice(extra);
    keys.sort_unstable();
    keys
}

/// Boundary the C2S upload bodies are framed with.
pub(super) const C2S_BOUNDARY: &str = "wired-c2s-test-boundary";
/// The `ActivityStreams` public collection.
pub(super) const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// A local account named `nickname` with a bearer token granting `scopes`.
pub(super) async fn c2s_account(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, scopes).await;
    (user, token)
}

/// A status by `author` that @-mentions `mentioned` — the fact the C2S
/// inbox pages on — inserted straight through the model.
pub(super) async fn mention_status(
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

/// Posts `activity` to `/users/{nickname}/outbox` as `token` (none: the
/// anonymous ask).
pub(super) async fn post_outbox(
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
pub(super) fn create_note(to: &Value, cc: &Value, content: &str) -> Value {
    json!({
        "type": "Create",
        "to": to,
        "cc": cc,
        "object": { "type": "Note", "content": content },
    })
}

/// The status id an outbox answer's `Location` names: the tail of the
/// `{BASE}/activities/{uuid}` address.
pub(super) fn location_status_id(response: &common::TestResponse) -> Uuid {
    let location = response
        .header("location")
        .expect("a 201 carries a Location");
    let tail = location
        .strip_prefix(&format!("{BASE}/activities/"))
        .unwrap_or(location);
    Uuid::parse_str(tail).expect("the Location tail is the minted id")
}

/// `GET /api/v1/statuses/{id}` as `token`.
pub(super) async fn show_status(app: &Router, token: &str, id: Uuid) -> common::TestResponse {
    common::TestRequest::get(&format!("/api/v1/statuses/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// `GET /api/v1/accounts/relationships?id={target}` as `token`, answered
/// for the one account it names.
pub(super) async fn relationship(app: &Router, token: &str, target: &User) -> Value {
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
pub(super) fn png_fixture(seed: u32) -> Vec<u8> {
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
pub(super) async fn upload_to_ap(
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
