//! The pack create and metadata-patch routes: `POST /pack` and
//! `PATCH /pack`, their validation refusals and the moderation log entries
//! they leave behind.

use std::collections::BTreeMap;

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::services::emoji_packs::{Pack, PackMeta};

use super::support::{admin_token, log_row, log_rows, pack, seed_pack};
use crate::common;

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
