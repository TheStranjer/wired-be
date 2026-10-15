//! Tests for the media upload endpoints: `POST /api/v1/media` and
//! `POST /api/v2/media` (`src/api/media.rs`). They cover the whole path a
//! client takes — a multipart body in, the `Attachment` entity out, and the
//! file then readable over the URLs the entity announced — plus every
//! rejection the endpoint owes: 401 and 403 for credentials and scopes, 413
//! over `UPLOAD_LIMIT_BYTES`, and 422 for a missing or empty file, a type
//! outside `image/*`, `video/*` and `audio/*`, an undecodable or oversized
//! image, or a malformed `focus`. The `GET /media/{key}` route those
//! announced URLs point at is covered by `tests/media.rs`.
//!
//! Uploads land in a [`TempDir`], never in the repository's own `./uploads`,
//! through the `common::test_app_with_*` helpers. Image fixtures are real,
//! decodable files built with the `image` crate — the endpoint now analyzes
//! `image/*` uploads, so a magic-bytes header is only good for bodies the
//! analysis never reaches (rejected earlier, or not an image). Audio and
//! video fixtures stay header-only: the detector names their type from the
//! magic bytes and nothing decodes them.

#![cfg(test)]

use axum::{Router, body::Bytes, http::StatusCode};
use image::{DynamicImage, Frame, ImageBuffer, ImageFormat, Rgba, RgbaImage};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::io::Cursor;
use tempfile::TempDir;
use uuid::Uuid;
use wired_be::models::{media_attachment, user::User};

use crate::common::{self, MultipartPart};

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
/// The edge the upload endpoint scales generated previews down to.
const PREVIEW_EDGE: u32 = 400;

/// An `MP3`: an `ID3v2` tag header with a zero-length tag.
const MP3: &[u8] = b"ID3\x03\x00\x00\x00\x00\x00\x00";
/// An `MP4`: an `ftyp` box with the `mp42` brand.
const MP4: &[u8] = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom";
/// A `PDF`: bytes with a type, just not one of the three accepted ones.
const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";
/// Text: bytes no matcher can name at all.
const TEXT: &[u8] = b"a plain text file, neither image, video nor audio\n";

/// The deterministic gradient color of pixel `(x, y)`, opaque.
fn raster_pixel(x: u32, y: u32) -> Rgba<u8> {
    let channel = |value: u32| u8::try_from(value % 256).expect("a modulo of 256 fits a u8");
    Rgba([
        channel(x * 7 + y * 3),
        channel(x + y * 11),
        channel(x * y),
        255,
    ])
}

/// A deterministic `width` x `height` gradient, fully opaque.
fn raster(width: u32, height: u32) -> RgbaImage {
    ImageBuffer::from_fn(width, height, raster_pixel)
}

/// A gradient with every other column fully transparent: alpha to keep.
fn transparent_raster(width: u32, height: u32) -> RgbaImage {
    ImageBuffer::from_fn(width, height, |x, y| {
        let Rgba([r, g, b, _]) = raster_pixel(x, y);
        let alpha = if x % 2 == 0 { 0 } else { 255 };
        Rgba([r, g, b, alpha])
    })
}

/// The `image` fixture encoded as `format`'s bytes.
fn write_raster(image: RgbaImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Vec::new();
    DynamicImage::ImageRgba8(image)
        .write_to(Cursor::new(&mut out), format)
        .expect("the fixture encodes");
    out
}

/// A decodable `PNG` of `width` x `height`.
fn png(width: u32, height: u32) -> Vec<u8> {
    write_raster(raster(width, height), ImageFormat::Png)
}

/// A decodable `JPEG` of `width` x `height`.
fn jpeg(width: u32, height: u32) -> Vec<u8> {
    write_raster(raster(width, height), ImageFormat::Jpeg)
}

/// A decodable, transparent `PNG` of `width` x `height`.
fn transparent_png(width: u32, height: u32) -> Vec<u8> {
    write_raster(transparent_raster(width, height), ImageFormat::Png)
}

/// A two-frame animated `GIF`: `first` sized, then a smaller second frame.
/// The encoder writes the trailer on drop.
fn animated_gif() -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut out);
        let first = RgbaImage::from_pixel(640, 480, Rgba([210, 30, 30, 255]));
        let second = RgbaImage::from_pixel(320, 240, Rgba([30, 30, 210, 255]));
        encoder
            .encode_frame(Frame::new(first))
            .expect("the first frame encodes");
        encoder
            .encode_frame(Frame::new(second))
            .expect("the second frame encodes");
    }
    out
}

/// The `meta.original` object a `width` x `height` image must report.
fn original_meta(width: u32, height: u32) -> Value {
    json!({
        "width": width,
        "height": height,
        "size": format!("{width}x{height}"),
        "aspect": f64::from(width) / f64::from(height),
    })
}

/// The `meta.small` object the generated preview of a `width` x `height`
/// image must report: scaled down to `PREVIEW_EDGE` on the long edge.
/// Only used with fixtures whose scale is exact (640x480, 800x600), so the
/// rounding of the resize never shows.
fn small_meta(width: u32, height: u32) -> Value {
    let long_edge = width.max(height);
    let small = |edge: u32| (edge * PREVIEW_EDGE).div_euclid(long_edge);
    let (small_w, small_h) = (small(width), small(height));
    json!({
        "width": small_w,
        "height": small_h,
        "size": format!("{small_w}x{small_h}"),
        "aspect": f64::from(small_w) / f64::from(small_h),
    })
}

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

/// The entity id an upload answered.
///
/// # Panics
///
/// If the entity carries no id.
fn entity_id(entity: &Value) -> String {
    entity["id"]
        .as_str()
        .expect("the id is a string")
        .to_owned()
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

/// A `PNG` header padded to 80 bytes, for the test that caps one file lower.
/// The 413 comes from the body limit, so these bytes never reach a decoder.
fn long_png_header() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01".to_vec();
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

/// The rendered `Attachment` entity of a fresh `PNG` upload: its type, the
/// fields a local unattached upload leaves empty, the blurhash and both
/// dimension variants of `meta`.
fn assert_attachment_entity(entity: &Value, id: &str) {
    assert!(
        Uuid::parse_str(id).is_ok(),
        "the id is a hyphenated UUID: {id}"
    );
    assert_eq!(entity["type"], "image", "a png is an image attachment");
    assert_eq!(
        entity["remote_url"],
        Value::Null,
        "a local file has no origin"
    );
    assert_eq!(entity["description"], Value::Null, "no alt text was sent");
    assert!(
        entity["blurhash"]
            .as_str()
            .is_some_and(|hash| !hash.is_empty()),
        "the upload computed a blurhash: {}",
        entity["blurhash"]
    );
    assert_eq!(
        entity["meta"],
        json!({
            "original": original_meta(640, 480),
            "small": small_meta(640, 480),
        }),
        "detection filled both dimension variants of the meta"
    );
    assert!(
        entity.get("file_size").is_none(),
        "the entity is Mastodon's shape, which has no file size"
    );
}

/// The row a `PNG` upload stored: the uploader's own, attached to nothing,
/// with the analyzed dimensions and byte count of the file.
fn assert_stored_png(row: &media_attachment::MediaAttachment, owner: &User, bytes: usize) {
    assert_eq!(row.user_id, Some(owner.id), "the uploader owns the media");
    assert!(
        row.status_id.is_none(),
        "fresh media is attached to nothing"
    );
    assert_eq!(row.position, 0, "so its position is the default");
    assert_eq!(row.content_type, "image/png", "the stored type");
    assert_eq!(
        row.file_size,
        Some(i64::try_from(bytes).expect("a fixture size fits an i64")),
        "the byte count of the stored file"
    );
    assert_eq!(row.width, Some(640), "the analyzed width is stored");
    assert_eq!(row.height, Some(480), "and the analyzed height");
    assert!(row.blurhash.is_some(), "and the blurhash");
    assert!(!row.sensitive_hint, "the upload sends no sensitive flag");
}

/// The two keys the upload wrote — the original and its `_small` preview —
/// plus the announcement of both: the uploader's namespace, the detected
/// extension and the public URL for each key.
fn assert_stored_keys(
    entity: &Value,
    row: &media_attachment::MediaAttachment,
    owner: &User,
) -> (String, String) {
    let key = row
        .storage_key
        .clone()
        .expect("a local upload has a storage key");
    assert!(
        key.starts_with(&format!("{}/", owner.id)),
        "the key is namespaced by the uploader: {key}"
    );
    assert_eq!(
        key_extension(&key),
        "png",
        "the key carries the detected type"
    );
    assert_eq!(
        url_of(entity),
        format!("{MEDIA_BASE_URL}/{key}"),
        "the announced url is the uploader's public url for the key"
    );

    // The generated preview: the same stem with a `_small` suffix, JPEG
    // because the opaque source has no alpha to keep.
    let preview_key = row
        .preview_url
        .as_deref()
        .map(storage_key)
        .expect("the row records the preview url");
    assert_eq!(
        format!("{MEDIA_BASE_URL}/{preview_key}"),
        entity["preview_url"].as_str().expect("a preview url"),
        "the entity announces the preview it stored"
    );
    let stem = key
        .strip_suffix(".png")
        .expect("the key names its extension");
    assert_eq!(
        preview_key,
        format!("{stem}_small.jpg"),
        "same stem, _small, jpeg for an opaque image"
    );
    assert_eq!(row.preview_width, Some(400), "the preview width is stored");
    assert_eq!(row.preview_height, Some(300), "and its height");
    (key, preview_key)
}

/// Both files the upload wrote come back over the URLs it announced, as the
/// bytes and the types they were detected as.
async fn assert_both_files_serve(app: &Router, key: &str, preview_key: &str, bytes: Vec<u8>) {
    let served = common::TestRequest::get(&format!("/media/{key}"))
        .send(app)
        .await;
    common::assert_answer(&served, StatusCode::OK, "the announced url serves");
    assert_eq!(
        served.body,
        Bytes::from(bytes),
        "the original is stored as uploaded, byte for byte"
    );
    assert_eq!(
        served.header("content-type"),
        Some("image/png"),
        "as the type that was detected"
    );
    let preview = common::TestRequest::get(&format!("/media/{preview_key}"))
        .send(app)
        .await;
    common::assert_answer(&preview, StatusCode::OK, "the preview url serves");
    assert_eq!(
        preview.header("content-type"),
        Some("image/jpeg"),
        "the preview serves as its encoded type"
    );
}

/// An uploaded `PNG` answers the `Attachment` entity with its dimensions,
/// generated preview and `Blurhash`, records an unattached row for the
/// uploader, and both files come back over the announced URLs.
#[tokio::test]
async fn png_upload_answers_the_entity_and_serves_the_file() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "alice", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let fixture = png(640, 480);

    let entity = upload_file(
        &app,
        V1,
        &token,
        file_part("cat.png", "image/png", fixture.clone()),
    )
    .await;
    let id = entity_id(&entity);
    assert_attachment_entity(&entity, &id);

    let row = stored_row(&pool, &id).await;
    assert_stored_png(&row, &user, fixture.len());

    let (key, preview_key) = assert_stored_keys(&entity, &row, &user);
    assert_both_files_serve(&app, &key, &preview_key, fixture).await;
}

/// An upload whose bytes claim a `PNG` but do not decode is rejected with the
/// ticket's 422 — before any file is written and without a row.
#[tokio::test]
async fn an_undecodable_image_is_rejected() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "vicky", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    // A full PNG signature and a partial IHDR chunk: `infer` names the type,
    // no decoder gets dimensions out of it.
    let mut corrupt = png(8, 8);
    corrupt.truncate(20);
    let response = upload(
        &app,
        V1,
        &token,
        &[file_part("broken.png", "image/png", corrupt)],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an image that does not decode is a 422"
    );
    assert_eq!(
        response.text(),
        error_body("Invalid image"),
        "the 422 is the ticket's exact message"
    );
    assert_eq!(
        row_count(&pool, &user).await,
        0,
        "an undecodable image stores no row"
    );
    assert_eq!(entries_in(&dir), 0, "and nothing was written to disk");
}

/// An image over `MAX_IMAGE_PIXELS` is the ticket's 422: the header check
/// rejects it before the upload is stored, without a full decode.
#[tokio::test]
async fn an_over_pixel_image_is_rejected() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "wendy", SCOPE).await;
    let mut config = common::test_config_with_uploads_dir(dir.path());
    config.max_image_pixels = 100_000;
    let app = common::test_app_with_config(pool.clone(), config);

    let response = upload(
        &app,
        V1,
        &token,
        &[file_part("big.png", "image/png", png(640, 480))],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "640x480 is 307_200 pixels, over the configured 100_000"
    );
    assert_eq!(
        response.text(),
        error_body("Image too large"),
        "the 422 is the ticket's exact message"
    );
    assert_eq!(row_count(&pool, &user).await, 0, "no row for it");
    assert_eq!(entries_in(&dir), 0, "and no file on disk");
}

/// A transparent PNG previews as a PNG: the generated file keeps the alpha
/// instead of flattening it into a JPEG.
#[tokio::test]
async fn a_transparent_png_previews_as_png() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "xenia", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let entity = upload_file(
        &app,
        V1,
        &token,
        file_part("ghost.png", "image/png", transparent_png(800, 600)),
    )
    .await;
    let id = entity_id(&entity);
    assert_eq!(
        entity["meta"],
        json!({
            "original": original_meta(800, 600),
            "small": small_meta(800, 600),
        }),
        "the transparent image carries both dimension variants too"
    );

    let row = stored_row(&pool, &id).await;
    let key = row.storage_key.clone().expect("a local upload has a key");
    let stem = key
        .strip_suffix(".png")
        .expect("the key names its extension");
    let preview_key = storage_key(row.preview_url.as_deref().expect("a preview url"));
    assert_eq!(
        preview_key,
        format!("{stem}_small.png"),
        "a transparent source previews as a png"
    );
    let preview = common::TestRequest::get(&format!("/media/{preview_key}"))
        .send(&app)
        .await;
    assert_eq!(preview.status, StatusCode::OK, "the preview serves");
    assert_eq!(
        preview.header("content-type"),
        Some("image/png"),
        "as the type its bytes carry"
    );
    assert_eq!(user.id, row.user_id.expect("the uploader owns it"));
}

/// An animated GIF uploads like a still: the dimensions come from the
/// header's (first frame's) canvas and the preview is that first frame.
#[tokio::test]
async fn an_animated_gif_stores_first_frame_dimensions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "yves", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let entity = upload_file(
        &app,
        V1,
        &token,
        file_part("cat.gif", "image/gif", animated_gif()),
    )
    .await;
    assert_eq!(entity["type"], "image", "a gif is an image attachment");
    assert_eq!(
        entity["meta"],
        json!({
            "original": original_meta(640, 480),
            "small": small_meta(640, 480),
        }),
        "the first frame's canvas, not the second frame's"
    );
}

/// Video and audio uploads are not analyzed: no preview, no `Blurhash`, no
/// dimensions — the detection columns stay as empty as they were.
#[tokio::test]
async fn video_and_audio_keep_no_detection() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "zoe", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for (bytes, filename, mime) in [
        (MP3, "song.mp3", "audio/mpeg"),
        (MP4, "clip.mp4", "video/mp4"),
    ] {
        let entity = upload_file(&app, V1, &token, file_part(filename, mime, bytes)).await;
        let id = entity_id(&entity);
        assert_eq!(entity["meta"], json!({}), "{filename} detects nothing");
        assert_eq!(entity["blurhash"], Value::Null, "and hashes nothing");
        assert_eq!(
            entity["preview_url"],
            url_of(&entity),
            "{filename} previews as the file itself"
        );
        let row = stored_row(&pool, &id).await;
        assert!(
            row.width.is_none() && row.height.is_none() && row.blurhash.is_none(),
            "no detection columns were filled for {filename}"
        );
        assert!(row.preview_url.is_none(), "and no preview was stored");
    }
    assert_eq!(
        entries_in(&dir),
        1,
        "one namespace directory for all of it, no extra files"
    );
}

/// Every accepted kind uploads as the type its bytes carry, and the entity's
/// `type` follows it: `image`, `video` or `audio`.
#[tokio::test]
async fn each_media_kind_keeps_the_type_its_bytes_carry() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "carol", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for (bytes, filename, extension, mime, kind) in [
        (png(64, 32), "cat.png", "png", "image/png", "image"),
        (jpeg(64, 32), "cat.jpg", "jpg", "image/jpeg", "image"),
        (MP3.to_vec(), "song.mp3", "mp3", "audio/mpeg", "audio"),
        (MP4.to_vec(), "clip.mp4", "mp4", "video/mp4", "video"),
    ] {
        let entity = upload_file(&app, V1, &token, file_part(filename, mime, bytes.clone())).await;
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
        assert_eq!(served.body, Bytes::from(bytes), "{filename} is intact");
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

/// `description` and `focus` reach the row and the entity reports them
/// beside the detected dimensions.
#[tokio::test]
async fn description_and_focus_are_stored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "dave", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let parts = [
        file_part("mat.png", "image/png", png(80, 60)),
        MultipartPart::text("description", "a cat on a mat"),
        MultipartPart::text("focus", "0.25,-0.5"),
    ];
    let entity = upload(&app, V1, &token, &parts).await.json_value();
    assert_eq!(
        entity["description"], "a cat on a mat",
        "the alt text comes back"
    );
    assert_eq!(
        entity["meta"]["focus"],
        json!({ "x": 0.25, "y": -0.5 }),
        "the focal point is reported"
    );
    assert_eq!(
        entity["meta"]["original"],
        original_meta(80, 60),
        "and so are the detected dimensions"
    );

    let id = entity_id(&entity);
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
        file_part("edge.png", "image/png", png(80, 60)),
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
#[tokio::test]
async fn an_invalid_focus_is_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
            file_part("mat.png", "image/png", png(32, 32)),
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
#[tokio::test]
async fn an_empty_focus_stores_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "fred", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let blank = [
        file_part("mat.png", "image/png", png(32, 32)),
        MultipartPart::text("focus", ""),
    ];
    let entity = upload(&app, V1, &token, &blank).await.json_value();
    assert_eq!(
        entity["meta"].get("focus"),
        None,
        "an empty focus is no focus"
    );
    assert_eq!(
        entity["meta"]["original"],
        original_meta(32, 32),
        "while the detected dimensions stay reported"
    );
    let id = entity_id(&entity);
    let row = stored_row(&pool, &id).await;
    assert_eq!(row.focus_x, None, "and stores no x");
    assert_eq!(row.focus_y, None, "nor y");
    assert_eq!(
        row.user_id,
        Some(user.id),
        "the upload is still the caller's"
    );

    let absent = upload_file(
        &app,
        V1,
        &token,
        file_part("mat.png", "image/png", png(32, 32)),
    )
    .await;
    assert_eq!(absent["meta"].get("focus"), None, "and so is an absent one");
}

/// No `file` part at all is the ticket's 422, whatever else the body holds.
#[tokio::test]
async fn a_body_without_a_file_is_a_missing_parameter() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
                Bytes::from(png(16, 16)),
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
#[tokio::test]
async fn an_empty_file_is_rejected() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn a_file_that_is_not_media_is_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn the_client_header_never_decides_the_type() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "judy", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let entity = upload_file(
        &app,
        V1,
        &token,
        file_part("cat.png", "text/html", png(120, 90)),
    )
    .await;
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
#[tokio::test]
async fn an_oversize_file_is_payload_too_large() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "ken", SCOPE).await;
    let app = common::test_app_with_upload_limits(pool.clone(), dir.path(), 64);

    for path in [V1, V2] {
        let response = upload(
            &app,
            path,
            &token,
            &[file_part("big.png", "image/png", long_png_header())],
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
/// applies. A video stands in here: its bytes only need the magic header the
/// detector reads, since nothing decodes them.
#[tokio::test]
async fn a_file_over_the_extractor_default_uploads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "lana", SCOPE).await;
    let app = common::test_app_with_upload_limits(pool.clone(), dir.path(), 4_000_000);

    // The `MP4` signature, padded just past the 2 MiB axum defaults at.
    let mut big: Vec<u8> = MP4.to_vec();
    big.resize(2 * 1024 * 1024 + 1024, b'x');
    let expected = Bytes::from(big.clone());

    let entity = upload_file(&app, V1, &token, file_part("big.mp4", "video/mp4", big)).await;
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
        entity["pleroma"]["mime_type"], "video/mp4",
        "the type still came from the signature"
    );
}

/// Without a token the endpoint answers 401 and the body is never stored.
#[tokio::test]
async fn an_anonymous_upload_is_unauthorized() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for path in [V1, V2] {
        let response = common::TestRequest::post(path)
            .multipart(BOUNDARY, &[file_part("cat.png", "image/png", png(16, 16))])
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
#[tokio::test]
async fn a_token_without_the_scope_is_forbidden() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
        let response = upload(
            &app,
            V1,
            token,
            &[file_part("cat.png", "image/png", png(16, 16))],
        )
        .await;
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
        &[file_part("cat.png", "image/png", png(16, 16))],
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
#[tokio::test]
async fn the_broader_write_scope_uploads_too() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "peggy", "write").await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for path in [V1, V2] {
        let response = upload(
            &app,
            path,
            &token,
            &[file_part("cat.png", "image/png", png(16, 16))],
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
#[tokio::test]
async fn v1_and_v2_answer_the_same() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "quinn", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let fixture = png(200, 100);

    let mut entities: Vec<(String, Value)> = Vec::new();
    for path in [V1, V2] {
        let parts = [
            file_part("cat.png", "image/png", fixture.clone()),
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
    assert_ne!(
        first["preview_url"], second["preview_url"],
        "and its own generated preview"
    );

    let ids: Vec<String> = entities
        .iter()
        .map(|(_, entity)| entity_id(entity))
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
            Bytes::from(fixture.clone()),
            "and its file serves from the announced url"
        );
    }
}

/// A client-sent `thumbnail` part is accepted and ignored, whether sent as a
/// file part or as a text field: the preview the row records is the generated
/// one, keyed off the `file` part's stem, not the posted thumbnail's bytes.
#[tokio::test]
async fn a_client_thumbnail_is_ignored_for_the_generated_preview() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "rosa", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let fixture = png(64, 64);

    let parts = [
        file_part("cat.png", "image/png", fixture.clone()),
        MultipartPart::text("description", "a cat"),
        MultipartPart::file(
            "thumbnail",
            "thumb.png",
            "image/png",
            Bytes::from(jpeg(64, 64)),
        ),
        MultipartPart::text("thumbnail", "ignored"),
    ];
    let entity = upload(&app, V1, &token, &parts).await.json_value();
    assert_eq!(entity["type"], "image", "the upload went through");
    assert_eq!(
        entity["description"], "a cat",
        "the description belongs to the file"
    );
    assert_eq!(mime_of(&entity), "image/png", "and one type for one file");

    let id = entity_id(&entity);
    let row = stored_row(&pool, &id).await;
    let key = row.storage_key.clone().expect("a storage key");
    assert_eq!(
        row.content_type, "image/png",
        "the client thumbnail did not change the stored type"
    );
    let stem = key
        .strip_suffix(".png")
        .expect("the key names its extension");
    assert_eq!(
        storage_key(row.preview_url.as_deref().expect("a preview url")),
        format!("{stem}_small.jpg"),
        "the preview is generated from the file, not taken from the part"
    );
    let served = common::TestRequest::get(&format!("/media/{key}"))
        .send(&app)
        .await;
    assert_eq!(
        served.body,
        Bytes::from(fixture),
        "and what got stored is the `file` part"
    );
    assert_eq!(row_count(&pool, &user).await, 1, "one file in, one row");
    assert_eq!(
        entries_in(&dir),
        1,
        "one namespace directory holding the file and its preview"
    );
}

/// Rejected uploads leave nothing behind, and repeated uploads of the same
/// bytes never share a key.
#[tokio::test]
async fn repeated_uploads_do_not_share_a_key() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "sue", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());
    let fixture = png(16, 16);

    let mut keys: Vec<String> = Vec::new();
    for _ in 0..3 {
        let entity = upload_file(
            &app,
            V1,
            &token,
            file_part("cat.png", "image/png", fixture.clone()),
        )
        .await;
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
#[tokio::test]
async fn only_post_uploads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
