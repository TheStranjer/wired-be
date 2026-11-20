//! The emoji-file add route: `POST /packs/files` with an uploaded part or
//! a fetched URL, the duplicate-shortcode refusal, bad files and unsafe
//! shortcodes and file names.

use axum::{body::Bytes, http::StatusCode};
use serde_json::{Value, json};
use wired_be::{config::TEST_BASE_URL, services::emoji_packs};

use super::{
    file_mocks::{MAX_FILE_BYTES, pack_entries, spawn_file_server, upload_parts},
    support::{
        BOUNDARY, GIF_BYTES, NOT_AN_IMAGE, PNG_BYTES, admin_token, log_rows, pack, seed_pack,
        served_emojis,
    },
};
use crate::common;

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
