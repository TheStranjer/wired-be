//! The local install routes of WBE-216: `POST /packs/download_zip` for an
//! uploaded archive, `GET /packs/import` for the filesystem import and the
//! body refusals of `POST /packs/download`.

use axum::{body::Bytes, http::StatusCode};
use serde_json::json;

use super::{
    file_mocks::pack_entries,
    remote_mocks::{
        ARCHIVE_MANIFEST, UNREADABLE, UNSAFE_PATH, WEBP_BYTES, build_zip, good_archive,
        installed_manifest, manifest_files,
    },
    support::{
        BOUNDARY, GIF_BYTES, PNG_BYTES, admin_token, error_message, pack, seed_pack, served_emojis,
    },
};
use crate::common;

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
pub(super) async fn upload_pack(
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
    let token = admin_token(&pool, "packimporter", "admin:write").await;

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
