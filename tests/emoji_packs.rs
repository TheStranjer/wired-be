//! Integration tests for the emoji-pack endpoints of
//! `src/api/emoji_packs.rs`: the pack listing and its pagination, the
//! single-pack view, the zip archive download and its sharing gate, the
//! util emoji map over synced `custom_emojis` rows, and the static
//! `GET /emoji/{pack}/{file}` route with its media safety headers. All of
//! them are public (no authentication), and unknown or unsafe pack names
//! answer one shared 404.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use std::{
    collections::BTreeMap,
    io::{Cursor, Read as _},
    path::Path,
};

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    config::TEST_BASE_URL,
    models::custom_emoji::{self, NewCustomEmoji},
    services::emoji_packs::{self, Pack, PackMeta},
};

/// Bytes for every fixture image file.
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nfake";

/// A manifest naming `files` (shortcode, file name) pairs, sharing its
/// files when `shared`.
fn pack(shared: bool, files: &[(&str, &str)]) -> Pack {
    Pack {
        pack: PackMeta {
            share_files: shared,
            ..PackMeta::default()
        },
        files: files
            .iter()
            .map(|(shortcode, file)| ((*shortcode).to_owned(), (*file).to_owned()))
            .collect(),
    }
}

/// Writes pack `name` under the packs root `dir`: the manifest plus a real
/// PNG for every file it lists.
async fn seed_pack(dir: &Path, name: &str, manifest: &Pack) {
    emoji_packs::save_pack(dir, name, manifest)
        .await
        .expect("fixture pack saves");
    for file in manifest.files.values() {
        let path = dir.join(name).join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("fixture directories create");
        }
        std::fs::write(&path, PNG_BYTES).expect("fixture image writes");
    }
}

/// A minimal shared pack with two images, one nested, for the download and
/// static-route tests.
fn cats_pack() -> Pack {
    pack(true, &[("cat", "cat.png"), ("kitten", "sub/kitten.png")])
}

/// The sorted entry names and bytes of a zip archive body.
fn read_zip(body: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(body)).expect("response is a zip");
    let mut entries = BTreeMap::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).expect("entry readable");
        let name = entry.name().expect("entry name is utf-8").into_owned();
        let mut payload = Vec::new();
        let read: usize = entry.read_to_end(&mut payload).expect("entry reads fully");
        assert_eq!(read, payload.len(), "entry {name} came back complete");
        let _previous: Option<Vec<u8>> = entries.insert(name, payload);
    }
    entries
}

/// The `error` string of a JSON error response.
fn error_message(response: &common::TestResponse) -> Option<String> {
    Some(response.json_value().get("error")?.as_str()?.to_owned())
}

/// The keys of the `packs` object of a listing body, in serialization
/// order.
fn pack_names(body: &Value) -> Vec<&str> {
    body["packs"]
        .as_object()
        .expect("packs is an object")
        .keys()
        .map(String::as_str)
        .collect()
}

/// `GET /api/v1/pleroma/emoji/packs` lists every pack keyed by name with
/// the total count, serializes each as Pleroma's `pack`/`files`/
/// `files_count` triple, skips entries that are not packs, and answers
/// without any credentials.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn lists_packs_with_their_total_count(pool: PgPool) {
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "bb", &pack(true, &[("b", "b.png")])).await;
    seed_pack(
        dir.path(),
        "aa",
        &pack(false, &[("a", "a.png"), ("a2", "a2.gif")]),
    )
    .await;
    std::fs::create_dir_all(dir.path().join(".no-manifest")).unwrap();
    std::fs::write(dir.path().join("stray.png"), PNG_BYTES).unwrap();

    let app = common::test_app_with_emoji_dir(pool, dir.path());
    let response = common::TestRequest::get("/api/v1/pleroma/emoji/packs")
        .bearer("not-a-real-token")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "200 without real auth");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the listing is JSON"
    );
    let body = response.json_value();
    assert_eq!(body["count"], json!(2), "two loadable packs on disk");
    assert_eq!(pack_names(&body), ["aa", "bb"], "keyed and sorted by name");
    assert_eq!(
        body["packs"]["aa"],
        json!({
            "pack": {"share-files": false},
            "files": {"a": "a.png", "a2": "a2.gif"},
            "files_count": 2,
        }),
        "one pack in Pleroma's serialization"
    );
}

/// `/packs` pages by name: full, partial and past-the-end windows of the
/// ordered pack list, with `count` fixed at the total; a huge `page_size`
/// is clamped to the 100-item cap instead of failing.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn pack_listing_pages_by_name(pool: PgPool) {
    let dir = tempfile::TempDir::new().unwrap();
    for name in ["aa", "bb", "cc"] {
        seed_pack(dir.path(), name, &pack(true, &[("x", "x.png")])).await;
    }
    let app = common::test_app_with_emoji_dir(pool, dir.path());

    let first = common::TestRequest::get("/api/v1/pleroma/emoji/packs?page=1&page_size=2")
        .send(&app)
        .await
        .json_value();
    assert_eq!(pack_names(&first), ["aa", "bb"], "first page of two");
    assert_eq!(first["count"], json!(3), "count spans all pages");

    let second = common::TestRequest::get("/api/v1/pleroma/emoji/packs?page=2&page_size=2")
        .send(&app)
        .await
        .json_value();
    assert_eq!(
        pack_names(&second),
        ["cc"],
        "second page holds the remainder"
    );

    let past = common::TestRequest::get("/api/v1/pleroma/emoji/packs?page=4&page_size=2")
        .send(&app)
        .await
        .json_value();
    assert_eq!(past["packs"], json!({}), "past the end is an empty page");

    let clamped = common::TestRequest::get("/api/v1/pleroma/emoji/packs?page_size=5000")
        .send(&app)
        .await;
    assert_eq!(clamped.status, StatusCode::OK, "an oversized page works");
    assert_eq!(
        pack_names(&clamped.json_value()),
        ["aa", "bb", "cc"],
        "the cap is at least wide enough for every pack here"
    );
}

/// `/pack` serves one pack with its files paginated by shortcode
/// (`files_count` still counts the whole pack), and answers 404 for an
/// unknown pack, a name that could traverse, and 400 for a missing one.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn shows_one_pack_with_files_paginated(pool: PgPool) {
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(
        dir.path(),
        "cats",
        &pack(true, &[("a", "a.png"), ("b", "b.png"), ("c", "c.png")]),
    )
    .await;
    let app = common::test_app_with_emoji_dir(pool, dir.path());

    let page = common::TestRequest::get("/api/v1/pleroma/emoji/pack?name=cats&page=2&page_size=2")
        .send(&app)
        .await;
    assert_eq!(page.status, StatusCode::OK, "the second page answers");
    assert_eq!(
        page.json_value(),
        json!({
            "pack": {"share-files": true},
            "files": {"c": "c.png"},
            "files_count": 3,
        }),
        "one of three shortcodes, counted whole"
    );

    for (query, want_status, want_body) in [
        ("name=nope", StatusCode::NOT_FOUND, "Pack not found"),
        (
            "name=%2e%2e%2fsecret",
            StatusCode::NOT_FOUND,
            "Pack not found",
        ),
        ("", StatusCode::BAD_REQUEST, "pack name cannot be empty"),
    ] {
        let response = common::TestRequest::get(&format!("/api/v1/pleroma/emoji/pack?{query}"))
            .send(&app)
            .await;
        assert_eq!(response.status, want_status, "status for ?{query}");
        assert_eq!(
            error_message(&response).as_deref(),
            Some(want_body),
            "body for ?{query}"
        );
    }
}

/// The archive download: shared packs come back as a zip holding
/// `pack.json` and every file with the download headers; a pack that does
/// not share is a 403, an unknown pack a 404 and a missing name a 400.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn archives_a_shared_pack_as_a_zip_download(pool: PgPool) {
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &cats_pack()).await;
    seed_pack(dir.path(), "dogs", &pack(false, &[("dog", "dog.png")])).await;
    std::fs::write(dir.path().join("outside.txt"), b"do not package me").unwrap();
    let app = common::test_app_with_emoji_dir(pool, dir.path());

    let response = common::TestRequest::get("/api/v1/pleroma/emoji/packs/archive?name=cats")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the shared pack downloads");
    assert_eq!(
        response.header("content-type"),
        Some("application/zip"),
        "the body is a zip"
    );
    assert_eq!(
        response.header("content-disposition"),
        Some(r#"attachment; filename="cats.zip""#),
        "named after the pack"
    );
    let entries = read_zip(&response.body);
    let names: Vec<&str> = entries.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        ["cat.png", "pack.json", "sub/kitten.png"],
        "the manifest plus every file at its relative path (sorted here)"
    );
    assert_eq!(
        entries["cat.png"], PNG_BYTES,
        "the file bytes survive the round trip"
    );
    assert_eq!(
        entries["sub/kitten.png"], PNG_BYTES,
        "nested entries keep their path and bytes"
    );
    let manifest: Value = serde_json::from_slice(&entries["pack.json"]).unwrap();
    assert_eq!(
        manifest["files"]["cat"],
        json!("cat.png"),
        "the manifest rode along"
    );

    let denied = common::TestRequest::get("/api/v1/pleroma/emoji/packs/archive?name=dogs")
        .send(&app)
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "unshared is a 403");
    assert_eq!(
        error_message(&denied).as_deref(),
        Some("Pack is not shared"),
        "the refusal names sharing"
    );

    let missing = common::TestRequest::get("/api/v1/pleroma/emoji/packs/archive?name=nope")
        .send(&app)
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "unknown is a 404");
    assert_eq!(
        error_message(&missing).as_deref(),
        Some("Pack not found"),
        "the refusal names the pack kind"
    );

    let no_name = common::TestRequest::get("/api/v1/pleroma/emoji/packs/archive")
        .send(&app)
        .await;
    assert_eq!(no_name.status, StatusCode::BAD_REQUEST, "no name is a 400");
    assert_eq!(
        error_message(&no_name).as_deref(),
        Some("pack name cannot be empty"),
        "Pleroma's empty-value message"
    );
}

/// `GET /api/v1/pleroma/emoji` is the util emoji map over the synced
/// `custom_emojis` table: every local enabled emoji keyed by shortcode with
/// its image url and tags; disabled and remote rows stay out, and a row
/// that no pack owns still shows up.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_util_map_covers_every_local_custom_emoji(pool: PgPool) {
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &cats_pack()).await;
    seed_pack(dir.path(), "dogs", &pack(false, &[("dog", "dog.png")])).await;
    let synced: usize = emoji_packs::sync_custom_emojis(&pool, TEST_BASE_URL, dir.path())
        .await
        .expect("fixture packs sync");
    assert_eq!(synced, 3, "three pack files stored");
    for (shortcode, url, domain, disabled) in [
        (
            "upload",
            "https://cdn.example/custom/upload.png",
            None,
            false,
        ),
        (
            "hidden",
            "https://cdn.example/custom/hidden.png",
            None,
            true,
        ),
        (
            "remote-cat",
            "https://remote.example/emoji/cat.png",
            Some("remote.example"),
            false,
        ),
    ] {
        let _row: custom_emoji::CustomEmoji = custom_emoji::upsert(
            &pool,
            &NewCustomEmoji {
                domain: domain.map(str::to_owned),
                disabled,
                ..NewCustomEmoji::local(shortcode, url)
            },
        )
        .await
        .expect("fixture emoji upserts");
    }

    let app = common::test_app_with_emoji_dir(pool, dir.path());
    let response = common::TestRequest::get("/api/v1/pleroma/emoji")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the map answers");
    assert_eq!(
        response.json_value(),
        json!({
            "cat": {
                "image_url": "https://test.example/emoji/cats/cat.png",
                "tags": ["cats"],
            },
            "dog": {
                "image_url": "https://test.example/emoji/dogs/dog.png",
                "tags": ["dogs"],
            },
            "kitten": {
                "image_url": "https://test.example/emoji/cats/sub/kitten.png",
                "tags": ["cats"],
            },
            "upload": {
                "image_url": "https://cdn.example/custom/upload.png",
                "tags": [],
            },
        }),
        "local enabled emojis only, with the synced pack urls and tags"
    );
}

/// With nothing synced the map is the empty object, JSON-typed, no auth.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_util_map_is_empty_without_emojis(pool: PgPool) {
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/api/v1/pleroma/emoji")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "an empty map is still 200");
    assert_eq!(response.text(), "{}", "no rows, no entries");
}

/// The static route serves pack images with the media safety headers and
/// nothing else: manifests, web files and unknown or unsafe names all get
/// the same 404 as an absent file, so the route cannot probe the pack
/// directory.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn static_route_serves_only_pack_images(pool: PgPool) {
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &cats_pack()).await;
    std::fs::write(dir.path().join("cats").join("icon.svg"), b"<svg/>").unwrap();
    std::fs::write(dir.path().join("cats").join("page.html"), b"<h1>hi</h1>").unwrap();
    std::fs::write(dir.path().join("outside.png"), PNG_BYTES).unwrap();
    let app = common::test_app_with_emoji_dir(pool, dir.path());

    let response = common::TestRequest::get("/emoji/cats/cat.png")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "a pack image serves");
    assert_eq!(response.body.as_ref(), PNG_BYTES, "served byte for byte");
    assert_eq!(
        response.header("content-type"),
        Some("image/png"),
        "the type comes from the extension"
    );
    assert_eq!(
        response.header("x-content-type-options"),
        Some("nosniff"),
        "the type may not be sniffed over"
    );
    assert_eq!(
        response.header("content-disposition"),
        Some("inline"),
        "images render"
    );
    assert_eq!(
        response.header("cache-control"),
        Some("public, max-age=86400"),
        "pack images cache for a day"
    );

    let nested = common::TestRequest::get("/emoji/cats/sub/kitten.png")
        .send(&app)
        .await;
    assert_eq!(
        nested.status,
        StatusCode::NOT_FOUND,
        "nested files are outside the one-segment route"
    );

    for path in [
        "/emoji/cats/pack.json",
        "/emoji/cats/icon.svg",
        "/emoji/cats/page.html",
        "/emoji/cats/missing.png",
        "/emoji/nope/cat.png",
        "/emoji/%2e%2e/outside.png",
        "/emoji/cats/%2e%2e%2foutside.png",
        "/emoji/.git/config.png",
    ] {
        let miss = common::TestRequest::get(path).send(&app).await;
        assert_eq!(miss.status, StatusCode::NOT_FOUND, "{path} is a 404");
        assert_eq!(
            miss.header("content-type"),
            Some("application/json"),
            "misses answer as JSON errors, not as files: {path}"
        );
    }
}

/// The endpoints are GET-only; other verbs keep axum's 405 on the routed
/// paths (there is nothing to auth-fail: all four are public).
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn other_methods_are_not_routed(pool: PgPool) {
    let app = common::test_app(pool);
    for path in [
        "/api/v1/pleroma/emoji",
        "/api/v1/pleroma/emoji/packs",
        "/api/v1/pleroma/emoji/pack",
    ] {
        let response = common::TestRequest::post(path).send(&app).await;
        assert_eq!(
            response.status,
            StatusCode::METHOD_NOT_ALLOWED,
            "a known path with the wrong verb keeps axum's 405: {path}"
        );
    }
}
