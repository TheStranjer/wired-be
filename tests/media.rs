//! Tests for the media route (`src/api/media_files.rs`) and the local uploader
//! behind it: a file stored through [`wired_be::uploads`] comes back over
//! `GET /media/{key}` with the type, caching and safety headers of the route,
//! keys that could walk out of the uploads directory answer 404 exactly like
//! absent files do, and deleting a key takes the file away.
//!
//! Every test stores into a [`TempDir`], never into the repository's own
//! `./uploads`, through the `common::test_*_uploads_dir` helpers.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use std::path::Path;

use axum::{Router, body::Bytes, http::StatusCode};
use sqlx::PgPool;
use tempfile::TempDir;
use uuid::Uuid;
use wired_be::{error::AppError, uploads};

/// The bytes this suite stores: the route never looks inside a file, so a fake
/// PNG header is enough to stand for an image.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n-fake-image-data";
/// Bytes that would be dangerous if a browser rendered them.
const HTML: &[u8] = b"<!doctype html><script>alert(1)</script>";
/// The `cache-control` stored media is served with.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// Where the test instance serves files from.
const MEDIA_BASE_URL: &str = "https://test.example/media";

/// A throwaway uploads directory, removed when the returned guard drops.
fn uploads_dir() -> TempDir {
    TempDir::new().expect("temp dir for uploads")
}

/// Stores `body` at `key` in `dir` with the same configuration the router
/// under test is built with.
async fn store(dir: &Path, key: &str, content_type: &str, body: &'static [u8]) {
    common::test_uploader(dir)
        .put(key, Bytes::from_static(body), content_type)
        .await
        .unwrap_or_else(|err| panic!("storing {key} must succeed: {err}"));
}

/// Reads back the file `key` names in `dir`, if the uploader still serves it.
async fn served(dir: &Path, key: &str) -> Result<Bytes, AppError> {
    common::test_uploader(dir).read(key).await
}

/// GETs `path` through `app`.
async fn get(app: &Router, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).send(app).await
}

/// The exact body of a JSON error response.
fn error_body(message: &str) -> String {
    format!(r#"{{"error":"{message}"}}"#)
}

/// A stored file is served back byte for byte, as the media type of its
/// extension, cached for a year, un-sniffable and inline.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn put_then_get_serves_the_file(pool: PgPool) {
    let dir = uploads_dir();
    let key = "018f4c2e-1a2b-7000-8000-000000000000/9b3c1d0e4f5a6b7c.png";
    store(dir.path(), key, "image/png", PNG).await;
    let app = common::test_app_with_uploads_dir(pool, dir.path());

    let response = get(&app, &format!("/media/{key}")).await;
    assert_eq!(response.status, StatusCode::OK, "a stored file is found");
    assert_eq!(
        response.body,
        Bytes::from_static(PNG),
        "the bytes come back unchanged"
    );
    assert_eq!(
        response.header("content-type"),
        Some("image/png"),
        "the type follows the extension"
    );
    assert_eq!(
        response.header("cache-control"),
        Some(IMMUTABLE),
        "keys are random and never reused, so the response is immutable"
    );
    assert_eq!(
        response.header("x-content-type-options"),
        Some("nosniff"),
        "browsers must not guess another type"
    );
    assert_eq!(
        response.header("content-disposition"),
        Some("inline"),
        "an image renders in the browser"
    );
}

/// The extension decides the served type, for every media type the uploader
/// knows; all of them stay inline.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn every_known_extension_keeps_its_type(pool: PgPool) {
    let dir = uploads_dir();
    let app = common::test_app_with_uploads_dir(pool, dir.path());
    for (extension, content_type) in [
        ("jpg", "image/jpeg"),
        ("jpeg", "image/jpeg"),
        ("png", "image/png"),
        ("gif", "image/gif"),
        ("webp", "image/webp"),
        ("mp4", "video/mp4"),
        ("webm", "video/webm"),
        ("ogg", "audio/ogg"),
        ("mp3", "audio/mpeg"),
        ("wav", "audio/wav"),
    ] {
        let key = format!("u/file.{extension}");
        store(dir.path(), &key, "application/octet-stream", PNG).await;
        let response = get(&app, &format!("/media/{key}")).await;
        assert_eq!(response.status, StatusCode::OK, "stored as {key}");
        assert_eq!(
            response.header("content-type"),
            Some(content_type),
            "a .{extension} file serves as {content_type}"
        );
        assert_eq!(
            response.header("content-disposition"),
            Some("inline"),
            "{content_type} is media a browser renders itself"
        );
    }
}

/// An uploaded HTML file — or SVG, or anything else that could run — is never
/// served as a document: it arrives as an octet-stream attachment, with its
/// bytes unchanged.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn non_media_is_served_as_an_attachment(pool: PgPool) {
    let dir = uploads_dir();
    let app = common::test_app_with_uploads_dir(pool, dir.path());
    for (extension, body) in [
        ("html", HTML),
        ("svg", HTML),
        ("xht", HTML),
        ("js", HTML),
        ("bin", PNG),
        ("noext", PNG),
    ] {
        let key = format!("u/evil.{extension}");
        store(dir.path(), &key, "text/html", body).await;
        let response = get(&app, &format!("/media/{key}")).await;
        assert_eq!(response.status, StatusCode::OK, "stored as {key}");
        assert_eq!(
            response.header("content-type"),
            Some("application/octet-stream"),
            "{key} has no type a browser would render"
        );
        assert_eq!(
            response.header("content-disposition"),
            Some("attachment"),
            "{key} downloads instead of displaying"
        );
        assert_eq!(
            response.body,
            Bytes::from_static(body),
            "{key} keeps its bytes"
        );
    }
}

/// Nothing about the route reveals the filesystem: a key that could walk out
/// of the uploads directory answers exactly like an absent file, and the file
/// it pointed at is not served.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn traversal_attempts_are_not_found(pool: PgPool) {
    let dir = uploads_dir();
    std::fs::write(dir.path().join("outside.txt"), b"a secret").expect("secret file writes");
    let uploads = dir.path().join("uploads");
    std::fs::create_dir_all(&uploads).expect("uploads dir is created");
    let app = common::test_app_with_uploads_dir(pool, &uploads);

    for path in [
        "/media/../outside.txt",
        "/media/uploads/../outside.txt",
        "/media/%2e%2e/outside.txt",
        "/media/%2E%2E%2Foutside.txt",
        "/media/%252e%252e/outside.txt",
        "/media/..%2f..%2foutside.txt",
        "/media/%2e%2e%2f%2e%2e%2fetc/passwd",
        "/media/..",
        "/media//outside.txt",
        "/media/%00outside.txt",
        "/media/a/../../b.png",
    ] {
        let response = get(&app, path).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{path} must not reach anything outside the uploads directory"
        );
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "the rejection of {path} is the standard JSON error (decision 8)"
        );
        assert_eq!(
            response.text(),
            error_body("Record not found"),
            "an unusable key is indistinguishable from a missing file, and never reveals the path"
        );
    }
}

/// A key that holds nothing is a JSON 404; so is a bare `/media`, which is not
/// a route at all and falls through to the app-wide fallback.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn missing_files_are_json_not_found(pool: PgPool) {
    let dir = uploads_dir();
    let app = common::test_app_with_uploads_dir(pool, dir.path());
    let response = get(&app, "/media/u/nope.png").await;
    assert_eq!(response.status, StatusCode::NOT_FOUND, "an absent key 404s");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "errors are JSON"
    );
    assert_eq!(response.text(), error_body("Record not found"), "404 body");

    let bare = get(&app, "/media").await;
    assert_eq!(bare.status, StatusCode::NOT_FOUND, "no key at all 404s");
    assert_eq!(
        bare.text(),
        error_body("Not found"),
        "that is the router-level 404"
    );
}

/// Deleting a key takes the file away: it is gone from disk, unreadable
/// through the uploader and a 404 over HTTP.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn delete_removes_the_file(pool: PgPool) {
    let dir = uploads_dir();
    let key = "u/gone.png";
    store(dir.path(), key, "image/png", PNG).await;
    let path = dir.path().join(key);
    assert!(path.is_file(), "storing wrote the file to disk");

    let app = common::test_app_with_uploads_dir(pool, dir.path());
    assert_eq!(
        get(&app, &format!("/media/{key}")).await.status,
        StatusCode::OK,
        "and the route served it"
    );

    common::test_uploader(dir.path())
        .delete(key)
        .await
        .unwrap_or_else(|err| panic!("deleting {key} must succeed: {err}"));
    assert!(!path.exists(), "the file is removed from disk");
    assert!(
        matches!(served(dir.path(), key).await, Err(AppError::NotFound)),
        "reading a deleted key is a 404"
    );
    let response = get(&app, &format!("/media/{key}")).await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "the route no longer serves a deleted key"
    );
    assert_eq!(
        common::TestRequest::delete(&format!("/media/{key}"))
            .send(&app)
            .await
            .status,
        StatusCode::METHOD_NOT_ALLOWED,
        "the media route only answers GET"
    );
}

/// Deleting a key that holds nothing is a 404 from the uploader, not a silent
/// success.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn deleting_an_absent_key_is_not_found(_pool: PgPool) {
    let dir = uploads_dir();
    let err = common::test_uploader(dir.path())
        .delete("u/never-stored.png")
        .await
        .unwrap_err();
    assert!(
        matches!(err, AppError::NotFound),
        "an absent file is a 404, got {err}"
    );
}

/// The whole path a client takes: a key from [`uploads::media_key`] stores,
/// serves, and is announced at `{MEDIA_BASE_URL}/{key}` — which is this route.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_generated_key_round_trips(pool: PgPool) {
    let dir = uploads_dir();
    let key = uploads::media_key(Uuid::now_v7(), Some("photo.jpeg"), "image/png");
    store(dir.path(), &key, "image/png", PNG).await;
    let app = common::test_app_with_uploads_dir(pool, dir.path());

    let response = get(&app, &format!("/media/{key}")).await;
    assert_eq!(response.status, StatusCode::OK, "the generated key serves");
    assert_eq!(
        response.header("content-type"),
        Some("image/png"),
        "the generated extension is the uploaded type: {key}"
    );
    assert_eq!(
        common::test_uploader(dir.path()).public_url(&key),
        format!("{MEDIA_BASE_URL}/{key}"),
        "public_url points at this route"
    );
}

/// The backend refuses to store, read or delete a key it would not accept in
/// a URL, whatever the route does.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_backend_rejects_invalid_keys(_pool: PgPool) {
    let dir = uploads_dir();
    let uploader = common::test_uploader(dir.path());
    for key in [
        "",
        "/absolute.png",
        "../escape.png",
        "u/../../escape.png",
        "u//double.png",
        "u/trailing/",
        "u/space name.png",
        "u/percent%20.png",
        "u\\backslash.png",
    ] {
        let errors = [
            uploader
                .put(key, Bytes::new(), "image/png")
                .await
                .unwrap_err(),
            uploader.read(key).await.unwrap_err(),
            uploader.delete(key).await.unwrap_err(),
        ];
        let all_bad_requests = errors
            .iter()
            .all(|err| matches!(err, AppError::BadRequest(_)));
        assert!(
            all_bad_requests,
            "{key:?} must be a 400 from every operation, got {errors:?}"
        );
        // A hostile filename contributes nothing but an extension: the key
        // generated from it is valid, so the backend reports it absent rather
        // than invalid.
        let generated = uploads::media_key(Uuid::now_v7(), Some(key), "application/octet-stream");
        let err = uploader.read(&generated).await.unwrap_err();
        assert!(
            matches!(err, AppError::NotFound),
            "the key generated from {key:?} is a valid key, got {err}"
        );
    }
}

/// Keys are namespaced by user, so one user's directory holds several files
/// and nested keys work the same way.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn many_files_share_a_directory(pool: PgPool) {
    let dir = uploads_dir();
    for (key, content_type) in [
        ("u/two/first.png", "image/png"),
        ("u/two/second.mp4", "video/mp4"),
        ("u/two/third.bin", "application/octet-stream"),
    ] {
        store(dir.path(), key, content_type, PNG).await;
    }
    let app = common::test_app_with_uploads_dir(pool, dir.path());
    for (key, content_type) in [
        ("u/two/first.png", "image/png"),
        ("u/two/second.mp4", "video/mp4"),
    ] {
        let response = get(&app, &format!("/media/{key}")).await;
        assert_eq!(response.status, StatusCode::OK, "nested key {key} serves");
        assert_eq!(
            response.header("content-type"),
            Some(content_type),
            "type for nested key {key}"
        );
    }
    let download = get(&app, "/media/u/two/third.bin").await;
    assert_eq!(
        download.header("content-disposition"),
        Some("attachment"),
        "a nested .bin is still only a download"
    );
}
