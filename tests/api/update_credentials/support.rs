//! Shared consts and helpers every submodule of the `update_credentials` area
//! draws on: the route and byte fixtures, the request verbs, the fixture
//! accounts and apps, the stored-row read, the entity block readers and the
//! uploads-directory helpers of the profile-image tests.

use std::path::{Path, PathBuf};

use axum::{Router, body::Bytes, http::StatusCode};
use chrono::NaiveDate;
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::user::{Flag, User, get_by_id},
};

use crate::common;

/// The endpoint under test.
pub(super) const ROUTE: &str = "/api/v1/accounts/update_credentials";
/// The multipart boundary the test bodies are framed with.
pub(super) const BOUNDARY: &str = "wired-update-credentials-boundary";
/// The instance origin the fixtures live on (`Config::for_tests`).
pub(super) const BASE: &str = TEST_BASE_URL;
/// Where the test instance serves stored files from.
pub(super) const MEDIA: &str = "https://test.example/media";
/// The entity's default avatar, `Config::for_tests`' instance asset.
pub(super) const DEFAULT_AVATAR: &str = "https://test.example/media/default/avi.png";
/// The entity's default banner, likewise.
pub(super) const DEFAULT_HEADER: &str = "https://test.example/media/default/banner.png";
/// The 401 body `AuthUser` renders when no token was presented at all.
pub(super) const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 401 body for a token that does not resolve.
pub(super) const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 403 body an app-only token gets from the user-requiring extractor.
pub(super) const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body for a token without the endpoint's scope.
pub(super) const NO_SCOPE: &str = r#"{"error":"Insufficient permissions: write:accounts."}"#;
/// The magic bytes of a `PNG` and nothing else: the detector names a `png`,
/// no decoder reads an image from it.
pub(super) const TRUNCATED_PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";
/// A `BMP`: an image the detector names, and one the endpoint still
/// refuses.
pub(super) const BMP: &[u8] = b"BM\x36\x00\x00\x00\x00\x00\x36\x00\x00\x00";
/// A `PDF`: bytes with a type that is no image.
pub(super) const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";
/// Text: bytes no matcher can name at all.
pub(super) const TEXT: &[u8] = b"a plain text file, definitely not an image\n";

/// `PATCH` the endpoint with `body` as JSON.
pub(super) async fn patch_json(app: &Router, token: &str, body: &Value) -> common::TestResponse {
    common::TestRequest::patch(ROUTE)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// `PATCH` the endpoint with `fields` as an urlencoded form body.
pub(super) async fn patch_form(
    app: &Router,
    token: &str,
    fields: &[(&str, &str)],
) -> common::TestResponse {
    common::TestRequest::patch(ROUTE)
        .bearer(token)
        .form(fields)
        .send(app)
        .await
}

/// `PATCH` the endpoint with a `multipart/form-data` body of text parts.
pub(super) async fn patch_multipart(
    app: &Router,
    token: &str,
    parts: &[common::MultipartPart],
) -> common::TestResponse {
    common::TestRequest::patch(ROUTE)
        .bearer(token)
        .multipart(BOUNDARY, parts)
        .send(app)
        .await
}

/// A multipart text part.
pub(super) fn part(name: &'static str, value: &'static str) -> common::MultipartPart {
    common::MultipartPart::text(name, value)
}

/// Assert a 200 and hand back the body.
pub(super) fn ok_body(response: &common::TestResponse) -> Value {
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the update is accepted: {}",
        response.text()
    );
    response.json_value()
}

/// A user bound `write:accounts` token and the app to send it to, plus the
/// fixture user whose row the updates change.
pub(super) async fn authed_app(pool: &PgPool, nickname: &str) -> (Router, String, User) {
    let alice = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &alice, "write:accounts").await;
    (common::test_app(pool.clone()), token, alice)
}

/// The stored row behind `id`, for asserting what the write left behind.
pub(super) async fn stored(pool: &PgPool, id: Uuid) -> User {
    get_by_id(pool, id)
        .await
        .expect("the row read runs")
        .expect("the fixture row exists")
}

/// One field of the JSON body each boolean round sends.
pub(super) fn one_flag(field: &str, value: bool) -> Value {
    let mut body = serde_json::Map::new();
    let _old: Option<Value> = body.insert(field.to_owned(), json!(value));
    Value::Object(body)
}

/// The account every present-only assertion writes against: a name, the note
/// in both forms, the flags on, the posting defaults and Pleroma preferences
/// set, a birthday and a settings blob.
pub(super) async fn prefilled(pool: &PgPool, nickname: &str) -> User {
    common::create_local_user_with(pool, nickname, |mut new| {
        new.name = "Ada".to_owned();
        new.raw_bio = "old bio".to_owned();
        new.bio = "<p>old bio</p>".to_owned();
        new.locked = Flag::new(true);
        new.bot = Flag::new(true);
        new.discoverable = Flag::new(true);
        new.default_scope = "unlisted".to_owned();
        new.sensitive_default = Flag::new(true);
        new.language = Some("pt-BR".to_owned());
        new.hide_follows = Flag::new(true);
        new.no_rich_text = Flag::new(true);
        new.show_birthday = Flag::new(true);
        new.birthday = NaiveDate::from_ymd_opt(1990, 1, 31);
        new.settings_store = json!({"theme": "dark"});
        new
    })
    .await
}

/// The account and the router every `PATCH` of these tests writes through.
pub(super) async fn prefilled_app(pool: &PgPool) -> (Router, String, User) {
    let alice = prefilled(pool, "alice").await;
    let token = common::create_app_and_token(pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());
    (app, token, alice)
}

/// One profile metadata entry as the entity answers it.
pub(super) fn field(name: &str, value: &str) -> Value {
    json!({"name": name, "value": value, "verified_at": null})
}

/// The `source` object of a rendered account, present or panicked over:
/// the tests that ask for it are the ones asserting its shape.
pub(super) fn source_of(body: &Value) -> Value {
    body.get("source")
        .cloned()
        .expect("update_credentials renders the source block")
}

/// The `pleroma` block of an answered owner entity.
pub(super) fn pleroma_of(body: &Value) -> Value {
    body.get("pleroma")
        .cloned()
        .expect("update_credentials renders the pleroma block")
}

/// A throwaway uploads directory, removed when the returned guard drops:
/// profile-image tests store real files and must not write into the
/// repository's own `./uploads`.
pub(super) fn uploads_dir() -> TempDir {
    TempDir::new().expect("temp dir for uploads")
}

/// A user bound `write:accounts` token, the app that serves it out of
/// `dir`, and the fixture user whose row the updates change.
pub(super) async fn authed_app_with_uploads(
    pool: &PgPool,
    nickname: &str,
    dir: &TempDir,
) -> (Router, String, User) {
    let alice = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &alice, "write:accounts").await;
    (
        common::test_app_with_uploads_dir(pool.clone(), dir.path()),
        token,
        alice,
    )
}

/// An image file part uploaded under `field`. The client always claims
/// `image/png` and names the part `picture.png`: what is stored comes from
/// the bytes, so a test that wants to prove that (and one that wants to see
/// the claim refused) can reuse this one builder.
pub(super) fn file_part(field: &'static str, bytes: &[u8]) -> common::MultipartPart {
    common::MultipartPart::file(
        field,
        "picture.png",
        "image/png",
        Bytes::copy_from_slice(bytes),
    )
}

/// A `PATCH` multipart body of one image part and nothing else.
pub(super) async fn upload(
    app: &Router,
    token: &str,
    field: &'static str,
    bytes: &[u8],
) -> common::TestResponse {
    patch_multipart(app, token, &[file_part(field, bytes)]).await
}

/// The URL of a string value, for comparing against what the row stored.
///
/// # Panics
///
/// If the value is not a string (an entity that carried no URL at all).
pub(super) fn url_of(value: &Value) -> String {
    value
        .as_str()
        .unwrap_or_else(|| panic!("{value} is not a URL string"))
        .to_owned()
}

/// The uploads key a media URL names: the part below [`MEDIA`].
///
/// # Panics
///
/// If the URL is not served from [`MEDIA`].
pub(super) fn key_of(url: &str) -> String {
    url.strip_prefix(&format!("{MEDIA}/"))
        .unwrap_or_else(|| panic!("{url} is not served from {MEDIA}"))
        .to_owned()
}

/// Where the file behind a media URL lives inside `dir`.
fn path_of(dir: &TempDir, url: &str) -> PathBuf {
    dir.path().join(key_of(url))
}

/// The bytes the uploads directory holds behind a media URL, or `None` when
/// no file is there (anymore).
pub(super) fn served(dir: &TempDir, url: &str) -> Option<Vec<u8>> {
    std::fs::read(path_of(dir, url)).ok()
}

/// Every file under `dir`, counted: zero is an empty uploads directory, and
/// a profile-image write that stored one file moves it by one.
pub(super) fn stored_files(dir: &TempDir) -> usize {
    count_files(dir.path())
}

/// The files under `path`, recursively (the uploader namespaces them by
/// user id, so they are never directly in the root).
fn count_files(path: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let child = entry.path();
            if child.is_dir() {
                count_files(&child)
            } else {
                1
            }
        })
        .sum()
}

/// Assert a 422 with the exact error body, and name it for the caller.
pub(super) fn assert_422(response: &common::TestResponse, message: &str, why: &str) {
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{why}: {}",
        response.text()
    );
    assert_eq!(
        response.text(),
        format!(r#"{{"error":"{message}"}}"#),
        "{why}"
    );
}

/// The extension of a stored media URL's key: the part after the last dot of
/// its name.
///
/// # Panics
///
/// If the key has no dot, which a generated key always has.
pub(super) fn extension_of(url: &str) -> String {
    let key = key_of(url);
    let (_, extension) = key
        .rsplit_once('.')
        .unwrap_or_else(|| panic!("{url} is not a named key"));
    extension.to_owned()
}
