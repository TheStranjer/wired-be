//! The shared fixtures of the media upload tests: sample bytes, multipart
//! builders, stored-row readers and the entity assertions.

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
pub(super) const BOUNDARY: &str = "wired-media-test-boundary";
/// The scope both endpoints demand, as Pleroma's `OAuthScopesPlug` does.
pub(super) const SCOPE: &str = "write:media";
/// The v1 upload path.
pub(super) const V1: &str = "/api/v1/media";
/// The v2 upload path: the same handler, on a second address.
pub(super) const V2: &str = "/api/v2/media";
/// Where the test instance serves stored files from.
pub(super) const MEDIA_BASE_URL: &str = "https://test.example/media";
/// The edge the upload endpoint scales generated previews down to.
pub(super) const PREVIEW_EDGE: u32 = 400;

/// An `MP3`: an `ID3v2` tag header with a zero-length tag.
pub(super) const MP3: &[u8] = b"ID3\x03\x00\x00\x00\x00\x00\x00";
/// An `MP4`: an `ftyp` box with the `mp42` brand.
pub(super) const MP4: &[u8] = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom";
/// A `PDF`: bytes with a type, just not one of the three accepted ones.
pub(super) const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";
/// Text: bytes no matcher can name at all.
pub(super) const TEXT: &[u8] = b"a plain text file, neither image, video nor audio\n";

/// The deterministic gradient color of pixel `(x, y)`, opaque.
pub(super) fn raster_pixel(x: u32, y: u32) -> Rgba<u8> {
    let channel = |value: u32| u8::try_from(value % 256).expect("a modulo of 256 fits a u8");
    Rgba([
        channel(x * 7 + y * 3),
        channel(x + y * 11),
        channel(x * y),
        255,
    ])
}

/// A deterministic `width` x `height` gradient, fully opaque.
pub(super) fn raster(width: u32, height: u32) -> RgbaImage {
    ImageBuffer::from_fn(width, height, raster_pixel)
}

/// A gradient with every other column fully transparent: alpha to keep.
pub(super) fn transparent_raster(width: u32, height: u32) -> RgbaImage {
    ImageBuffer::from_fn(width, height, |x, y| {
        let Rgba([r, g, b, _]) = raster_pixel(x, y);
        let alpha = if x % 2 == 0 { 0 } else { 255 };
        Rgba([r, g, b, alpha])
    })
}

/// The `image` fixture encoded as `format`'s bytes.
pub(super) fn write_raster(image: RgbaImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Vec::new();
    DynamicImage::ImageRgba8(image)
        .write_to(Cursor::new(&mut out), format)
        .expect("the fixture encodes");
    out
}

/// A decodable `PNG` of `width` x `height`.
pub(super) fn png(width: u32, height: u32) -> Vec<u8> {
    write_raster(raster(width, height), ImageFormat::Png)
}

/// A decodable `JPEG` of `width` x `height`.
pub(super) fn jpeg(width: u32, height: u32) -> Vec<u8> {
    write_raster(raster(width, height), ImageFormat::Jpeg)
}

/// A decodable, transparent `PNG` of `width` x `height`.
pub(super) fn transparent_png(width: u32, height: u32) -> Vec<u8> {
    write_raster(transparent_raster(width, height), ImageFormat::Png)
}

/// A two-frame animated `GIF`: `first` sized, then a smaller second frame.
/// The encoder writes the trailer on drop.
pub(super) fn animated_gif() -> Vec<u8> {
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
pub(super) fn original_meta(width: u32, height: u32) -> Value {
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
pub(super) fn small_meta(width: u32, height: u32) -> Value {
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
pub(super) fn uploads_dir() -> TempDir {
    TempDir::new().expect("temp dir for uploads")
}

/// The exact body of a JSON error response.
pub(super) fn error_body(message: &str) -> String {
    format!(r#"{{"error":"{message}"}}"#)
}

/// A local user named `nickname` with a token granting `scopes`: both, since
/// one assertion needs the id and every request needs the token.
///
/// # Panics
///
/// If a fixture fails to insert.
pub(super) async fn uploader(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, scopes).await;
    (user, token)
}

/// The `file` part of an upload: `bytes` uploaded as `filename` and announced
/// by the client as `content_type`.
pub(super) fn file_part<B: Into<Bytes>>(
    filename: &'static str,
    content_type: &'static str,
    bytes: B,
) -> MultipartPart {
    MultipartPart::file("file", filename, content_type, bytes.into())
}

/// POSTs `parts` to `path` with `token` as the bearer credential.
pub(super) async fn upload(
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
pub(super) async fn upload_file(
    app: &Router,
    path: &str,
    token: &str,
    file: MultipartPart,
) -> Value {
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
pub(super) async fn stored_row(pool: &PgPool, id: &str) -> media_attachment::MediaAttachment {
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
pub(super) fn entity_id(entity: &Value) -> String {
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
pub(super) fn storage_key(url: &str) -> String {
    url.strip_prefix(&format!("{MEDIA_BASE_URL}/"))
        .unwrap_or_else(|| panic!("{url} is not served from {MEDIA_BASE_URL}"))
        .to_owned()
}

/// The extension of a storage key: the part after the last dot.
///
/// # Panics
///
/// If the key has no dot, which a generated key always has.
pub(super) fn key_extension(key: &str) -> &str {
    let (_, extension) = key
        .rsplit_once('.')
        .unwrap_or_else(|| panic!("{key} is not a named key"));
    extension
}

/// The `url` of an entity as a value, for comparing against a computed URL.
pub(super) fn url_of(entity: &Value) -> String {
    entity["url"]
        .as_str()
        .expect("the entity carries a url")
        .to_owned()
}

/// The `MIME` type an upload's entity reports in its `pleroma` block.
pub(super) fn mime_of(entity: &Value) -> String {
    entity["pleroma"]["mime_type"]
        .as_str()
        .expect("the pleroma block names the mime type")
        .to_owned()
}

/// The rows uploaded by `user`, counted straight from the table.
pub(super) async fn row_count(pool: &PgPool, user: &User) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_attachments WHERE user_id = $1")
        .bind(user.id)
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// A `PNG` header padded to 80 bytes, for the test that caps one file lower.
/// The 413 comes from the body limit, so these bytes never reach a decoder.
pub(super) fn long_png_header() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01".to_vec();
    bytes.resize(80, b'x');
    bytes
}

/// The entries directly inside an uploads directory.
///
/// # Panics
///
/// If the directory cannot be read.
pub(super) fn entries_in(dir: &TempDir) -> usize {
    std::fs::read_dir(dir.path())
        .expect("the uploads directory is readable")
        .count()
}

/// The rendered `Attachment` entity of a fresh `PNG` upload: its type, the
/// fields a local unattached upload leaves empty, the blurhash and both
/// dimension variants of `meta`.
pub(super) fn assert_attachment_entity(entity: &Value, id: &str) {
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
pub(super) fn assert_stored_png(
    row: &media_attachment::MediaAttachment,
    owner: &User,
    bytes: usize,
) {
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
pub(super) fn assert_stored_keys(
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
pub(super) async fn assert_both_files_serve(
    app: &Router,
    key: &str,
    preview_key: &str,
    bytes: Vec<u8>,
) {
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
