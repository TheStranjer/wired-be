//! Tests for the media upload endpoints: `POST /api/v1/media` and
//! `POST /api/v2/media` (`src/api/media.rs`). They cover the whole path a
//! client takes — a multipart body in, the `Attachment` entity out, and the
//! file then readable at the URL the entity announced — plus every rejection
//! the endpoint owes: 401 and 403 for credentials and scopes, 413 over
//! `UPLOAD_LIMIT_BYTES`, and 422 for a missing or empty file, a type outside
//! `image/*`, `video/*` and `audio/*`, or a malformed `focus`. The
//! `GET /media/{key}` route those announced URLs point at is covered by
//! `tests/media.rs`.
//!
//! Uploads land in a [`TempDir`], never in the repository's own `./uploads`,
//! through the `common::test_app_with_*` helpers. The media fixtures are the
//! shortest byte sequences whose magic bytes the detector names: a decoder
//! would reject them, type detection only reads the header.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, body::Bytes, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use uuid::Uuid;
use wired_be::models::{media_attachment, user::User};

use crate::common::MultipartPart;

/// Boundary the multipart bodies are framed with.
const BOUNDARY: &str = "wired-media-test-boundary";
/// The scope both endpoints demand, as Pleroma's `OAuthScopesPlug` does.
const SCOPE: &str = "write:media";
/// The v1 upload path.
const V1: &str = "/api/v1/media";
/// The v2 upload path: the same handler, on a second address.
const V2: &str = "/api/v2/media";
/// Where the test instance serves stored files from.
const MEDIA_BASE_URL: &str = "https://test.example/media";

/// A `PNG`: the signature `infer` matches on, plus the start of an `IHDR`.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x00\x01";
/// A `JPEG`: `SOI` followed by the `APP0` marker.
const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01";
/// An `MP3`: an `ID3v2` tag header with a zero-length tag.
const MP3: &[u8] = b"ID3\x03\x00\x00\x00\x00\x00\x00";
/// An `MP4`: an `ftyp` box with the `mp42` brand.
const MP4: &[u8] = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom";
/// A `PDF`: bytes with a type, just not one of the three accepted ones.
const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";
/// Text: bytes no matcher can name at all.
const TEXT: &[u8] = b"a plain text file, neither image, video nor audio\n";

/// A throwaway uploads directory, removed when the returned guard drops.
fn uploads_dir() -> TempDir {
    TempDir::new().expect("temp dir for uploads")
}

/// The exact body of a JSON error response.
fn error_body(message: &str) -> String {
    format!(r#"{{"error":"{message}"}}"#)
}

/// A local user named `nickname` with a token granting `scopes`: both, since
/// one assertion needs the id and every request needs the token.
///
/// # Panics
///
/// If a fixture fails to insert.
async fn uploader(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, scopes).await;
    (user, token)
}

/// The `file` part of an upload: `bytes` uploaded as `filename` and announced
/// by the client as `content_type`.
fn file_part<B: Into<Bytes>>(
    filename: &'static str,
    content_type: &'static str,
    bytes: B,
) -> MultipartPart {
    MultipartPart::file("file", filename, content_type, bytes.into())
}

/// POSTs `parts` to `path` with `token` as the bearer credential.
async fn upload(
    app: &Router,
    path: &str,
    token: &str,
    parts: &[MultipartPart],
) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(token)
        .multipart(BOUNDARY, parts)
        .send(app)
        .await
}

/// Uploads one file part with no other parameter and returns the parsed
/// entity, reporting the body when the endpoint refused.
///
/// # Panics
///
/// If the upload is refused.
async fn upload_file(app: &Router, path: &str, token: &str, file: MultipartPart) -> Value {
    let response = upload(app, path, token, &[file]).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the upload is accepted, got: {}",
        response.text()
    );
    response.json_value()
}

/// The `media_attachments` row an upload answered, by its entity id.
///
/// # Panics
///
/// If `id` is not a `UUID` or no row carries it.
async fn stored_row(pool: &PgPool, id: &str) -> media_attachment::MediaAttachment {
    let Ok(row_id) = Uuid::parse_str(id) else {
        panic!("{id} is not the hyphenated UUID the entity owes");
    };
    let row = media_attachment::get_by_id(pool, row_id)
        .await
        .expect("the row reads");
    row.unwrap_or_else(|| panic!("the upload stored no row for {id}"))
}

/// The storage key of an upload: the part of its public URL below the media
/// prefix the test instance announces.
///
/// # Panics
///
/// If the URL is not under [`MEDIA_BASE_URL`].
fn storage_key(url: &str) -> String {
    url.strip_prefix(&format!("{MEDIA_BASE_URL}/"))
        .unwrap_or_else(|| panic!("{url} is not served from {MEDIA_BASE_URL}"))
        .to_owned()
}

/// The extension of a storage key: the part after the last dot.
///
/// # Panics
///
/// If the key has no dot, which a generated key always has.
fn key_extension(key: &str) -> &str {
    let (_, extension) = key
        .rsplit_once('.')
        .unwrap_or_else(|| panic!("{key} is not a named key"));
    extension
}

/// The `url` of an entity as a value, for comparing against a computed URL.
fn url_of(entity: &Value) -> String {
    entity["url"]
        .as_str()
        .expect("the entity carries a url")
        .to_owned()
}

/// The `MIME` type an upload's entity reports in its `pleroma` block.
fn mime_of(entity: &Value) -> String {
    entity["pleroma"]["mime_type"]
        .as_str()
        .expect("the pleroma block names the mime type")
        .to_owned()
}

/// The rows uploaded by `user`, counted straight from the table.
async fn row_count(pool: &PgPool, user: &User) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_attachments WHERE user_id = $1")
        .bind(user.id)
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// A `PNG` header padded to 80 bytes, for the tests that cap one file lower.
fn long_png() -> Vec<u8> {
    let mut bytes = PNG.to_vec();
    bytes.resize(80, b'x');
    bytes
}

/// The entries directly inside an uploads directory.
///
/// # Panics
///
/// If the directory cannot be read.
fn entries_in(dir: &TempDir) -> usize {
    std::fs::read_dir(dir.path())
        .expect("the uploads directory is readable")
        .count()
}

/// An uploaded `PNG` answers the `Attachment` entity, records an unattached
/// row for the uploader, and its bytes come back over the announced URL.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn png_upload_answers_the_entity_and_serves_the_file(pool: PgPool) {
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "alice", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let entity = upload_file(&app, V1, &token, file_part("cat.png", "image/png", PNG)).await;
    let id = entity["id"]
        .as_str()
        .expect("the id is a string")
        .to_owned();
    assert!(
        Uuid::parse_str(&id).is_ok(),
        "the id is a hyphenated UUID: {id}"
    );
    assert_eq!(entity["type"], "image", "a png is an image attachment");
    assert_eq!(
        entity["remote_url"],
        Value::Null,
        "a local file has no origin"
    );
    assert_eq!(entity["description"], Value::Null, "no alt text was sent");
    assert_eq!(
        entity["blurhash"],
        Value::Null,
        "detection is a later ticket"
    );
    assert_eq!(entity["meta"], json!({}), "and so is the metadata it fills");
    assert!(
        entity.get("file_size").is_none(),
        "the entity is Mastodon's shape, which has no file size"
    );

    let row = stored_row(&pool, &id).await;
    assert_eq!(row.user_id, Some(user.id), "the uploader owns the media");
    assert!(
        row.status_id.is_none(),
        "fresh media is attached to nothing"
    );
    assert_eq!(row.position, 0, "so its position is the default");
    assert_eq!(row.content_type, "image/png", "the stored type");
    assert_eq!(
        row.file_size,
        Some(i64::try_from(PNG.len()).expect("a fixture size fits an i64")),
        "the byte count of the stored file"
    );
    assert!(row.preview_url.is_none(), "thumbnailing is a later ticket");
    assert!(
        row.width.is_none() && row.height.is_none(),
        "and so are the dimensions"
    );
    assert!(!row.sensitive_hint, "the upload sends no sensitive flag");

    let key = row
        .storage_key
        .clone()
        .expect("a local upload has a storage key");
    assert!(
        key.starts_with(&format!("{}/", user.id)),
        "the key is namespaced by the uploader: {key}"
    );
    assert_eq!(
        key_extension(&key),
        "png",
        "the key carries the detected type"
    );
    assert_eq!(
        url_of(&entity),
        format!("{MEDIA_BASE_URL}/{key}"),
        "the announced url is the uploader's public url for the key"
    );
    assert_eq!(
        entity["preview_url"],
        url_of(&entity),
        "with no thumbnail the preview is the file itself"
    );
    assert_eq!(
        entity["text_url"],
        url_of(&entity),
        "and so is the text url"
    );
    assert_eq!(mime_of(&entity), "image/png", "the mime type rides along");

    let served = common::TestRequest::get(&format!("/media/{key}"))
        .send(&app)
        .await;
    assert_eq!(served.status, StatusCode::OK, "the announced url serves");
    assert_eq!(served.body, Bytes::from_static(PNG), "byte for byte");
    assert_eq!(
        served.header("content-type"),
        Some("image/png"),
        "as the type that was detected"
    );
}

/// Every accepted kind uploads as the type its bytes carry, and the entity's
/// `type` follows it: `image`, `video` or `audio`.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn each_media_kind_keeps_the_type_its_bytes_carry(pool: PgPool) {
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "carol", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for (bytes, filename, extension, mime, kind) in [
        (PNG, "cat.png", "png", "image/png", "image"),
        (JPEG, "cat.jpg", "jpg", "image/jpeg", "image"),
        (MP3, "song.mp3", "mp3", "audio/mpeg", "audio"),
        (MP4, "clip.mp4", "mp4", "video/mp4", "video"),
    ] {
        let entity = upload_file(&app, V1, &token, file_part(filename, mime, bytes)).await;
        assert_eq!(entity["type"], kind, "the entity type for {filename}");
        assert_eq!(mime_of(&entity), mime, "the mime type for {filename}");

        let key = storage_key(&url_of(&entity));
        assert_eq!(
            key_extension(&key),
            extension,
            "the key of {filename} carries the detected extension"
        );
        let served = common::TestRequest::get(&format!("/media/{key}"))
            .send(&app)
            .await;
        assert_eq!(served.status, StatusCode::OK, "{filename} serves back");
        assert_eq!(
            served.body,
            Bytes::from_static(bytes),
            "{filename} is intact"
        );
        assert_eq!(
            served.header("content-type"),
            Some(mime),
            "served as {mime}, the type its bytes said"
        );
        assert_eq!(
            served.header("content-disposition"),
            Some("inline"),
            "{mime} is a media type a browser renders"
        );
    }
}

/// `description` and `focus` reach the row and the entity reports them: the
/// focal point is the only part of `meta` an upload can fill.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn description_and_focus_are_stored(pool: PgPool) {
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "dave", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let parts = [
        file_part("mat.png", "image/png", PNG),
        MultipartPart::text("description", "a cat on a mat"),
        MultipartPart::text("focus", "0.25,-0.5"),
    ];
    let entity = upload(&app, V1, &token, &parts).await.json_value();
    assert_eq!(
        entity["description"], "a cat on a mat",
        "the alt text comes back"
    );
    assert_eq!(
        entity["meta"],
        json!({ "focus": { "x": 0.25, "y": -0.5 } }),
        "the focal point is meta's one filled part"
    );

    let id = entity["id"].as_str().expect("an id").to_owned();
    let row = stored_row(&pool, &id).await;
    assert_eq!(
        row.description.as_deref(),
        Some("a cat on a mat"),
        "the row keeps the alt text"
    );
    assert_eq!(row.focus_x, Some(0.25), "the row keeps focus x");
    assert_eq!(row.focus_y, Some(-0.5), "the row keeps focus y");
    assert_eq!(row.user_id, Some(user.id), "for the uploader");

    let both_ends = [
        file_part("edge.png", "image/png", PNG),
        MultipartPart::text("focus", "-1,1"),
    ];
    let edged = upload(&app, V1, &token, &both_ends).await.json_value();
    assert_eq!(
        edged["meta"]["focus"],
        json!({ "x": -1.0, "y": 1.0 }),
        "both ends of the range are inclusive"
    );
}

/// A `focus` that is not two floats in `[-1.0, 1.0]` is a 422, and nothing is
/// stored for it.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn an_invalid_focus_is_unprocessable(pool: PgPool) {
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "erin", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for focus in [
        "0.5",
        "x,y",
        "1.5,0",
        "0,-1.5",
        "0.5,0.5,0.5",
        "abc,def",
        "nan,nan",
        "inf,0",
        ",0",
        "0,",
    ] {
        let parts = [
            file_part("mat.png", "image/png", PNG),
            MultipartPart::text("focus", focus),
        ];
        let response = upload(&app, V1, &token, &parts).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{focus}` is not a focal point"
        );
        assert_eq!(
            response.text(),
            error_body("Validation failed: Focus is invalid"),
            "the 422 says what was wrong with `{focus}`"
        );
    }
    assert_eq!(
        row_count(&pool, &user).await,
        0,
        "a rejected focus stores no media"
    );
}

/// An absent `focus`, and one sent empty, store no focal point instead of
/// failing: clearing the value is not a validation error.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn an_empty_focus_stores_nothing(pool: PgPool) {
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "fred", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let blank = [
        file_part("mat.png", "image/png", PNG),
        MultipartPart::text("focus", ""),
    ];
    let entity = upload(&app, V1, &token, &blank).await.json_value();
    assert_eq!(entity["meta"], json!({}), "an empty focus is no focus");
    let id = entity["id"].as_str().expect("an id").to_owned();
    let row = stored_row(&pool, &id).await;
    assert_eq!(row.focus_x, None, "and stores no x");
    assert_eq!(row.focus_y, None, "nor y");
    assert_eq!(
        row.user_id,
        Some(user.id),
        "the upload is still the caller's"
    );

    let absent = upload_file(&app, V1, &token, file_part("mat.png", "image/png", PNG)).await;
    assert_eq!(absent["meta"], json!({}), "and so is an absent one");
}

/// No `file` part at all is the ticket's 422, whatever else the body holds.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_body_without_a_file_is_a_missing_parameter(pool: PgPool) {
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "gina", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let cases = [
        // Only a text field.
        upload(
            &app,
            V1,
            &token,
            &[MultipartPart::text("description", "no file here")],
        )
        .await,
        // No parts at all.
        upload(&app, V1, &token, &[]).await,
        // The file under another field name.
        upload(
            &app,
            V1,
            &token,
            &[MultipartPart::file(
                "upload",
                "cat.png",
                "image/png",
                Bytes::from_static(PNG),
            )],
        )
        .await,
        // A `POST` to the second version's path, the same body.
        upload(
            &app,
            V2,
            &token,
            &[MultipartPart::text("description", "no file here")],
        )
        .await,
        // A JSON body, which carries no files.
        common::TestRequest::post(V1)
            .bearer(&token)
            .json(&json!({"description": "no file"}))
            .send(&app)
            .await,
    ];
    for response in cases {
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a body without a `file` part is a 422, got: {}",
            response.text()
        );
        assert_eq!(
            response.text(),
            error_body("Missing parameter: file"),
            "the 422 names the parameter that was missing"
        );
    }
    assert_eq!(
        row_count(&pool, &user).await,
        0,
        "and a rejected body stores no media"
    );
}

/// A `file` part that carries no bytes is rejected as blank: an empty file has
/// no magic bytes to detect and would store nothing.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn an_empty_file_is_rejected(pool: PgPool) {
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "heidi", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for path in [V1, V2] {
        let response = upload(
            &app,
            path,
            &token,
            &[file_part("empty.png", "image/png", "")],
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path} refuses a zero-length file"
        );
        assert_eq!(
            response.text(),
            error_body("Validation failed: File can't be blank"),
            "the 422 says the file was blank"
        );
    }
    assert_eq!(row_count(&pool, &user).await, 0, "nothing was stored");
    assert_eq!(
        entries_in(&dir),
        0,
        "and nothing was written to the uploads directory"
    );
}

/// Bytes that are not image, video or audio never enter the store, whether the
/// detector names their type or not.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_file_that_is_not_media_is_unprocessable(pool: PgPool) {
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "ivan", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for (bytes, filename) in [(PDF, "report.pdf"), (TEXT, "notes.txt")] {
        let response = upload(&app, V1, &token, &[file_part(filename, "image/png", bytes)]).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{filename} is not media, whatever its part claimed"
        );
        assert_eq!(
            response.text(),
            error_body("Unsupported file type"),
            "the 422 is about the type"
        );
    }
    assert_eq!(
        row_count(&pool, &user).await,
        0,
        "no row for a refused file"
    );
    assert_eq!(entries_in(&dir), 0, "and no file on disk");
}

/// The bytes decide the type, never the part header: a `PNG` sent as HTML is
/// stored, named and served as the image it is.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_client_header_never_decides_the_type(pool: PgPool) {
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "judy", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let entity = upload_file(&app, V1, &token, file_part("cat.png", "text/html", PNG)).await;
    assert_eq!(entity["type"], "image", "the detected kind wins");
    assert_eq!(mime_of(&entity), "image/png", "and so does the mime type");

    let key = storage_key(&url_of(&entity));
    assert_eq!(
        key_extension(&key),
        "png",
        "the key follows the bytes, not the header"
    );
    let served = common::TestRequest::get(&format!("/media/{key}"))
        .send(&app)
        .await;
    assert_eq!(
        served.header("content-type"),
        Some("image/png"),
        "the file serves as what it is, not as what was claimed"
    );
    assert_eq!(
        served.header("content-disposition"),
        Some("inline"),
        "and it is the media the extension says"
    );
}

/// A file over `UPLOAD_LIMIT_BYTES` is a 413 naming the limit it passed.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn an_oversize_file_is_payload_too_large(pool: PgPool) {
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "ken", SCOPE).await;
    let app = common::test_app_with_upload_limits(pool.clone(), dir.path(), 64);

    for path in [V1, V2] {
        let response = upload(
            &app,
            path,
            &token,
            &[file_part("big.png", "image/png", long_png())],
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{path} refuses a file over the per-file limit"
        );
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "the 413 is the standard JSON error"
        );
        let body = response.json_value();
        let message = body["error"].as_str().expect("an error message");
        assert!(
            message.contains("64 bytes"),
            "the 413 names the limit it enforced: {message}"
        );
    }
    assert_eq!(
        row_count(&pool, &user).await,
        0,
        "an oversize file stores nothing"
    );
}

/// A file larger than axum's 2 MiB extractor default but within
/// `UPLOAD_LIMIT_BYTES` uploads whole: the media route carries
/// `upload_body_limit`, which is what makes the configured limit the one that
/// applies.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_file_over_the_extractor_default_uploads(pool: PgPool) {
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "lana", SCOPE).await;
    let app = common::test_app_with_upload_limits(pool.clone(), dir.path(), 4_000_000);

    // The PNG signature, padded just past the 2 MiB axum defaults at.
    let mut big: Vec<u8> = PNG.to_vec();
    big.resize(2 * 1024 * 1024 + 1024, b'x');
    let expected = Bytes::from(big.clone());

    let entity = upload_file(&app, V1, &token, file_part("big.png", "image/png", big)).await;
    let key = storage_key(&url_of(&entity));
    let served = common::TestRequest::get(&format!("/media/{key}"))
        .send(&app)
        .await;
    assert_eq!(served.status, StatusCode::OK, "the file serves back");
    assert_eq!(
        served.body.len(),
        expected.len(),
        "the whole file arrived, not a truncated head"
    );
    assert_eq!(served.body, expected, "and its bytes are intact");
    assert_eq!(
        entity["pleroma"]["mime_type"], "image/png",
        "the type still came from the signature"
    );
}

/// Without a token the endpoint answers 401 and the body is never stored.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn an_anonymous_upload_is_unauthorized(pool: PgPool) {
    let dir = uploads_dir();
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for path in [V1, V2] {
        let response = common::TestRequest::post(path)
            .multipart(BOUNDARY, &[file_part("cat.png", "image/png", PNG)])
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{path} needs a token"
        );
        assert_eq!(
            response.text(),
            error_body("Invalid credentials"),
            "the 401 is Pleroma's wording"
        );
        assert_eq!(
            response.header("www-authenticate"),
            Some("Bearer"),
            "and it names the scheme"
        );
    }
    assert_eq!(entries_in(&dir), 0, "no file was written");
}

/// A token that does not grant `write:media` is a 403 naming the scope, and an
/// app-only token cannot upload at all.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_token_without_the_scope_is_forbidden(pool: PgPool) {
    let dir = uploads_dir();
    let (user, read_media) = uploader(&pool, "mona", "read:media").await;
    let (_other, write_statuses) = uploader(&pool, "nora", "write:statuses").await;
    let (_third, read) = uploader(&pool, "olly", "read").await;
    let app_only = common::create_app_only_token(&pool, "write").await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let denials = [
        ("read:media", &read_media),
        ("write:statuses", &write_statuses),
        ("read", &read),
    ];
    for (label, token) in denials {
        let response = upload(&app, V1, token, &[file_part("cat.png", "image/png", PNG)]).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{label} is a 403");
        assert_eq!(
            response.text(),
            error_body("Insufficient permissions: write:media."),
            "the 403 names the missing scope for {label}"
        );
    }

    let anonymous = upload(
        &app,
        V1,
        &app_only,
        &[file_part("cat.png", "image/png", PNG)],
    )
    .await;
    assert_eq!(
        anonymous.status,
        StatusCode::FORBIDDEN,
        "an app-only token is not a user"
    );
    assert_eq!(
        anonymous.text(),
        error_body("This API requires an authenticated user"),
        "and the 403 says so"
    );
    assert_eq!(
        row_count(&pool, &user).await,
        0,
        "a forbidden upload stores nothing"
    );
}

/// The broader `write` scope uploads too: `write:media` is satisfied by its
/// top-level scope as well as by itself.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_broader_write_scope_uploads_too(pool: PgPool) {
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "peggy", "write").await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for path in [V1, V2] {
        let response = upload(
            &app,
            path,
            &token,
            &[file_part("cat.png", "image/png", PNG)],
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "`write` covers `write:media` on {path}: {}",
            response.text()
        );
    }
}

/// The two versions are one endpoint: same status, same entity keys, same
/// fields, and two separate uploads.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn v1_and_v2_answer_the_same(pool: PgPool) {
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "quinn", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let mut entities: Vec<(String, Value)> = Vec::new();
    for path in [V1, V2] {
        let parts = [
            file_part("cat.png", "image/png", PNG),
            MultipartPart::text("description", "the same cat"),
            MultipartPart::text("focus", "0,0"),
        ];
        let response = upload(&app, path, &token, &parts).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{path} answers 200, not Mastodon's 202: uploads are synchronous"
        );
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "{path} answers JSON"
        );
        entities.push((path.to_owned(), response.json_value()));
    }

    let (_, first) = &entities[0];
    let (second_path, second) = &entities[1];
    assert_eq!(
        sorted_keys(first),
        sorted_keys(second),
        "{second_path} reports the same keys as v1"
    );
    for field in ["type", "description", "blurhash", "remote_url", "meta"] {
        assert_eq!(
            first[field], second[field],
            "the {field} of the two responses agrees"
        );
    }
    assert_ne!(
        first["id"], second["id"],
        "but they are two separate uploads"
    );
    assert_ne!(
        url_of(first),
        url_of(second),
        "each with its own unguessable url"
    );

    let ids: Vec<String> = entities
        .iter()
        .map(|(_, entity)| entity["id"].as_str().expect("an id").to_owned())
        .collect();
    for id in &ids {
        let row = stored_row(&pool, id).await;
        assert_eq!(row.user_id, Some(user.id), "both rows belong to the caller");
        assert!(row.status_id.is_none(), "and both are unattached");
        let key = row.storage_key.clone().expect("a local upload has a key");
        let served = common::TestRequest::get(&format!("/media/{key}"))
            .send(&app)
            .await;
        assert_eq!(
            served.body,
            Bytes::from_static(PNG),
            "and its file serves from the announced url"
        );
    }
}

/// A `thumbnail` is accepted and ignored for now, whether a client sends it as
/// a file part or as a text field: neither reaches the row.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn a_thumbnail_is_accepted_and_ignored(pool: PgPool) {
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "rosa", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let parts = [
        file_part("cat.png", "image/png", PNG),
        MultipartPart::text("description", "a cat"),
        file_part("thumb.png", "image/png", JPEG),
        MultipartPart::text("thumbnail", "ignored"),
    ];
    let entity = upload(&app, V1, &token, &parts).await.json_value();
    assert_eq!(entity["type"], "image", "the upload went through");
    assert_eq!(
        entity["description"], "a cat",
        "the description belongs to the file"
    );
    assert_eq!(mime_of(&entity), "image/png", "and one type for one file");

    let id = entity["id"].as_str().expect("an id").to_owned();
    let row = stored_row(&pool, &id).await;
    let key = row.storage_key.clone().expect("a storage key");
    assert_eq!(
        row.content_type, "image/png",
        "the thumbnail did not change the stored type"
    );
    let served = common::TestRequest::get(&format!("/media/{key}"))
        .send(&app)
        .await;
    assert_eq!(
        served.body,
        Bytes::from_static(PNG),
        "and what got stored is the `file` part"
    );
    assert_eq!(row_count(&pool, &user).await, 1, "one file in, one row");
    assert_eq!(entries_in(&dir), 1, "and one file on disk, not two");
}

/// Rejected uploads leave nothing behind, and repeated uploads of the same
/// bytes never share a key.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn repeated_uploads_do_not_share_a_key(pool: PgPool) {
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "sue", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let mut keys: Vec<String> = Vec::new();
    for _ in 0..3 {
        let entity = upload_file(&app, V1, &token, file_part("cat.png", "image/png", PNG)).await;
        keys.push(storage_key(&url_of(&entity)));
        let rejected = upload(&app, V1, &token, &[file_part("x.txt", "image/png", TEXT)]).await;
        assert_eq!(
            rejected.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "the unsupported file between them is refused"
        );
    }
    let unique: std::collections::HashSet<String> = keys.iter().cloned().collect();
    assert_eq!(unique.len(), 3, "every upload minted its own key");
    for key in &keys {
        assert!(
            key.starts_with(&format!("{}/", user.id)),
            "and every key is in the uploader's namespace: {key}"
        );
    }
    assert_eq!(
        row_count(&pool, &user).await,
        3,
        "one row per accepted upload"
    );
    assert_eq!(
        entries_in(&dir),
        1,
        "one directory, shared by the uploader's three files"
    );
}

/// The upload routes are `POST`-only: a `GET` there is a method mismatch, not
/// a path the fallback swallows.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn only_post_uploads(pool: PgPool) {
    let dir = uploads_dir();
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for path in [V1, V2] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(
            response.status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{path} is not readable"
        );
        assert!(response.body.is_empty(), "axum's 405 body is empty");
    }
}

/// The keys of an entity object, sorted, for comparing two responses exactly.
///
/// # Panics
///
/// If the value is not an object.
fn sorted_keys(entity: &Value) -> Vec<&str> {
    let mut names: Vec<&str> = entity
        .as_object()
        .expect("an entity object")
        .keys()
        .map(String::as_str)
        .collect();
    names.sort_unstable();
    names
}
