//! Integration tests for the emoji-pack endpoints of
//! `src/api/emoji_packs.rs`: the pack listing and its pagination, the
//! single-pack view, the zip archive download and its sharing gate, the
//! util emoji map over synced `custom_emojis` rows, the static
//! `GET /emoji/{pack}/{file}` route with its media safety headers, and
//! the admin write routes of WBE-211: create, metadata patch and delete
//! under `admin:write`, their validation refusals, the `custom_emojis`
//! resync they trigger and the `emoji_pack_*` moderation entries they
//! leave behind. The reads answer without authentication and unknown or
//! unsafe pack names answer one shared 404; the writes demand an admin
//! bearer token and answer the named `"Pack <name> does not exist"` 404.

#![cfg(test)]

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
    models::{
        custom_emoji::{self, NewCustomEmoji},
        user::{Flag, NewUser},
    },
    services::emoji_packs::{self, Pack, PackMeta},
};

use crate::common;

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
