//! Integration tests for the emoji-pack endpoints of
//! `src/api/emoji_packs.rs`: the pack listing and its pagination, the
//! single-pack view, the zip archive download and its sharing gate, the
//! util emoji map over synced `custom_emojis` rows, the static
//! `GET /emoji/{pack}/{file}` route with its media safety headers, the
//! admin write routes of WBE-211: create, metadata patch and delete
//! under `admin:write`, their validation refusals, the `custom_emojis`
//! resync they trigger and the `emoji_pack_*` moderation entries they
//! leave behind, the emoji-file routes of WBE-215 under `/packs/files`:
//! add (uploaded part and fetched URL), rename with `force`, and delete
//! — each answering the pack's full `files` object, resyncing the custom
//! emojis, and refusing an unknown pack with the 400 `{"error":"Pack
//! <name> does not exist"}` — and the install routes of WBE-216: the
//! remote listing relay, the pack download from a mock remote instance
//! (shared, fallback and refused), the uploaded-zip install and the
//! filesystem import, under `admin:read` for the GETs and `admin:write`
//! for the POSTs. The reads answer without authentication and unknown
//! or unsafe pack names answer the reads' shared 404; every write
//! demands an admin bearer token.

#![cfg(test)]

use std::{
    collections::BTreeMap,
    io::{Cursor, Read as _, Write as _},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{StatusCode, Uri, header},
    response::Response,
    routing::get,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        custom_emoji::{self, NewCustomEmoji},
        user::{Flag, NewUser},
    },
    services::emoji_packs::{self, Pack, PackMeta},
};

use crate::common;

/// Bytes for every fixture image file.
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nfake";
/// Magic bytes the emoji-file detection names as `image/gif`.
const GIF_BYTES: &[u8] = b"GIF89a....";
/// Bytes no accepted image matcher names.
const NOT_AN_IMAGE: &[u8] = b"print(1); // definitely no image";
/// Boundary the multipart file-route bodies are written with.
const BOUNDARY: &str = "wired-test-boundary";

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
#[tokio::test]
async fn lists_packs_with_their_total_count() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn pack_listing_pages_by_name() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn shows_one_pack_with_files_paginated() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn archives_a_shared_pack_as_a_zip_download() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn the_util_map_covers_every_local_custom_emoji() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn the_util_map_is_empty_without_emojis() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn static_route_serves_only_pack_images() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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

/// The read routes are GET-only and the write routes take POST, PATCH and
/// DELETE on `/pack`; other verbs keep axum's 405 on the routed paths (a
/// routed write with no credentials is the auth extractor's refusal, not
/// a 405, and the write-auth test covers that).
#[tokio::test]
async fn other_methods_are_not_routed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    for path in [
        "/api/v1/pleroma/emoji",
        "/api/v1/pleroma/emoji/packs",
        "/api/v1/pleroma/emoji/packs/archive",
    ] {
        let response = common::TestRequest::post(path).send(&app).await;
        assert_eq!(
            response.status,
            StatusCode::METHOD_NOT_ALLOWED,
            "a known read path with the wrong verb keeps axum's 405: {path}"
        );
    }
    let put = common::TestRequest::put("/api/v1/pleroma/emoji/pack")
        .send(&app)
        .await;
    assert_eq!(
        put.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "PUT is not one of /pack's write verbs"
    );
}

/// A local admin named `nickname` with a bearer token granting `scopes`.
async fn admin_token(pool: &PgPool, nickname: &str, scopes: &str) -> String {
    let admin = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    common::create_app_and_token(pool, &admin, scopes).await
}

/// A local user with no staff role, with a bearer token granting `scopes`.
async fn plain_token(pool: &PgPool, nickname: &str, scopes: &str) -> String {
    let user = common::create_local_user(pool, nickname).await;
    common::create_app_and_token(pool, &user, scopes).await
}

/// The `(action, message, actor)` triples of the moderation log, oldest
/// write first.
async fn log_rows(pool: &PgPool) -> Vec<(String, String, String)> {
    sqlx::query_as::<_, (String, String, String)>(
        "SELECT action, message, actor_nickname FROM moderation_log ORDER BY inserted_at, id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The expected form of one pack-write log row by `actor`.
fn log_row(actor: &str, action: &str, verb: &str, pack: &str) -> (String, String, String) {
    (
        action.to_owned(),
        format!("@{actor} {verb} emoji pack: {pack}"),
        actor.to_owned(),
    )
}

/// The `GET /api/v1/custom_emojis` body as `(category, shortcode)` pairs
/// in the endpoint's category-then-shortcode order.
async fn served_emojis(app: &axum::Router) -> Vec<(String, String)> {
    let response = common::TestRequest::get("/api/v1/custom_emojis")
        .send(app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "custom_emojis answers");
    response
        .json_value()
        .as_array()
        .expect("the endpoint answers an array")
        .iter()
        .map(|emoji| {
            (
                emoji["category"].as_str().unwrap_or_default().to_owned(),
                emoji["shortcode"]
                    .as_str()
                    .expect("an emoji entity")
                    .to_owned(),
            )
        })
        .collect()
}

/// `POST /pack` creates the pack directory with the default manifest and
/// answers the JSON string `"ok"`, from the query name or the body one;
/// an occupied name is the ticket's 400 `{"error":"Pack already exists"}`
/// (its 409, re-pinned), and one `emoji_pack_create` row is logged.
#[tokio::test]
async fn create_writes_the_default_manifest_once_per_pack() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packmaker", "admin:write").await;

    let created = common::TestRequest::post("/api/v1/pleroma/emoji/pack?name=fresh")
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&created, StatusCode::OK, "create by query name");
    assert_eq!(created.json::<String>(), "ok", "Pleroma's json(conn, ok)");
    assert_eq!(
        created.header("content-type"),
        Some("application/json"),
        "the answer is the JSON string, typed"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("fresh").join("pack.json")).unwrap(),
        r#"{"pack": {}, "files": {}}"#,
        "the default manifest lands on disk"
    );
    let shown = common::TestRequest::get("/api/v1/pleroma/emoji/pack?name=fresh")
        .send(&app)
        .await
        .json_value();
    assert_eq!(
        shown,
        json!({"pack": {"share-files": false}, "files": {}, "files_count": 0}),
        "the fresh pack reads back empty and unshared"
    );
    let twice = common::TestRequest::post("/api/v1/pleroma/emoji/pack?name=fresh")
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&twice, StatusCode::BAD_REQUEST, "create over itself");
    common::assert_error(&twice, "Pack already exists", "create over itself");

    let body_named = common::TestRequest::post("/api/v1/pleroma/emoji/pack")
        .bearer(&token)
        .json(&json!({"name": " from-body "}))
        .send(&app)
        .await;
    common::assert_answer(&body_named, StatusCode::OK, "create by body name");
    assert!(
        dir.path().join("from-body").join("pack.json").exists(),
        "the body name is trimmed and used"
    );

    assert_eq!(
        log_rows(&pool).await,
        vec![
            log_row("packmaker", "emoji_pack_create", "created", "fresh"),
            log_row("packmaker", "emoji_pack_create", "created", "from-body"),
        ],
        "one entry per success; the refused creates logged nothing"
    );
}

/// The create route's refusals: an absent or blank name is Pleroma's 400
/// "pack name cannot be empty", and a name that `pack_name_valid` rejects
/// — traversal, absolute, embedded slash — is a 400 that never reaches
/// the filesystem.
#[tokio::test]
async fn create_refuses_empty_and_unsafe_names_without_touching_disk() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("outside.txt"), b"do not create me").unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packscreener", "admin:write").await;

    for (query, body, message) in [
        ("", json!({}), "pack name cannot be empty"),
        ("?name=%20%20", json!({}), "pack name cannot be empty"),
        ("", json!({"name": "  "}), "pack name cannot be empty"),
        (
            "?name=%2e%2e%2fescape",
            json!({}),
            "invalid pack name: ../escape",
        ),
        (
            "?name=%2fetc%2fpasswd",
            json!({}),
            "invalid pack name: /etc/passwd",
        ),
        ("", json!({"name": "a/b"}), "invalid pack name: a/b"),
        ("", json!({"name": ".hidden"}), "invalid pack name: .hidden"),
    ] {
        let refused = common::TestRequest::post(&format!("/api/v1/pleroma/emoji/pack{query}"))
            .bearer(&token)
            .json(&body)
            .send(&app)
            .await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, "the refused create");
        common::assert_error(&refused, message, "the refused create");
    }

    let siblings: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        siblings,
        ["outside.txt"],
        "no refused name created or removed anything"
    );
    assert!(log_rows(&pool).await.is_empty(), "refusals log nothing");
}

/// `PATCH /pack` merges the body's `metadata` into the manifest's `pack`
/// block: present keys replace, absent keys keep what stands, a
/// form-encoded share-files reads as the boolean, the resulting metadata
/// object answers, and each write logs an `emoji_pack_update`.
#[tokio::test]
async fn patch_merges_metadata_and_answers_the_result() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(
        dir.path(),
        "cats",
        &Pack {
            pack: PackMeta {
                license: Some("CC0-1.0".to_owned()),
                description: Some("keep me".to_owned()),
                share_files: true,
                ..PackMeta::default()
            },
            files: BTreeMap::from([("cat".to_owned(), "cat.png".to_owned())]),
        },
    )
    .await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packeditor", "admin:write").await;

    let full = common::TestRequest::patch("/api/v1/pleroma/emoji/pack?name=cats")
        .bearer(&token)
        .json(&json!({
            "metadata": {
                "license": "MIT",
                "homepage": "https://emoji.example",
                "fallback-src": "https://emoji.example/cats.zip",
            }
        }))
        .send(&app)
        .await;
    common::assert_answer(&full, StatusCode::OK, "the license patch");
    assert_eq!(
        full.json_value(),
        json!({
            "license": "MIT",
            "homepage": "https://emoji.example",
            "description": "keep me",
            "fallback-src": "https://emoji.example/cats.zip",
            "share-files": true,
        }),
        "the merged object answers; the untouched keys kept their values"
    );

    // share-files travels as the form spelling and turns the pack
    // private; nothing else moves.
    let sharing = common::TestRequest::patch("/api/v1/pleroma/emoji/pack?name=cats")
        .bearer(&token)
        .form(&[("metadata[share-files]", "false")])
        .send(&app)
        .await;
    common::assert_answer(&sharing, StatusCode::OK, "the form patch");
    assert_eq!(
        sharing.json_value(),
        json!({
            "license": "MIT",
            "homepage": "https://emoji.example",
            "description": "keep me",
            "fallback-src": "https://emoji.example/cats.zip",
            "share-files": false,
        }),
        "the flex boolean merged, the rest stands"
    );

    assert_eq!(
        log_rows(&pool).await,
        vec![
            log_row("packeditor", "emoji_pack_update", "updated", "cats"),
            log_row("packeditor", "emoji_pack_update", "updated", "cats"),
        ],
        "one entry per successful patch"
    );
}

/// A key the other patches never sent — the fallback sha — merges into a
/// full metadata block on its own, stored exactly as given (the archive
/// is never fetched), under a name that only needed Pleroma's trimming;
/// the stored manifest keeps its files beside the merged block.
#[tokio::test]
async fn patch_merges_one_key_into_a_full_metadata_block() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(
        dir.path(),
        "cats",
        &Pack {
            pack: PackMeta {
                license: Some("MIT".to_owned()),
                homepage: Some("https://emoji.example".to_owned()),
                description: Some("keep me".to_owned()),
                fallback_src: Some("https://emoji.example/cats.zip".to_owned()),
                fallback_src_sha256: None,
                share_files: false,
            },
            files: BTreeMap::from([("cat".to_owned(), "cat.png".to_owned())]),
        },
    )
    .await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packfinisher", "admin:write").await;

    let sha = common::TestRequest::patch("/api/v1/pleroma/emoji/pack?name=%20cats%20")
        .bearer(&token)
        .json(&json!({"metadata": {"fallback-src-sha256": "beef"}}))
        .send(&app)
        .await;
    common::assert_answer(&sha, StatusCode::OK, "the sha patch, trimmed name");
    assert_eq!(
        sha.json_value(),
        json!({
            "license": "MIT",
            "homepage": "https://emoji.example",
            "description": "keep me",
            "fallback-src": "https://emoji.example/cats.zip",
            "fallback-src-sha256": "beef",
            "share-files": false,
        }),
        "the one key merged into the whole block, the url kept as stored"
    );

    let stored: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("cats").join("pack.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        stored["pack"]["fallback-src-sha256"],
        json!("beef"),
        "the sha is stored beside the url it describes"
    );
    assert_eq!(
        stored["files"]["cat"],
        json!("cat.png"),
        "a metadata write leaves the files alone"
    );

    assert_eq!(
        log_rows(&pool).await,
        vec![log_row(
            "packfinisher",
            "emoji_pack_update",
            "updated",
            "cats"
        )],
        "one entry for the one write"
    );
}

/// The patch route's refusals: an absent or blank name is the shared
/// empty-name 400, an unknown or unusable pack the Pleroma-worded 404
/// `{"error":"Pack <name> does not exist"}`, a body without `metadata` or
/// with a non-object one the extractor's 422, and a non-boolean
/// `share-files` likewise — nothing of a refused write lands.
#[tokio::test]
async fn patch_refuses_unknown_packs_and_bad_bodies() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &pack(true, &[("cat", "cat.png")])).await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packtester", "admin:write").await;
    let metadata = json!({"metadata": {"license": "GPL-3.0"}});

    for (query, body, status, message) in [
        (
            "",
            metadata.clone(),
            StatusCode::BAD_REQUEST,
            "pack name cannot be empty",
        ),
        (
            "?name=nope",
            metadata.clone(),
            StatusCode::NOT_FOUND,
            "Pack nope does not exist",
        ),
        (
            "?name=%2e%2e%2fsecret",
            metadata.clone(),
            StatusCode::NOT_FOUND,
            "Pack ../secret does not exist",
        ),
        (
            "?name=%2fetc%2fpasswd",
            metadata.clone(),
            StatusCode::NOT_FOUND,
            "Pack /etc/passwd does not exist",
        ),
        (
            "?name=cats",
            json!({}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid parameters",
        ),
        (
            "?name=cats",
            json!({"metadata": "not-an-object"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid parameters",
        ),
        (
            "?name=cats",
            json!({"metadata": {"share-files": "maybe"}}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid parameters",
        ),
    ] {
        let refused = common::TestRequest::patch(&format!("/api/v1/pleroma/emoji/pack{query}"))
            .bearer(&token)
            .json(&body)
            .send(&app)
            .await;
        common::assert_answer(&refused, status, "the refused patch");
        assert!(
            refused.json_value()["error"]
                .as_str()
                .is_some_and(|error| error.starts_with(message)),
            "the refusal says {message}: {}",
            refused.text()
        );
    }

    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("cats").join("pack.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        manifest["pack"],
        json!({"share-files": true}),
        "the untouched pack still holds only its seeded metadata"
    );
    assert!(log_rows(&pool).await.is_empty(), "refusals log nothing");
}

/// `DELETE /pack` removes the pack directory with its files, resyncs
/// `custom_emojis` so the pack's shortcodes leave
/// `GET /api/v1/custom_emojis`, and answers `"ok"`; an absent or unusable
/// pack is the 404 `{"error":"Pack <name> does not exist"}` — including
/// deleting the same pack twice.
#[tokio::test]
async fn delete_removes_the_pack_and_forgets_its_emojis() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &cats_pack()).await;
    seed_pack(dir.path(), "dogs", &pack(false, &[("dog", "dog.png")])).await;
    let synced: usize = emoji_packs::sync_custom_emojis(&pool, TEST_BASE_URL, dir.path())
        .await
        .expect("fixture packs sync");
    assert_eq!(synced, 3, "three pack files stored before the delete");
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packreaper", "admin:write").await;
    assert_eq!(
        served_emojis(&app).await,
        [
            ("cats".to_owned(), "cat".to_owned()),
            ("cats".to_owned(), "kitten".to_owned()),
            ("dogs".to_owned(), "dog".to_owned()),
        ],
        "every pack file is served before the delete"
    );

    let deleted = common::TestRequest::delete("/api/v1/pleroma/emoji/pack?name=cats")
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&deleted, StatusCode::OK, "delete");
    assert_eq!(deleted.json::<String>(), "ok");
    assert!(
        !dir.path().join("cats").exists(),
        "the whole directory — manifest and images — is gone"
    );
    assert!(dir.path().join("dogs").exists(), "the sibling pack stays");
    assert_eq!(
        served_emojis(&app).await,
        [("dogs".to_owned(), "dog".to_owned())],
        "the deleted pack's shortcodes left custom_emojis with it"
    );

    let again = common::TestRequest::delete("/api/v1/pleroma/emoji/pack?name=cats")
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&again, StatusCode::NOT_FOUND, "delete twice");
    common::assert_error(&again, "Pack cats does not exist", "delete twice");

    for (query, message) in [
        ("%2e%2e%2fsecret", "Pack ../secret does not exist"),
        ("%2fetc%2fpasswd", "Pack /etc/passwd does not exist"),
        (".hidden", "Pack .hidden does not exist"),
    ] {
        let refused =
            common::TestRequest::delete(&format!("/api/v1/pleroma/emoji/pack?name={query}"))
                .bearer(&token)
                .send(&app)
                .await;
        common::assert_answer(&refused, StatusCode::NOT_FOUND, "the escaping delete");
        common::assert_error(&refused, message, "the escaping delete");
    }
    let no_name = common::TestRequest::delete("/api/v1/pleroma/emoji/pack")
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&no_name, StatusCode::BAD_REQUEST, "delete without a name");
    common::assert_error(
        &no_name,
        "pack name cannot be empty",
        "delete without a name",
    );

    assert_eq!(
        log_rows(&pool).await,
        vec![log_row(
            "packreaper",
            "emoji_pack_delete",
            "deleted",
            "cats"
        )],
        "only the successful delete is logged"
    );
}

/// Any pack write re-runs the sync: a file that appeared in a pack's
/// manifest out of band is stored by the next write's resync, and
/// `GET /api/v1/custom_emojis` shows it without a restart.
#[tokio::test]
async fn every_write_resyncs_custom_emojis() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &pack(false, &[("cat", "cat.png")])).await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packsyncer", "admin:write").await;
    assert!(
        served_emojis(&app).await.is_empty(),
        "nothing synced before the first write"
    );
    // A second shortcode arrives in the manifest out of band (the file
    // upload routes are their own ticket); no sync has run since.
    seed_pack(
        dir.path(),
        "cats",
        &pack(false, &[("cat", "cat.png"), ("kitten", "sub/kitten.png")]),
    )
    .await;

    // The create of an unrelated pack resyncs everything: kitten lands.
    let created = common::TestRequest::post("/api/v1/pleroma/emoji/pack?name=dogs")
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&created, StatusCode::OK, "the resyncing create");
    assert_eq!(
        served_emojis(&app).await,
        [
            ("cats".to_owned(), "cat".to_owned()),
            ("cats".to_owned(), "kitten".to_owned()),
        ],
        "the new pack adds no emoji; the resync brought the cat pack"
    );
    // The patch resyncs too: drop kitten from the manifest on disk,
    // patch another pack, and the sweep forgets kitten.
    seed_pack(dir.path(), "cats", &pack(false, &[("cat", "cat.png")])).await;
    let patched = common::TestRequest::patch("/api/v1/pleroma/emoji/pack?name=dogs")
        .bearer(&token)
        .json(&json!({"metadata": {"license": "CC0-1.0"}}))
        .send(&app)
        .await;
    common::assert_answer(&patched, StatusCode::OK, "the resyncing patch");
    assert_eq!(
        served_emojis(&app).await,
        [("cats".to_owned(), "cat".to_owned())],
        "the sweep ran on the patch resync"
    );
}

/// The `verb` write against the gated pack route as `token` (`None` for
/// a request with no bearer header at all).
async fn gated_write(
    app: &axum::Router,
    verb: &str,
    token: Option<&str>,
    name: &str,
) -> common::TestResponse {
    let path = format!("/api/v1/pleroma/emoji/pack?name={name}");
    let request = match verb {
        "post" => common::TestRequest::post(&path),
        "patch" => common::TestRequest::patch(&path).json(&json!({"metadata": {}})),
        _ => common::TestRequest::delete(&path),
    };
    match token {
        Some(bearer) => request.bearer(bearer).send(app).await,
        None => request.send(app).await,
    }
}

/// One write-auth case: who the caller is, the bearer token to send
/// (`None` for no header at all), the status owed and its error wording.
type Refusal<'a> = (&'a str, Option<&'a str>, StatusCode, &'a str);

/// Every pack write refuses weaker credentials the way the shared admin
/// gate words them, on all three verbs: no bearer is the 401 "Invalid
/// credentials", an unknown token the 401 "The access token is invalid",
/// an app-only token the 403 that demands a user, a valid user who is not
/// an admin the 403 "User is not an admin.", and an admin whose token
/// lacks `admin:write` the 403 naming it. None of them touches the disk
/// or the log; the narrow `admin:write:emoji` alias grants every route,
/// like the root it stands under.
#[tokio::test]
async fn pack_writes_demand_an_admin_with_admin_write() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let app_only = common::create_app_only_token(&pool, "admin:write").await;
    let plain = plain_token(&pool, "packpleb", "admin:write").await;
    let rootless = admin_token(&pool, "packrootless", "read write").await;

    let refusals: &[Refusal<'_>] = &[
        (
            "no bearer token",
            None,
            StatusCode::UNAUTHORIZED,
            "Invalid credentials",
        ),
        (
            "a bogus token",
            Some("not-a-real-token"),
            StatusCode::UNAUTHORIZED,
            "The access token is invalid",
        ),
        (
            "an app-only token",
            Some(&app_only),
            StatusCode::FORBIDDEN,
            "This API requires an authenticated user",
        ),
        (
            "a non-admin user",
            Some(&plain),
            StatusCode::FORBIDDEN,
            "User is not an admin.",
        ),
        (
            "an admin without admin:write",
            Some(&rootless),
            StatusCode::FORBIDDEN,
            "Insufficient permissions: admin:write.",
        ),
    ];
    for verb in ["post", "patch", "delete"] {
        for (who, token, status, message) in refusals {
            let response = gated_write(&app, verb, *token, "gated").await;
            common::assert_answer(&response, *status, &format!("{verb} as {who}"));
            common::assert_error(&response, message, &format!("{verb} as {who}"));
        }
    }
    assert!(
        !dir.path().join("gated").exists(),
        "no refused write touched the filesystem"
    );
    assert!(log_rows(&pool).await.is_empty(), "refusals log nothing");

    let narrow = admin_token(&pool, "packnarrow", "admin:write:emoji").await;
    let created = gated_write(&app, "post", Some(&narrow), "gated").await;
    common::assert_answer(&created, StatusCode::OK, "the narrow alias creates");
    let patched = gated_write(&app, "patch", Some(&narrow), "gated").await;
    common::assert_answer(&patched, StatusCode::OK, "the narrow alias patches");
    let deleted = gated_write(&app, "delete", Some(&narrow), "gated").await;
    common::assert_answer(&deleted, StatusCode::OK, "the narrow alias deletes");
}

/// The add route's per-file byte cap, mirrored for the test body.
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

/// `multipart/form-data` parts: text `fields` plus, when `file` is given,
/// a `file` part uploaded under that name.
fn upload_parts(
    fields: &[(&'static str, &'static str)],
    file: Option<(&'static str, &'static [u8])>,
) -> Vec<common::MultipartPart> {
    let mut parts: Vec<common::MultipartPart> = fields
        .iter()
        .map(|(name, value)| common::MultipartPart::text(name, value))
        .collect();
    if let Some((filename, bytes)) = file {
        parts.push(common::MultipartPart::file(
            "file",
            filename,
            "image/png",
            Bytes::from_static(bytes),
        ));
    }
    parts
}

/// The sorted names inside pack `name`'s directory, for "nothing else
/// landed" assertions.
fn pack_entries(dir: &Path, name: &str) -> Vec<String> {
    let mut entries: Vec<String> = std::fs::read_dir(dir.join(name))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort_unstable();
    entries
}

/// The scripted answer of the emoji-file mock origin.
#[derive(Debug)]
struct MockFile {
    /// The body to serve.
    bytes: Vec<u8>,
    /// The status to serve it with.
    status: StatusCode,
}

/// Answers with the script's status and bytes.
async fn serve_mock(State(mock): State<Arc<MockFile>>) -> Response {
    Response::builder()
        .status(mock.status)
        .body(Body::from(mock.bytes.clone()))
        .unwrap()
}

/// Serves `bytes` at `/file` on loopback with `status`, returning the
/// URL and the server task: the mock origin the URL-file tests fetch
/// from, reachable because the test config turns the SSRF guard off for
/// private hosts exactly as `HTTP_ALLOW_PRIVATE_HOSTS` does.
async fn spawn_file_server(bytes: &[u8], status: StatusCode) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route("/file", get(serve_mock))
        .with_state(Arc::new(MockFile {
            bytes: bytes.to_vec(),
            status,
        }));
    let server = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("mock emoji-file server stopped: {err}");
        }
    });
    (format!("http://{addr}/file"), server)
}

/// `POST /packs/files` lands an uploaded image in the pack directory
/// under `<shortcode>.<ext>` — the part's own client name ignored —
/// records the entry in `pack.json`, resyncs the custom emojis and
/// answers the full `files` object; an explicit `filename` (even a
/// nested one, and one whose extension differs from the magic bytes)
/// stands, and the new file serves from the static emoji route.
#[tokio::test]
async fn upload_adds_the_file_to_the_pack() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &pack(true, &[("cat", "cat.png")])).await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "filemaker", "admin:write").await;

    let added = common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .multipart(
            BOUNDARY,
            &upload_parts(
                &[("shortcode", "dog")],
                Some(("whatever name.png", PNG_BYTES)),
            ),
        )
        .send(&app)
        .await;
    common::assert_answer(&added, StatusCode::OK, "upload add");
    assert_eq!(
        added.json_value(),
        json!({"cat": "cat.png", "dog": "dog.png"}),
        "the full files object; the default name is shortcode plus extension"
    );
    assert_eq!(
        std::fs::read(dir.path().join("cats").join("dog.png")).unwrap(),
        PNG_BYTES,
        "the bytes landed in the pack directory"
    );

    let nested = common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .multipart(
            BOUNDARY,
            &[
                common::MultipartPart::text("shortcode", "kitten"),
                common::MultipartPart::text("filename", "sub/kitten.gif"),
                common::MultipartPart::file(
                    "file",
                    "kitten.png",
                    "image/png",
                    Bytes::from_static(GIF_BYTES),
                ),
            ],
        )
        .send(&app)
        .await;
    common::assert_answer(&nested, StatusCode::OK, "named nested upload");
    assert_eq!(
        nested.json_value(),
        json!({"cat": "cat.png", "dog": "dog.png", "kitten": "sub/kitten.gif"}),
        "the requested file name stands"
    );
    assert_eq!(
        std::fs::read(dir.path().join("cats").join("sub/kitten.gif")).unwrap(),
        GIF_BYTES,
        "the directories a nested name needs are created for it"
    );

    let served = common::TestRequest::get("/emoji/cats/dog.png")
        .send(&app)
        .await;
    common::assert_answer(&served, StatusCode::OK, "the new file serves");
    assert_eq!(served.body.as_ref(), PNG_BYTES, "byte for byte");

    assert_eq!(
        served_emojis(&app).await,
        [
            ("cats".to_owned(), "cat".to_owned()),
            ("cats".to_owned(), "dog".to_owned()),
            ("cats".to_owned(), "kitten".to_owned()),
        ],
        "the custom emoji list picked the new files up on the resync"
    );
}

/// A `file` parameter that is a URL is fetched with the instance's
/// guarded client and stored under the magic bytes' default name; a
/// remote that answers non-image bytes or a 404 is refused with a 422,
/// and nothing of a refused fetch lands.
#[tokio::test]
async fn a_url_file_is_fetched_and_stored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &pack(true, &[("cat", "cat.png")])).await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "filefetcher", "admin:write").await;

    let (url, server) = spawn_file_server(GIF_BYTES, StatusCode::OK).await;
    let added = common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .json(&json!({"shortcode": "fox", "file": url}))
        .send(&app)
        .await;
    server.abort();
    common::assert_answer(&added, StatusCode::OK, "url add");
    assert_eq!(
        added.json_value(),
        json!({"cat": "cat.png", "fox": "fox.gif"}),
        "the default extension comes from the magic bytes, not the URL"
    );
    assert_eq!(
        std::fs::read(dir.path().join("cats").join("fox.gif")).unwrap(),
        GIF_BYTES,
        "the fetched bytes landed"
    );

    // The urlencoded spelling, with an explicit filename, and a body
    // that also carries the pack name (the query has none here).
    let (named_url, named_server) = spawn_file_server(PNG_BYTES, StatusCode::OK).await;
    let renamed = common::TestRequest::post("/api/v1/pleroma/emoji/packs/files")
        .bearer(&token)
        .form(&[
            ("name", "cats"),
            ("shortcode", "lynx"),
            ("filename", "lynx.png"),
            ("file", named_url.as_str()),
        ])
        .send(&app)
        .await;
    named_server.abort();
    common::assert_answer(&renamed, StatusCode::OK, "form url add");
    assert_eq!(
        renamed.json_value()["lynx"],
        json!("lynx.png"),
        "the requested name stands for the fetched bytes"
    );

    let (junk_url, junk_server) = spawn_file_server(NOT_AN_IMAGE, StatusCode::OK).await;
    let junk = common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .json(&json!({"shortcode": "junk", "file": junk_url}))
        .send(&app)
        .await;
    junk_server.abort();
    common::assert_answer(&junk, StatusCode::UNPROCESSABLE_ENTITY, "non-image URL");
    common::assert_error(&junk, "Unsupported file type", "non-image URL");

    let (missing_url, missing_server) = spawn_file_server(b"", StatusCode::NOT_FOUND).await;
    let gone = common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .json(&json!({"shortcode": "gone", "file": missing_url}))
        .send(&app)
        .await;
    missing_server.abort();
    common::assert_answer(&gone, StatusCode::UNPROCESSABLE_ENTITY, "404 URL");
    assert!(
        gone.json_value()["error"]
            .as_str()
            .is_some_and(|error| error.contains("404")),
        "the refusal names the remote status: {}",
        gone.text()
    );

    assert_eq!(
        pack_entries(dir.path(), "cats"),
        ["cat.png", "fox.gif", "lynx.png", "pack.json"],
        "the refused fetches stored nothing"
    );
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("cats").join("pack.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        manifest["files"],
        json!({"cat": "cat.png", "fox": "fox.gif", "lynx": "lynx.png"}),
        "and recorded no entries"
    );
}

/// A duplicate shortcode is the 409 `{"error":"An emoji with the
/// \"<shortcode>\" shortcode already exists"}`, and none of the add
/// route's refusals — this one or the byte- and name-level ones tested
/// next — ever lands a file, an entry, an emoji row or a log row.
#[tokio::test]
async fn add_refuses_the_duplicate_shortcode() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &pack(true, &[("cat", "cat.png")])).await;
    let synced: usize = emoji_packs::sync_custom_emojis(&pool, TEST_BASE_URL, dir.path())
        .await
        .expect("the fixture pack syncs");
    assert_eq!(synced, 1, "the seeded cat is the one stored emoji");
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "filetester", "admin:write").await;

    let duplicate = common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .multipart(
            BOUNDARY,
            &upload_parts(&[("shortcode", "cat")], Some(("cat.png", PNG_BYTES))),
        )
        .send(&app)
        .await;
    common::assert_answer(&duplicate, StatusCode::CONFLICT, "duplicate shortcode");
    common::assert_error(
        &duplicate,
        "An emoji with the \"cat\" shortcode already exists",
        "duplicate shortcode",
    );
    assert_eq!(
        pack_entries(dir.path(), "cats"),
        ["cat.png", "pack.json"],
        "the refused duplicate landed no file"
    );
    assert_eq!(
        served_emojis(&app).await,
        [("cats".to_owned(), "cat".to_owned())],
        "the emoji list never saw a refused add"
    );
    assert!(log_rows(&pool).await.is_empty(), "file writes log nothing");
}

/// The add route's file refusals: non-image bytes are the 422
/// `{"error":"Unsupported file type"}`, a body without any `file` the
/// 422 `{"error":"Missing parameter: file"}`, and an upload past 2 MiB
/// the 413 that names the cap.
#[tokio::test]
async fn add_refuses_bad_files() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &pack(true, &[("cat", "cat.png")])).await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "filescreener", "admin:write").await;

    let junk = common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .multipart(
            BOUNDARY,
            &upload_parts(&[("shortcode", "junk")], Some(("junk.png", NOT_AN_IMAGE))),
        )
        .send(&app)
        .await;
    common::assert_answer(&junk, StatusCode::UNPROCESSABLE_ENTITY, "non-image upload");
    common::assert_error(&junk, "Unsupported file type", "non-image upload");

    let no_file = common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .json(&json!({"shortcode": "nothing"}))
        .send(&app)
        .await;
    common::assert_answer(
        &no_file,
        StatusCode::UNPROCESSABLE_ENTITY,
        "body without a file",
    );
    common::assert_error(&no_file, "Missing parameter: file", "body without a file");

    let mut oversize = PNG_BYTES.to_vec();
    oversize.resize(MAX_FILE_BYTES + 1, b'0');
    let big_file =
        common::MultipartPart::file("file", "big.png", "image/png", Bytes::from(oversize));
    let big = common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .multipart(
            BOUNDARY,
            &[common::MultipartPart::text("shortcode", "big"), big_file],
        )
        .send(&app)
        .await;
    common::assert_answer(
        &big,
        StatusCode::PAYLOAD_TOO_LARGE,
        "a 2 MiB plus one upload",
    );
    assert!(
        big.json_value()["error"]
            .as_str()
            .is_some_and(|error| error.contains("2097152")),
        "the 413 names the cap: {}",
        big.text()
    );
    assert_eq!(
        pack_entries(dir.path(), "cats"),
        ["cat.png", "pack.json"],
        "nothing of a refused file landed in the pack"
    );
}

/// The add route's name refusals: a shortcode outside
/// `[A-Za-z0-9_-]+` and a file name that could traverse out of the pack
/// are 400s, a body without `shortcode` is the extractor's 422 — and no
/// refused name writes, moves or touches anything.
#[tokio::test]
async fn add_refuses_unsafe_names() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &pack(true, &[("cat", "cat.png")])).await;
    std::fs::write(dir.path().join("outside.txt"), b"do not write me").unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "filenamer", "admin:write").await;

    for code in ["bad code", "a.b", "a/b", "", ".hidden"] {
        let refused = common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=cats")
            .bearer(&token)
            .json(&json!({"shortcode": code, "file": "https://unreachable.invalid/x.png"}))
            .send(&app)
            .await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, "the refused shortcode");
        common::assert_error(
            &refused,
            &format!("invalid emoji shortcode: {code}"),
            "the refused shortcode",
        );
    }

    let escaping = common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .multipart(
            BOUNDARY,
            &[
                common::MultipartPart::text("shortcode", "sneaky"),
                common::MultipartPart::text("filename", "../escape.png"),
                common::MultipartPart::file(
                    "file",
                    "sneaky.png",
                    "image/png",
                    Bytes::from_static(PNG_BYTES),
                ),
            ],
        )
        .send(&app)
        .await;
    common::assert_answer(&escaping, StatusCode::BAD_REQUEST, "the traversal filename");
    common::assert_error(
        &escaping,
        "invalid emoji file name: ../escape.png",
        "the traversal filename",
    );

    let no_shortcode = common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .json(&json!({"file": "https://unreachable.invalid/x.png"}))
        .send(&app)
        .await;
    common::assert_answer(
        &no_shortcode,
        StatusCode::UNPROCESSABLE_ENTITY,
        "no shortcode",
    );
    assert!(
        no_shortcode.json_value()["error"]
            .as_str()
            .is_some_and(|error| error.starts_with("Invalid parameters")),
        "the 422 names the shape: {}",
        no_shortcode.text()
    );

    assert_eq!(
        pack_entries(dir.path(), "cats"),
        ["cat.png", "pack.json"],
        "nothing of a refused name landed in the pack"
    );
    assert_eq!(
        std::fs::read(dir.path().join("outside.txt")).unwrap(),
        b"do not write me",
        "and the file outside the pack is untouched"
    );
}

/// `PATCH /packs/files` renames the shortcode, the file, or both: the
/// file on disk follows the entry (emptying subdirectories are pruned),
/// parameters ride the query, the body or both, and every success
/// answers the pack's full `files` object and moves the custom emoji
/// with it.
#[tokio::test]
async fn rename_moves_the_shortcode_and_the_file() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(
        dir.path(),
        "cats",
        &pack(true, &[("cat", "sub/cat.png"), ("dog", "dog.png")]),
    )
    .await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "filerenamer", "admin:write").await;

    let both = common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .json(&json!({
            "shortcode": "cat",
            "new_shortcode": "kitty",
            "new_filename": "kitty.png",
        }))
        .send(&app)
        .await;
    common::assert_answer(&both, StatusCode::OK, "rename both");
    assert_eq!(
        both.json_value(),
        json!({"dog": "dog.png", "kitty": "kitty.png"}),
        "the full files object after the rename"
    );
    assert_eq!(
        std::fs::read(dir.path().join("cats").join("kitty.png")).unwrap(),
        PNG_BYTES,
        "the file moved with the entry"
    );
    assert!(
        !dir.path().join("cats").join("sub").exists(),
        "the emptied subdirectory is pruned"
    );
    assert_eq!(
        served_emojis(&app).await,
        [
            ("cats".to_owned(), "dog".to_owned()),
            ("cats".to_owned(), "kitty".to_owned()),
        ],
        "the custom emoji list followed the rename"
    );

    // Shortcode only: the file stays where it is.
    let code = common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .form(&[("shortcode", "kitty"), ("new_shortcode", "cat")])
        .send(&app)
        .await;
    common::assert_answer(&code, StatusCode::OK, "rename the shortcode alone");
    assert_eq!(
        code.json_value(),
        json!({"cat": "kitty.png", "dog": "dog.png"}),
        "the stored file name kept"
    );

    // File only, and everything from the query string, no body at all.
    let file = common::TestRequest::patch(
        "/api/v1/pleroma/emoji/packs/files?name=cats&shortcode=cat&new_filename=c.png",
    )
    .bearer(&token)
    .send(&app)
    .await;
    common::assert_answer(&file, StatusCode::OK, "rename the file from the query");
    assert_eq!(
        file.json_value(),
        json!({"cat": "c.png", "dog": "dog.png"}),
        "query-only parameters work"
    );
    assert!(
        dir.path().join("cats").join("c.png").exists(),
        "and the file moved again"
    );
}

/// A rename into an occupied new shortcode is the 409
/// `{"error":"New shortcode \"<new>\" is already used. If you want to
/// override emoji use 'force' option"}` until `force` overwrites the
/// other entry — the custom emoji list follows the map.
#[tokio::test]
async fn rename_conflicts_unless_forced() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(
        dir.path(),
        "cats",
        &pack(true, &[("cat", "c.png"), ("dog", "dog.png")]),
    )
    .await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "fileclasher", "admin:write").await;

    let clash = common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .json(&json!({"shortcode": "cat", "new_shortcode": "dog"}))
        .send(&app)
        .await;
    common::assert_answer(&clash, StatusCode::CONFLICT, "the occupied new shortcode");
    common::assert_error(
        &clash,
        "New shortcode \"dog\" is already used. If you want to override emoji use 'force' option",
        "the occupied new shortcode",
    );
    assert_eq!(
        pack_entries(dir.path(), "cats"),
        ["c.png", "dog.png", "pack.json"],
        "the refused rename moved nothing"
    );

    let forced = common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .json(&json!({
            "shortcode": "cat",
            "new_shortcode": "dog",
            "new_filename": "c.png",
            "force": true,
        }))
        .send(&app)
        .await;
    common::assert_answer(&forced, StatusCode::OK, "the forced rename");
    assert_eq!(
        forced.json_value(),
        json!({"dog": "c.png"}),
        "force replaces the other entry with this one"
    );
    assert_eq!(
        served_emojis(&app).await,
        [("cats".to_owned(), "dog".to_owned())],
        "the overwritten shortcode left the emoji list"
    );
}

/// One refused rename: the pack, the body, the owed status and the
/// pinned error wording.
type RenameCase<'a> = (&'a str, Value, StatusCode, &'a str);

/// The rename's refusals: an unknown shortcode and an entry whose file
/// vanished off disk are the 400 `{"error":"Emoji \"<shortcode>\" does
/// not exist"}`, an empty `new_shortcode` or `new_filename` the 422, a
/// new shortcode that breaks the pattern and a new file name that could
/// traverse the 400s — and none of them moves anything.
#[tokio::test]
async fn rename_refuses_unknown_empty_and_unsafe() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &pack(true, &[("cat", "cat.png")])).await;
    emoji_packs::save_pack(dir.path(), "haunt", &pack(true, &[("ghost", "ghost.png")]))
        .await
        .expect("the manifest-only pack saves");
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "filescreener", "admin:write").await;

    let cases: [RenameCase<'_>; 6] = [
        (
            "haunt",
            json!({"shortcode": "ghost", "new_filename": "here.png"}),
            StatusCode::BAD_REQUEST,
            "Emoji \"ghost\" does not exist",
        ),
        (
            "cats",
            json!({"shortcode": "nope", "new_shortcode": "x"}),
            StatusCode::BAD_REQUEST,
            "Emoji \"nope\" does not exist",
        ),
        (
            "cats",
            json!({"shortcode": "cat", "new_shortcode": ""}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "new_shortcode or new_filename cannot be empty",
        ),
        (
            "cats",
            json!({"shortcode": "cat", "new_filename": "", "new_shortcode": "ok"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "new_shortcode or new_filename cannot be empty",
        ),
        (
            "cats",
            json!({"shortcode": "cat", "new_shortcode": "bad code"}),
            StatusCode::BAD_REQUEST,
            "invalid emoji shortcode: bad code",
        ),
        (
            "cats",
            json!({"shortcode": "cat", "new_filename": "../escape.png"}),
            StatusCode::BAD_REQUEST,
            "invalid emoji file name: ../escape.png",
        ),
    ];
    for (pack_name, body, status, message) in cases {
        let refused = common::TestRequest::patch(&format!(
            "/api/v1/pleroma/emoji/packs/files?name={pack_name}"
        ))
        .bearer(&token)
        .json(&body)
        .send(&app)
        .await;
        common::assert_answer(&refused, status, "the refused rename");
        common::assert_error(&refused, message, "the refused rename");
    }

    assert_eq!(
        pack_entries(dir.path(), "cats"),
        ["cat.png", "pack.json"],
        "no refused rename touched a file"
    );
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("cats").join("pack.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        manifest["files"],
        json!({"cat": "cat.png"}),
        "and none touched the manifest"
    );
}

/// `DELETE /packs/files` drops the entry from `pack.json` and the file
/// from the pack directory — pruning the subdirectory a nested name
/// emptied — accepts the `shortcode` (and the `name`) from the query or
/// the body, answers the pack's full `files` object, and the resync
/// forgets the custom emoji with it.
#[tokio::test]
async fn delete_removes_the_entry_and_the_file() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(
        dir.path(),
        "cats",
        &pack(
            true,
            &[
                ("cat", "cat.png"),
                ("kitten", "sub/kitten.png"),
                ("lone", "lone.gif"),
            ],
        ),
    )
    .await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "filereaper", "admin:write").await;

    let via_query =
        common::TestRequest::delete("/api/v1/pleroma/emoji/packs/files?name=cats&shortcode=cat")
            .bearer(&token)
            .send(&app)
            .await;
    common::assert_answer(&via_query, StatusCode::OK, "delete from the query");
    assert_eq!(
        via_query.json_value(),
        json!({"kitten": "sub/kitten.png", "lone": "lone.gif"}),
        "the entry is gone from the files object"
    );
    assert!(
        !dir.path().join("cats").join("cat.png").exists(),
        "and its file with it"
    );

    let via_body = common::TestRequest::delete("/api/v1/pleroma/emoji/packs/files?name=cats")
        .bearer(&token)
        .form(&[("shortcode", "kitten")])
        .send(&app)
        .await;
    common::assert_answer(&via_body, StatusCode::OK, "delete from the body");
    assert_eq!(
        via_body.json_value(),
        json!({"lone": "lone.gif"}),
        "the nested entry left"
    );
    assert!(
        !dir.path().join("cats").join("sub").exists(),
        "the emptied subdirectory is pruned"
    );

    let last = common::TestRequest::delete("/api/v1/pleroma/emoji/packs/files")
        .bearer(&token)
        .json(&json!({"name": "cats", "shortcode": "lone"}))
        .send(&app)
        .await;
    common::assert_answer(&last, StatusCode::OK, "delete with the name in the body");
    assert_eq!(last.json_value(), json!({}), "the pack is empty now");
    assert_eq!(
        pack_entries(dir.path(), "cats"),
        ["pack.json"],
        "every file left with its entry"
    );
    assert!(
        served_emojis(&app).await.is_empty(),
        "and the custom emoji list followed the deletes"
    );
}

/// A delete of a shortcode the pack does not name is the 400
/// `{"error":"Emoji \"<shortcode>\" does not exist"}` — the same refusal
/// twice-deleted entries and never-there ones — while an entry whose
/// file already vanished off disk is still deletable: the manifest is
/// what must stop naming it.
#[tokio::test]
async fn delete_refuses_unknown_shortcodes_and_tolerates_off_disk_entries() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    emoji_packs::save_pack(dir.path(), "haunt", &pack(true, &[("ghost", "ghost.png")]))
        .await
        .expect("the manifest-only pack saves");
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "fileghoster", "admin:write").await;

    let ghost = common::TestRequest::delete("/api/v1/pleroma/emoji/packs/files?name=haunt")
        .bearer(&token)
        .json(&json!({"shortcode": "ghost"}))
        .send(&app)
        .await;
    common::assert_answer(&ghost, StatusCode::OK, "delete an off-disk entry");
    assert_eq!(ghost.json_value(), json!({}), "the entry left the manifest");

    for shortcode in ["ghost", "never-was"] {
        let again = common::TestRequest::delete(&format!(
            "/api/v1/pleroma/emoji/packs/files?name=haunt&shortcode={shortcode}"
        ))
        .bearer(&token)
        .send(&app)
        .await;
        common::assert_answer(&again, StatusCode::BAD_REQUEST, "delete twice");
        common::assert_error(
            &again,
            &format!("Emoji \"{shortcode}\" does not exist"),
            "delete twice",
        );
    }
}

/// All three file routes answer an unknown pack with the ticket's 400
/// `{"error":"Pack <name> does not exist"}`, fold a name that could
/// traverse into the same refusal (the routes cannot be used to probe
/// the filesystem), and demand the `name`: absent or blank is the
/// shared empty-value 400.
#[tokio::test]
async fn file_routes_refuse_unknown_and_unnamed_packs() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("outside.txt"), b"do not touch me").unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "fileguesser", "admin:write").await;

    for (request, message) in [
        (
            common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=nope")
                .json(&json!({"shortcode": "cat", "file": "https://unreachable.invalid/c.png"})),
            "Pack nope does not exist",
        ),
        (
            common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files?name=nope")
                .json(&json!({"shortcode": "cat", "new_shortcode": "dog"})),
            "Pack nope does not exist",
        ),
        (
            common::TestRequest::delete(
                "/api/v1/pleroma/emoji/packs/files?name=nope&shortcode=cat",
            ),
            "Pack nope does not exist",
        ),
        (
            common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=%2e%2e%2fsecret")
                .json(&json!({"shortcode": "cat", "file": "https://unreachable.invalid/c.png"})),
            "Pack ../secret does not exist",
        ),
        (
            common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files?name=%2fetc%2fpasswd")
                .json(&json!({"shortcode": "cat"})),
            "Pack /etc/passwd does not exist",
        ),
        (
            common::TestRequest::delete("/api/v1/pleroma/emoji/packs/files?name=&shortcode=cat"),
            "pack name cannot be empty",
        ),
        (
            common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files")
                .json(&json!({"shortcode": "cat"})),
            "pack name cannot be empty",
        ),
    ] {
        let refused = request.bearer(&token).send(&app).await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, "the refused file write");
        common::assert_error(&refused, message, "the refused file write");
    }

    let siblings: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        siblings,
        ["outside.txt"],
        "no refused file write created or touched anything"
    );
    assert!(log_rows(&pool).await.is_empty(), "refusals log nothing");
}

/// One file-route `verb` against `/packs/files?name=gated` as `token`
/// (`None` for no bearer header), always with a body that would pass if
/// the caller were allowed: the gate refuses before anything else reads
/// it, and only the scope refusal needs the parseable body — the
/// extractor's role gate runs first.
async fn gated_file_write(
    app: &axum::Router,
    verb: &str,
    token: Option<&str>,
) -> common::TestResponse {
    let request = match verb {
        "post" => common::TestRequest::post("/api/v1/pleroma/emoji/packs/files?name=gated")
            .multipart(
                BOUNDARY,
                &upload_parts(&[("shortcode", "gated")], Some(("g.png", PNG_BYTES))),
            ),
        "patch" => common::TestRequest::patch("/api/v1/pleroma/emoji/packs/files?name=gated")
            .json(&json!({"shortcode": "gated", "new_shortcode": "renamed"})),
        _ => common::TestRequest::delete(
            "/api/v1/pleroma/emoji/packs/files?name=gated&shortcode=renamed",
        ),
    };
    match token {
        Some(bearer) => request.bearer(bearer).send(app).await,
        None => request.send(app).await,
    }
}

/// Every file write refuses weaker credentials the way the shared admin
/// gate words them, on all three verbs, and none of them touches the
/// pack on disk or the moderation log; the narrow `admin:write:emoji`
/// alias grants all three.
#[tokio::test]
async fn file_writes_demand_an_admin_with_admin_write() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "gated", &pack(false, &[])).await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let app_only = common::create_app_only_token(&pool, "admin:write").await;
    let plain = plain_token(&pool, "filepleb", "admin:write").await;
    let rootless = admin_token(&pool, "filerootless", "read write").await;

    let refusals: &[Refusal<'_>] = &[
        (
            "no bearer token",
            None,
            StatusCode::UNAUTHORIZED,
            "Invalid credentials",
        ),
        (
            "a bogus token",
            Some("not-a-real-token"),
            StatusCode::UNAUTHORIZED,
            "The access token is invalid",
        ),
        (
            "an app-only token",
            Some(&app_only),
            StatusCode::FORBIDDEN,
            "This API requires an authenticated user",
        ),
        (
            "a non-admin user",
            Some(&plain),
            StatusCode::FORBIDDEN,
            "User is not an admin.",
        ),
        (
            "an admin without admin:write",
            Some(&rootless),
            StatusCode::FORBIDDEN,
            "Insufficient permissions: admin:write.",
        ),
    ];
    for verb in ["post", "patch", "delete"] {
        for (who, token, status, message) in refusals {
            let response = gated_file_write(&app, verb, *token).await;
            common::assert_answer(&response, *status, &format!("{verb} as {who}"));
            common::assert_error(&response, message, &format!("{verb} as {who}"));
        }
    }
    assert_eq!(
        pack_entries(dir.path(), "gated"),
        ["pack.json"],
        "no refused file write touched the pack"
    );
    assert!(log_rows(&pool).await.is_empty(), "refusals log nothing");

    let narrow = admin_token(&pool, "filenarrow", "admin:write:emoji").await;
    let added = gated_file_write(&app, "post", Some(&narrow)).await;
    common::assert_answer(&added, StatusCode::OK, "the narrow alias adds");
    assert_eq!(added.json_value(), json!({"gated": "gated.png"}));
    let renamed = gated_file_write(&app, "patch", Some(&narrow)).await;
    common::assert_answer(&renamed, StatusCode::OK, "the narrow alias renames");
    assert_eq!(renamed.json_value(), json!({"renamed": "gated.png"}));
    let removed = gated_file_write(&app, "delete", Some(&narrow)).await;
    common::assert_answer(&removed, StatusCode::OK, "the narrow alias deletes");
    assert_eq!(removed.json_value(), json!({}));
}

// --- WBE-216: the remote relay, the pack download, the zip upload and
// --- the filesystem import.

/// Magic bytes the emoji-file detection names as `image/webp`.
const WEBP_BYTES: &[u8] = b"RIFF\x00\x00\x00\x00WEBPVP8 ";
/// The pinned 500 body of the remote-fetch routes.
const COULD_NOT_FETCH: &str = "Could not fetch the pack list";
/// The pinned 400 body of an unshared, fallback-less remote pack.
const NOT_SHAREABLE: &str =
    "The pack was not set as shared and there is no fallback src to download from";
/// The pinned 400 of the traversal refusal.
const UNSAFE_PATH: &str = "Pack archive contains an unsafe path";
/// The pinned 400 of the unreadable-archive refusal.
const UNREADABLE: &str = "Could not read the pack archive";

/// The manifest the fixture archives ship.
const ARCHIVE_MANIFEST: &str = concat!(
    r#"{"pack": {"license": "MIT", "share-files": true}, "#,
    r#""files": {"cat": "cat.png", "kitten": "sub/kitten.gif", "ghost": "gone.png"}}"#,
);

/// The bytes of a zip holding `entries` (name, payload).
fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, payload) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(payload).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// The shared pack's archive: two images (one nested), the manifest, and
/// a text and a hidden file the safe extraction must skip.
fn good_archive() -> Vec<u8> {
    build_zip(&[
        ("pack.json", ARCHIVE_MANIFEST.as_bytes()),
        ("cat.png", PNG_BYTES),
        ("sub/kitten.gif", GIF_BYTES),
        ("notes.txt", b"prose, not an emoji".as_slice()),
        (".hidden.png", PNG_BYTES),
    ])
}

/// The scripted answer of a mock remote instance.
#[derive(Debug)]
struct MockRemote {
    /// The body of `GET /api/v1/pleroma/emoji/packs` (the relay's source).
    packs: Value,
    /// The body of `GET /api/v1/pleroma/emoji/pack` for the first
    /// (`page_size=0`) read.
    pack_first: Value,
    /// The body of the same route when the caller re-reads with a real
    /// `page_size` (Pleroma's `files_count` second pass); `None` serves
    /// the first body again.
    pack_full: Option<Value>,
    /// The bytes of `GET /api/v1/pleroma/emoji/packs/archive`.
    archive: Vec<u8>,
    /// The status both listing routes answer with.
    list_status: StatusCode,
    /// The status the archive route answers with.
    archive_status: StatusCode,
    /// Whether the packs route serves non-JSON junk.
    packs_as_junk: bool,
    /// The queries the one-pack listing route saw, in order.
    seen_pack: Mutex<Vec<String>>,
    /// How often the archive route was called.
    archive_hits: AtomicUsize,
}

impl Default for MockRemote {
    fn default() -> Self {
        Self {
            packs: json!({"count": 0, "packs": {}}),
            pack_first: json!({
                "pack": {"share-files": true},
                "files": {"cat": "cat.png", "kitten": "sub/kitten.gif"},
                "files_count": 2,
            }),
            pack_full: None,
            archive: good_archive(),
            list_status: StatusCode::OK,
            archive_status: StatusCode::OK,
            packs_as_junk: false,
            seen_pack: Mutex::new(Vec::new()),
            archive_hits: AtomicUsize::new(0),
        }
    }
}

/// A JSON response of the mock remote.
fn mock_json(status: StatusCode, body: &Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Serves the all-packs listing, echoing the query it was asked with,
/// so the relay test proves exactly what this instance forwarded.
async fn serve_packs_list(State(remote): State<Arc<MockRemote>>, uri: Uri) -> Response {
    if remote.packs_as_junk {
        return Response::builder()
            .status(remote.list_status)
            .body(Body::from("not json at all"))
            .unwrap();
    }
    let mut body = remote.packs.clone();
    if let Some(fields) = body.as_object_mut() {
        let _seen: Option<Value> = fields.insert(
            "seen_query".to_owned(),
            json!(uri.query().unwrap_or_default()),
        );
    }
    mock_json(remote.list_status, &body)
}

/// Serves one pack's listing, recording the query and answering the
/// full body once the caller asks beyond the first `page_size=0` read.
async fn serve_pack_show(State(remote): State<Arc<MockRemote>>, uri: Uri) -> Response {
    let query = uri.query().unwrap_or_default().to_owned();
    remote.seen_pack.lock().await.push(query.clone());
    let body = match (&remote.pack_full, query.contains("page_size=0")) {
        (Some(full), false) => full,
        _ => &remote.pack_first,
    };
    mock_json(remote.list_status, body)
}

/// Serves the pack archive and counts the hit.
async fn serve_pack_archive(State(remote): State<Arc<MockRemote>>) -> Response {
    let _hits = remote.archive_hits.fetch_add(1, Ordering::SeqCst);
    Response::builder()
        .status(remote.archive_status)
        .header(header::CONTENT_TYPE, "application/zip")
        .body(Body::from(remote.archive.clone()))
        .unwrap()
}

/// Serves a mock remote instance on loopback (reachable because the test
/// config allows private hosts exactly as `HTTP_ALLOW_PRIVATE_HOSTS`
/// does), built from its own base URL: returns that URL, the shared
/// script for assertions, and the server task.
async fn spawn_pack_server<F>(build: F) -> (String, Arc<MockRemote>, JoinHandle<()>)
where
    F: FnOnce(String) -> MockRemote,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(build(base.clone()));
    let app = axum::Router::new()
        .route("/api/v1/pleroma/emoji/packs", get(serve_packs_list))
        .route("/api/v1/pleroma/emoji/pack", get(serve_pack_show))
        .route(
            "/api/v1/pleroma/emoji/packs/archive",
            get(serve_pack_archive),
        )
        .with_state(Arc::clone(&state));
    let server = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("mock emoji-pack remote stopped: {err}");
        }
    });
    (base, state, server)
}

/// The manifest of the pack installed under `name`, as stored on disk.
async fn installed_manifest(dir: &Path, name: &str) -> Pack {
    emoji_packs::load_pack(dir, name)
        .await
        .unwrap_or_else(|err| panic!("the installed pack parses: {err}"))
}

/// The `(shortcode, file)` pairs of a pack manifest, ordered.
fn manifest_files(pack: &Pack) -> Vec<(&str, &str)> {
    pack.files
        .iter()
        .map(|(code, file)| (code.as_str(), file.as_str()))
        .collect()
}

/// `GET /packs/remote` reads `{url}/api/v1/pleroma/emoji/packs` through
/// the guarded client and relays it verbatim, forwarding the requested
/// `page` and this module's capped `page_size`.
#[tokio::test]
async fn remote_relays_the_packs_listing_of_an_instance() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "remotereader", "admin:read").await;

    let listing = json!({
        "count": 1,
        "packs": {
            "cats": {"pack": {"share-files": true}, "files": {"cat": "cat.png"}, "files_count": 1},
        },
    });
    let (base, _script, server) = spawn_pack_server(|_| MockRemote {
        packs: listing.clone(),
        ..MockRemote::default()
    })
    .await;

    let response = common::TestRequest::get(&format!(
        "/api/v1/pleroma/emoji/packs/remote?url={base}&page=2&page_size=500"
    ))
    .bearer(&token)
    .send(&app)
    .await;
    server.abort();
    common::assert_answer(&response, StatusCode::OK, "remote relay");
    let mut expected = listing;
    let _seen: Option<Value> = expected
        .as_object_mut()
        .unwrap()
        .insert("seen_query".to_owned(), json!("page=2&page_size=100"));
    assert_eq!(
        response.json_value(),
        expected,
        "the remote body as it answered, with the forwarded page and the clamped size"
    );
}

/// The relay's refusals: an absent or unusable `url` is the 400, and a
/// remote that is down, answers an error status or talks non-JSON is the
/// 500 `{"error":"Could not fetch the pack list"}` — and nothing of any
/// of them touches the local packs.
#[tokio::test]
async fn remote_refuses_bad_urls_and_broken_remotes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "remotescreener", "admin:read").await;

    for query in [
        "",
        "?url=",
        "?url=%20%20",
        "?url=not-a-url",
        "?url=ftp://x.test/packs",
    ] {
        let refused =
            common::TestRequest::get(&format!("/api/v1/pleroma/emoji/packs/remote{query}"))
                .bearer(&token)
                .send(&app)
                .await;
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, &format!("url {query:?}"));
        common::assert_error(
            &refused,
            "url must be a valid http(s) URL",
            &format!("url {query:?}"),
        );
    }

    // A port nothing listens on any more: the connect failure is the 500.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_port = listener.local_addr().unwrap().port();
    drop(listener);
    let dead = common::TestRequest::get(&format!(
        "/api/v1/pleroma/emoji/packs/remote?url=http://127.0.0.1:{dead_port}"
    ))
    .bearer(&token)
    .send(&app)
    .await;
    common::assert_answer(&dead, StatusCode::INTERNAL_SERVER_ERROR, "a dead remote");
    common::assert_error(&dead, COULD_NOT_FETCH, "a dead remote");

    // An error status and a non-JSON body answer the same.
    let (failing_base, _failing, failing_server) = spawn_pack_server(|_| MockRemote {
        list_status: StatusCode::INTERNAL_SERVER_ERROR,
        ..MockRemote::default()
    })
    .await;
    let failing = common::TestRequest::get(&format!(
        "/api/v1/pleroma/emoji/packs/remote?url={failing_base}"
    ))
    .bearer(&token)
    .send(&app)
    .await;
    failing_server.abort();
    common::assert_answer(&failing, StatusCode::INTERNAL_SERVER_ERROR, "a 500 remote");
    common::assert_error(&failing, COULD_NOT_FETCH, "a 500 remote");

    let (junk_base, _junk, junk_server) = spawn_pack_server(|_| MockRemote {
        packs_as_junk: true,
        ..MockRemote::default()
    })
    .await;
    let junk = common::TestRequest::get(&format!(
        "/api/v1/pleroma/emoji/packs/remote?url={junk_base}"
    ))
    .bearer(&token)
    .send(&app)
    .await;
    junk_server.abort();
    common::assert_answer(
        &junk,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a non-JSON remote",
    );
    common::assert_error(&junk, COULD_NOT_FETCH, "a non-JSON remote");

    assert!(
        std::fs::read_dir(dir.path()).is_ok_and(|entries| entries.count() == 0),
        "a relay never reads or writes local packs"
    );
}

/// `POST /packs/download` of a shared pack: the listing is re-read with
/// its `files_count` (Pleroma's two-pass read), the archive lands under
/// the `as` name, the shipped manifest survives reduced to the files
/// that actually arrived, non-images and hidden entries are skipped,
/// the custom emojis resync and the answer is the JSON string `"ok"`.
#[tokio::test]
async fn download_installs_a_shared_remote_pack() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packpuller", "admin:write").await;

    let remote = MockRemote {
        pack_first: json!({
            "pack": {"license": "CC0-1.0", "share-files": true},
            "files": {"cat": "cat.png"},
            "files_count": 3,
        }),
        pack_full: Some(json!({
            "pack": {"license": "CC0-1.0", "share-files": true},
            "files": {"cat": "cat.png", "kitten": "sub/kitten.gif", "ghost": "gone.png"},
            "files_count": 3,
        })),
        ..MockRemote::default()
    };
    let (base, script, server) = spawn_pack_server(|_| remote).await;

    let response = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": base, "name": "cats", "as": "imported-cats"}))
        .send(&app)
        .await;
    server.abort();
    common::assert_answer(&response, StatusCode::OK, "shared pack download");
    assert_eq!(response.json_value(), json!("ok"), "Pleroma's `ok` body");

    let seen = script.seen_pack.lock().await;
    assert_eq!(seen.len(), 2, "the two-pass listing read: {seen:?}");
    assert!(
        seen[0].contains("page_size=0") && seen[1].contains("page_size=3"),
        "the truncated first page is re-read with the announced files_count: {seen:?}"
    );
    drop(seen);

    let installed = installed_manifest(dir.path(), "imported-cats").await;
    assert_eq!(
        manifest_files(&installed),
        [("cat", "cat.png"), ("kitten", "sub/kitten.gif")],
        "the archive manifest, reduced to the files that landed"
    );
    assert_eq!(
        installed.pack.license.as_deref(),
        Some("MIT"),
        "the shipped metadata came with the pack"
    );
    assert!(installed.pack.share_files, "and its share flag too");
    assert_eq!(
        pack_entries(dir.path(), "imported-cats"),
        ["cat.png", "pack.json", "sub"],
        "the text and hidden entries wrote nothing"
    );
    assert_eq!(
        std::fs::read(dir.path().join("imported-cats/sub/kitten.gif")).unwrap(),
        GIF_BYTES,
        "nested files keep their path"
    );
    assert_eq!(
        served_emojis(&app).await,
        [
            ("imported-cats".to_owned(), "cat".to_owned()),
            ("imported-cats".to_owned(), "kitten".to_owned()),
        ],
        "the resync served the installed pack"
    );
}

/// An unshared pack with a `fallback-src` downloads from it; that
/// archive ships no manifest, so the listing's files map and metadata
/// become the pack's, and an absent `as` installs under the remote name.
#[tokio::test]
async fn download_follows_a_fallback_src_and_keeps_the_listing_map() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packfaller", "admin:write").await;

    let (base, script, server) = spawn_pack_server(|base| MockRemote {
        pack_first: json!({
            "pack": {
                "description": "from the listing",
                "share-files": false,
                "fallback-src": format!("{base}/api/v1/pleroma/emoji/packs/archive"),
            },
            "files": {"cat": "cat.png", "ghost": "gone.png"},
            "files_count": 2,
        }),
        archive: build_zip(&[("cat.png", PNG_BYTES)]),
        ..MockRemote::default()
    })
    .await;

    let response = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": base, "name": "cats"}))
        .send(&app)
        .await;
    server.abort();
    common::assert_answer(&response, StatusCode::OK, "fallback download");
    assert_eq!(
        script.archive_hits.load(Ordering::SeqCst),
        1,
        "the fallback archive was fetched once"
    );

    let installed = installed_manifest(dir.path(), "cats").await;
    assert_eq!(
        manifest_files(&installed),
        [("cat", "cat.png")],
        "the listing's map, minus the file that never arrived"
    );
    assert_eq!(
        installed.pack.description.as_deref(),
        Some("from the listing"),
        "the listing's metadata came with the pack"
    );
    assert!(!installed.pack.share_files, "and it stayed unshared");
    assert_eq!(
        served_emojis(&app).await,
        [("cats".to_owned(), "cat".to_owned())],
        "the resync served the installed pack"
    );
}

/// A remote pack that shares nothing and names no fallback is the 400
/// `{"error":"The pack was not set as shared and there is no fallback
/// src to download from"}`; no archive is ever asked for and no
/// directory is left behind.
#[tokio::test]
async fn download_refuses_an_unshared_pack_without_a_fallback_src() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packbeggar", "admin:write").await;

    let (base, script, server) = spawn_pack_server(|_| MockRemote {
        pack_first: json!({"pack": {"share-files": false}, "files": {}, "files_count": 0}),
        ..MockRemote::default()
    })
    .await;
    let response = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": base, "name": "cats"}))
        .send(&app)
        .await;
    server.abort();
    common::assert_answer(&response, StatusCode::BAD_REQUEST, "unshared pack");
    common::assert_error(&response, NOT_SHAREABLE, "unshared pack");
    assert_eq!(
        script.archive_hits.load(Ordering::SeqCst),
        0,
        "an unshared pack never asks for an archive"
    );
    assert!(
        !dir.path().join("cats").exists(),
        "the refused download created no pack"
    );
}

/// A download that cannot finish — a remote listing that errors, or an
/// archive that is not a zip — answers the 500 "Could not fetch the pack
/// list" / 400 "Could not read the pack archive", and the half-created
/// pack directory of the extraction failure is removed again.
#[tokio::test]
async fn download_refuses_broken_remotes_and_archives() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packchecker", "admin:write").await;

    let (failing_base, _failing, failing_server) = spawn_pack_server(|_| MockRemote {
        list_status: StatusCode::NOT_FOUND,
        ..MockRemote::default()
    })
    .await;
    let failing = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": failing_base, "name": "cats"}))
        .send(&app)
        .await;
    failing_server.abort();
    common::assert_answer(
        &failing,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a failing listing",
    );
    common::assert_error(&failing, COULD_NOT_FETCH, "a failing listing");

    let (junk_base, _junk, junk_server) = spawn_pack_server(|_| MockRemote {
        archive: b"this is not a zip".to_vec(),
        ..MockRemote::default()
    })
    .await;
    let junk = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": junk_base, "name": "cats"}))
        .send(&app)
        .await;
    junk_server.abort();
    common::assert_answer(&junk, StatusCode::BAD_REQUEST, "a broken archive");
    common::assert_error(&junk, UNREADABLE, "a broken archive");
    assert!(
        !dir.path().join("cats").exists(),
        "the failed install removed its half-written pack"
    );
    assert!(
        std::fs::read_dir(dir.path()).is_ok_and(|entries| entries.count() == 0),
        "and left no other trace"
    );
}

/// An archive whose entries climb out of the pack directory is the 400
/// `{"error":"Pack archive contains an unsafe path"}`: nothing outside
/// the pack is written and the half-created pack is removed.
#[tokio::test]
async fn download_refuses_a_traversal_archive() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packescaper", "admin:write").await;

    for (bad, outside) in [
        ("../../escape.png", "escape.png"),
        ("/abs.png", "abs.png"),
        ("ok/../../up.png", "up.png"),
    ] {
        let (base, _script, server) = spawn_pack_server(move |_| MockRemote {
            archive: build_zip(&[("cat.png", PNG_BYTES), (bad, PNG_BYTES)]),
            ..MockRemote::default()
        })
        .await;
        let refused = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
            .bearer(&token)
            .json(&json!({"url": base, "name": "cats"}))
            .send(&app)
            .await;
        server.abort();
        common::assert_answer(&refused, StatusCode::BAD_REQUEST, &format!("entry {bad}"));
        common::assert_error(&refused, UNSAFE_PATH, &format!("entry {bad}"));
        assert!(
            !dir.path().join(outside).exists(),
            "the {bad} entry wrote nothing outside the pack"
        );
        assert!(
            !dir.path().join("cats").exists(),
            "and its pack was removed as the install failed"
        );
    }
}

/// An archive past the 20 MiB cap is refused mid-stream with the 500
/// "Could not download the pack archive", and an occupied local name —
/// checked after the remote work — answers the pinned 400 "Pack already
/// exists" without disturbing the pack that stands there.
#[tokio::test]
async fn download_respects_the_size_cap_and_the_existing_pack() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &pack(true, &[("cat", "cat.png")])).await;
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packhoarder", "admin:write").await;

    let oversized = vec![b'x'; emoji_packs::MAX_PACK_ARCHIVE_BYTES + 1];
    let (base, _script, server) = spawn_pack_server(move |_| MockRemote {
        archive: oversized,
        ..MockRemote::default()
    })
    .await;
    let big = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": base, "name": "not-here"}))
        .send(&app)
        .await;
    server.abort();
    common::assert_answer(
        &big,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a 20 MiB plus one archive",
    );
    common::assert_error(
        &big,
        "Could not download the pack archive",
        "an oversize archive",
    );
    assert!(
        !dir.path().join("not-here").exists(),
        "the oversize fetch created no pack"
    );

    let (occupied_base, _occupied, occupied_server) =
        spawn_pack_server(|_| MockRemote::default()).await;
    let occupied = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": occupied_base, "name": "cats", "as": "  "}))
        .send(&app)
        .await;
    occupied_server.abort();
    common::assert_answer(&occupied, StatusCode::BAD_REQUEST, "an occupied name");
    common::assert_error(&occupied, "Pack already exists", "an occupied name");
    let kept = installed_manifest(dir.path(), "cats").await;
    assert_eq!(
        manifest_files(&kept),
        [("cat", "cat.png")],
        "the standing pack was neither overwritten nor emptied"
    );
}

/// `POST /packs/download_zip` installs an uploaded archive without a
/// manifest: every image becomes an emoji named after its file stem,
/// nested paths are kept, texts, hidden names and a shipped manifest
/// play no naming role, and the custom emojis resync.
#[tokio::test]
async fn download_zip_names_emojis_after_the_file_stems() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "ziplifter", "admin:write").await;

    let archive = build_zip(&[
        ("cat.png", PNG_BYTES),
        ("cat.webp", WEBP_BYTES),
        ("sub/kitten.gif", GIF_BYTES),
        ("notes.txt", b"prose".as_slice()),
        (".hidden.png", PNG_BYTES),
        ("two words.png", PNG_BYTES),
    ]);
    let response = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download_zip")
        .bearer(&token)
        .multipart(
            BOUNDARY,
            &[
                common::MultipartPart::text("name", "packzip"),
                common::MultipartPart::file(
                    "file",
                    "whatever.zip",
                    "application/zip",
                    Bytes::from(archive),
                ),
            ],
        )
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "uploaded pack");
    assert_eq!(response.json_value(), json!("ok"), "Pleroma's `ok` body");

    let installed = installed_manifest(dir.path(), "packzip").await;
    assert_eq!(
        manifest_files(&installed),
        [("cat", "cat.webp"), ("kitten", "sub/kitten.gif")],
        "one emoji per stem, the last extracted name wins, junk and hidden names skipped"
    );
    assert!(
        !installed.pack.share_files,
        "a stem-built pack starts unshared"
    );
    assert_eq!(
        pack_entries(dir.path(), "packzip"),
        ["cat.png", "cat.webp", "pack.json", "sub"],
        "only the kept files and the manifest stand in the pack"
    );
    assert_eq!(
        served_emojis(&app).await,
        [
            ("packzip".to_owned(), "cat".to_owned()),
            ("packzip".to_owned(), "kitten".to_owned()),
        ],
        "the resync served the uploaded pack"
    );
    let served = common::TestRequest::get("/emoji/packzip/cat.webp")
        .send(&app)
        .await;
    common::assert_answer(&served, StatusCode::OK, "an uploaded image serves");
    assert_eq!(served.body.as_ref(), WEBP_BYTES, "byte for byte");
}

/// `download_zip` of an archive that ships its own `pack.json` keeps it,
/// reduced to the files that landed — the manifest on disk is what the
/// listing, the static route and the archive endpoint will read.
#[tokio::test]
async fn download_zip_keeps_the_shipped_manifest_reduced() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "zipkeeper", "admin:write").await;

    let archive = build_zip(&[
        ("pack.json", ARCHIVE_MANIFEST.as_bytes()),
        ("cat.png", PNG_BYTES),
    ]);
    let response =
        common::TestRequest::post("/api/v1/pleroma/emoji/packs/download_zip?name=shipped")
            .bearer(&token)
            .multipart(
                BOUNDARY,
                &[common::MultipartPart::file(
                    "file",
                    "shipped.zip",
                    "application/zip",
                    Bytes::from(archive),
                )],
            )
            .send(&app)
            .await;
    common::assert_answer(&response, StatusCode::OK, "manifest-carrying upload");

    let installed = installed_manifest(dir.path(), "shipped").await;
    assert_eq!(
        manifest_files(&installed),
        [("cat", "cat.png")],
        "only the entry whose file landed stands"
    );
    assert_eq!(
        installed.pack.license.as_deref(),
        Some("MIT"),
        "the shipped metadata survived the rewrite"
    );
}

/// `POST /api/v1/pleroma/emoji/packs/download_zip` with `name` and the
/// archive bytes as the `file` part.
async fn upload_pack(
    app: &axum::Router,
    token: &str,
    name: &'static str,
    archive: Vec<u8>,
) -> common::TestResponse {
    common::TestRequest::post("/api/v1/pleroma/emoji/packs/download_zip")
        .bearer(token)
        .multipart(
            BOUNDARY,
            &[
                common::MultipartPart::text("name", name),
                common::MultipartPart::file(
                    "file",
                    "pack.zip",
                    "application/zip",
                    Bytes::from(archive),
                ),
            ],
        )
        .send(app)
        .await
}

/// The upload route's parameter refusals: a body without a `file` part
/// is the 422 `{"error":"Missing parameter: file"}`, a blank name the
/// shared empty-name 400.
#[tokio::test]
async fn download_zip_refuses_bodies_without_file_or_name() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "zipbeggar", "admin:write").await;

    let no_file = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download_zip")
        .bearer(&token)
        .json(&json!({"name": "nothing"}))
        .send(&app)
        .await;
    common::assert_answer(
        &no_file,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a body without a file",
    );
    common::assert_error(&no_file, "Missing parameter: file", "a body without a file");

    let no_name = upload_pack(&app, &token, "   ", good_archive()).await;
    common::assert_answer(&no_name, StatusCode::BAD_REQUEST, "a blank name");
    common::assert_error(&no_name, "pack name cannot be empty", "a blank name");
    assert!(
        std::fs::read_dir(dir.path()).is_ok_and(|entries| entries.count() == 0),
        "the parameter refusals created nothing"
    );
}

/// The upload route's archive refusals: bytes that are no zip and an
/// archive that traverses answer the extraction's 400s with nothing
/// left behind, an occupied name the pinned 400 "Pack already exists"
/// without disturbing the standing pack, and a part past the configured
/// upload limit the 413 that names it.
#[tokio::test]
async fn download_zip_refuses_bad_archives_and_sizes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "taken", &pack(false, &[("cat", "cat.png")])).await;
    let mut config = common::test_config_with_emoji_dir(dir.path());
    config.upload_limits.max_file_bytes = 4096;
    let app = common::test_app_with_config(pool.clone(), config);
    let token = admin_token(&pool, "zipfilter", "admin:write").await;

    let junk = upload_pack(&app, &token, "junk", b"PK but not a zip".to_vec()).await;
    common::assert_answer(&junk, StatusCode::BAD_REQUEST, "no zip");
    common::assert_error(&junk, UNREADABLE, "no zip");

    let escaping = upload_pack(
        &app,
        &token,
        "escapes",
        build_zip(&[("../../outside.png", PNG_BYTES)]),
    )
    .await;
    common::assert_answer(&escaping, StatusCode::BAD_REQUEST, "a traversal");
    common::assert_error(&escaping, UNSAFE_PATH, "a traversal");
    assert!(
        !dir.path().join("escapes").exists() && !dir.path().join("outside.png").exists(),
        "the traversing upload installed nothing"
    );

    let occupied = upload_pack(&app, &token, "taken", good_archive()).await;
    common::assert_answer(&occupied, StatusCode::BAD_REQUEST, "an occupied name");
    common::assert_error(&occupied, "Pack already exists", "an occupied name");
    let kept = installed_manifest(dir.path(), "taken").await;
    assert_eq!(
        manifest_files(&kept),
        [("cat", "cat.png")],
        "the standing pack was neither overwritten nor emptied"
    );

    let mut oversize = good_archive();
    oversize.resize(5000, b'0');
    let big = upload_pack(&app, &token, "big", oversize).await;
    common::assert_answer(
        &big,
        StatusCode::PAYLOAD_TOO_LARGE,
        "a part past the upload limit",
    );
    assert!(
        error_message(&big).is_some_and(|message| message.contains("4096")),
        "the 413 names the limit: {}",
        big.text()
    );
    let standing: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(standing, ["taken"], "none of the refusals created a pack");
}

/// `GET /packs/import` writes a `pack.json` for every pack directory of
/// `EMOJI_DIR` that lacks a valid one — loose images and a broken
/// manifest alike — names the imported packs, leaves the valid packs and
/// hidden directories alone, resyncs the custom emojis, and finds
/// nothing the second time.
#[tokio::test]
async fn import_writes_manifests_for_loose_pack_directories() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    seed_pack(dir.path(), "cats", &pack(true, &[("cat", "cat.png")])).await;
    let cats_manifest = std::fs::read(dir.path().join("cats").join("pack.json")).unwrap();
    std::fs::create_dir_all(dir.path().join("dogs").join("sub")).unwrap();
    std::fs::write(dir.path().join("dogs").join("dog.png"), PNG_BYTES).unwrap();
    std::fs::write(dir.path().join("dogs/sub/puppy.gif"), GIF_BYTES).unwrap();
    std::fs::write(
        dir.path().join("dogs").join("notes.txt"),
        b"prose".as_slice(),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("broken")).unwrap();
    std::fs::write(dir.path().join("broken").join("pack.json"), b"{ not json").unwrap();
    std::fs::write(dir.path().join("broken").join("a.jpg"), PNG_BYTES).unwrap();
    std::fs::create_dir_all(dir.path().join(".secret")).unwrap();
    std::fs::write(dir.path().join(".secret/x.png"), PNG_BYTES).unwrap();
    std::fs::write(dir.path().join("stray.png"), PNG_BYTES).unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packimporter", "admin:read").await;

    let response = common::TestRequest::get("/api/v1/pleroma/emoji/packs/import")
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "filesystem import");
    assert_eq!(
        response.json_value(),
        json!(["broken", "dogs"]),
        "the two directories without a usable manifest, sorted"
    );

    let dogs = installed_manifest(dir.path(), "dogs").await;
    assert_eq!(
        manifest_files(&dogs),
        [("dog", "dog.png"), ("puppy", "sub/puppy.gif")],
        "stems key the images, nested paths kept, texts ignored"
    );
    assert_eq!(
        std::fs::read(dir.path().join("cats").join("pack.json")).unwrap(),
        cats_manifest,
        "the valid pack's manifest was not rewritten"
    );
    assert!(
        !dir.path().join(".secret").join("pack.json").exists(),
        "a hidden directory is not a pack name and gets nothing"
    );

    let again = common::TestRequest::get("/api/v1/pleroma/emoji/packs/import")
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&again, StatusCode::OK, "the second import");
    assert_eq!(
        again.json_value(),
        json!([]),
        "everything now has a manifest"
    );

    assert_eq!(
        served_emojis(&app).await,
        [
            ("broken".to_owned(), "a".to_owned()),
            ("cats".to_owned(), "cat".to_owned()),
            ("dogs".to_owned(), "dog".to_owned()),
            ("dogs".to_owned(), "puppy".to_owned()),
        ],
        "the resync served the imported packs"
    );
}

/// `POST /packs/download` with a body missing its `url` or `name` is the
/// extractor's 422, a blank `name` the shared 400 — and the request
/// reaches no remote at all.
#[tokio::test]
async fn download_demands_its_url_and_name() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let token = admin_token(&pool, "packhalfbody", "admin:write").await;

    for body in [
        json!({"name": "cats"}),
        json!({"url": "https://remote.invalid"}),
    ] {
        let refused = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
            .bearer(&token)
            .json(&body)
            .send(&app)
            .await;
        common::assert_answer(
            &refused,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a body missing a required key",
        );
    }

    let blank = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&token)
        .json(&json!({"url": "https://remote.invalid", "name": "   "}))
        .send(&app)
        .await;
    common::assert_answer(&blank, StatusCode::BAD_REQUEST, "a blank name");
    common::assert_error(&blank, "pack name cannot be empty", "a blank name");
    assert!(
        std::fs::read_dir(dir.path()).is_ok_and(|entries| entries.count() == 0),
        "a malformed download created nothing"
    );
}

/// The install routes' two GETs, with the query the remote one demands.
const INSTALL_GETS: [&str; 2] = [
    "/api/v1/pleroma/emoji/packs/remote?url=https://remote.invalid",
    "/api/v1/pleroma/emoji/packs/import",
];

/// The install routes' two POSTs and a body each; the auth extractor
/// rejects them before any of these bodies is parsed.
fn install_posts() -> [(&'static str, Value); 2] {
    [
        (
            "/api/v1/pleroma/emoji/packs/download",
            json!({"url": "https://remote.invalid", "name": "ghost"}),
        ),
        (
            "/api/v1/pleroma/emoji/packs/download_zip",
            json!({"name": "ghost"}),
        ),
    ]
}

/// The install routes answer the shared admin gate: no bearer, a bogus
/// token, an app-only token and a non-admin are refused alike on all
/// four, in the words the gate itself words them.
#[tokio::test]
async fn install_routes_refuse_weaker_credentials() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let app_only = common::create_app_only_token(&pool, "admin:read admin:write").await;
    let plain = plain_token(&pool, "installpleb", "admin:read admin:write").await;

    let refusals: &[Refusal<'_>] = &[
        (
            "no bearer token",
            None,
            StatusCode::UNAUTHORIZED,
            "Invalid credentials",
        ),
        (
            "a bogus token",
            Some("not-a-real-token"),
            StatusCode::UNAUTHORIZED,
            "The access token is invalid",
        ),
        (
            "an app-only token",
            Some(&app_only),
            StatusCode::FORBIDDEN,
            "This API requires an authenticated user",
        ),
        (
            "a non-admin user",
            Some(&plain),
            StatusCode::FORBIDDEN,
            "User is not an admin.",
        ),
    ];
    for path in &INSTALL_GETS {
        for (who, token, status, message) in refusals {
            let request = common::TestRequest::get(path);
            let response = match token {
                Some(bearer) => request.bearer(bearer).send(&app).await,
                None => request.send(&app).await,
            };
            common::assert_answer(&response, *status, &format!("get {path} as {who}"));
            common::assert_error(&response, message, &format!("get {path} as {who}"));
        }
    }
    for (path, body) in &install_posts() {
        for (who, token, status, message) in refusals {
            let request = common::TestRequest::post(path).json(body);
            let response = match token {
                Some(bearer) => request.bearer(bearer).send(&app).await,
                None => request.send(&app).await,
            };
            common::assert_answer(&response, *status, &format!("post {path} as {who}"));
            common::assert_error(&response, message, &format!("post {path} as {who}"));
        }
    }
}

/// The class the ticket pins per route: `admin:write` never grants the
/// GETs and `admin:read` never grants the POSTs, each refusal naming the
/// root it lacks; the narrow `admin:read:emoji` and `admin:write:emoji`
/// aliases pass the gate and reach the endpoints' own work — an answer
/// from past the gate on every one of them.
#[tokio::test]
async fn install_routes_enforce_their_admin_class() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let reader = admin_token(&pool, "installreader", "admin:read").await;
    let writer = admin_token(&pool, "installwriter", "admin:write").await;

    for path in &INSTALL_GETS {
        let refused = common::TestRequest::get(path)
            .bearer(&writer)
            .send(&app)
            .await;
        common::assert_answer(&refused, StatusCode::FORBIDDEN, "a writer on a reader");
        common::assert_error(
            &refused,
            "Insufficient permissions: admin:read.",
            "a writer on a reader",
        );
    }
    for (path, body) in &install_posts() {
        let refused = common::TestRequest::post(path)
            .bearer(&reader)
            .json(body)
            .send(&app)
            .await;
        common::assert_answer(&refused, StatusCode::FORBIDDEN, "a reader on a writer");
        common::assert_error(
            &refused,
            "Insufficient permissions: admin:write.",
            "a reader on a writer",
        );
    }

    let narrow_read = admin_token(&pool, "installnarrowr", "admin:read:emoji").await;
    let imported = common::TestRequest::get("/api/v1/pleroma/emoji/packs/import")
        .bearer(&narrow_read)
        .send(&app)
        .await;
    common::assert_answer(&imported, StatusCode::OK, "the narrow read alias imports");
    assert_eq!(
        imported.json_value(),
        json!([]),
        "the empty directory imports nothing"
    );
    let relayed =
        common::TestRequest::get("/api/v1/pleroma/emoji/packs/remote?url=https://remote.invalid")
            .bearer(&narrow_read)
            .send(&app)
            .await;
    common::assert_answer(
        &relayed,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the narrow read relay",
    );
    common::assert_error(&relayed, COULD_NOT_FETCH, "the narrow read relay");

    let narrow_write = admin_token(&pool, "installnarroww", "admin:write:emoji").await;
    let pulled = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&narrow_write)
        .json(&json!({"url": "https://remote.invalid", "name": "ghost"}))
        .send(&app)
        .await;
    common::assert_answer(
        &pulled,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the narrow write download",
    );
    let uploaded = upload_pack(&app, &narrow_write, "narrowzip", b"not a zip".to_vec()).await;
    common::assert_answer(
        &uploaded,
        StatusCode::BAD_REQUEST,
        "the narrow write upload",
    );
    common::assert_error(&uploaded, UNREADABLE, "the narrow write upload");

    assert!(
        !dir.path().join("narrowzip").exists() && !dir.path().join("ghost").exists(),
        "the post-gate failures left no packs behind"
    );
}
