//! The image processing a replacement media preview goes through: a
//! `thumbnail` part of `PUT /api/v1/media/{id}` is decoded, held to
//! `MAX_IMAGE_PIXELS` and encoded again before it is stored, exactly as a
//! file posted to `POST /api/v1/media` is. A preview with `EXIF` GPS data is
//! published without it, an oversized or undecodable one is a 422 that stores
//! nothing.

#![cfg(test)]

use axum::{body::Bytes, http::StatusCode};
use image::ImageFormat;
use sqlx::PgPool;
use tempfile::TempDir;
use wired_be::models::{
    media_attachment::{self, MediaAttachment as Row, NewMediaAttachment},
    user::User,
};

use crate::common::{self, MultipartPart, images};

/// Boundary the multipart bodies are framed with.
const BOUNDARY: &str = "wired-preview-processing-boundary";
/// The instance origin the fixtures serve from.
const MEDIA_BASE_URL: &str = "https://test.example/media";

/// A local user and a `write:media` token.
async fn writer(pool: &PgPool) -> (User, String) {
    let user = common::create_local_user(pool, "alice").await;
    let token = common::create_app_and_token(pool, &user, "write:media").await;
    (user, token)
}

/// An unattached upload of `owner`, stored straight through the model.
///
/// # Panics
///
/// If the fixture insert fails.
async fn insert_media(pool: &PgPool, owner: &User) -> Row {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(owner.id),
            url: format!("{MEDIA_BASE_URL}/{}/orig.png", owner.id),
            storage_key: Some(format!("{}/orig.png", owner.id)),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("media fixture inserts")
}

/// `PUT`s `bytes` as the `thumbnail` of `row`.
async fn put_thumbnail(
    app: &axum::Router,
    token: &str,
    row: &Row,
    bytes: &[u8],
) -> common::TestResponse {
    common::TestRequest::put(&format!("/api/v1/media/{}", row.id))
        .bearer(token)
        .multipart(
            BOUNDARY,
            &[MultipartPart::file(
                "thumbnail",
                "thumb.jpg",
                "image/jpeg",
                Bytes::copy_from_slice(bytes),
            )],
        )
        .send(app)
        .await
}

/// The app over `dir` with `max_image_pixels` as the pixel cap.
fn app_capped_at(pool: &PgPool, dir: &TempDir, max_image_pixels: usize) -> axum::Router {
    let mut config = common::test_config_with_uploads_dir(dir.path());
    config.max_image_pixels = max_image_pixels;
    common::test_app_with_config(pool.clone(), config)
}

/// The number of files under `dir`, recursively.
fn stored_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .map(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    stored_files(&path)
                } else {
                    1
                }
            })
            .sum()
    })
}

/// A preview that carries `EXIF` GPS coordinates is stored without them, and
/// the file the media route serves holds the pixels and nothing else.
#[tokio::test]
async fn a_replacement_preview_loses_its_exif_gps_data() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = TempDir::new().expect("temp dir for uploads");
    let (alice, token) = writer(&pool).await;
    let app = app_capped_at(&pool, &dir, 40_000_000);
    let row = insert_media(&pool, &alice).await;
    let tagged = images::with_exif_gps(&images::jpeg());
    assert!(
        images::contains(&tagged, images::GPS_MARKER),
        "the fixture carries the coordinates"
    );

    let response = put_thumbnail(&app, &token, &row, &tagged).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    let entity = response.json_value();
    let preview = entity["preview_url"]
        .as_str()
        .expect("the entity announces the preview");
    let key = preview
        .strip_prefix(&format!("{MEDIA_BASE_URL}/"))
        .expect("the preview is served from the media base");

    let served = common::TestRequest::get(&format!("/media/{key}"))
        .send(&app)
        .await;
    assert_eq!(served.status, StatusCode::OK, "the preview serves");
    assert!(
        !images::contains(served.body.as_ref(), images::GPS_MARKER)
            && !images::contains(served.body.as_ref(), b"Exif"),
        "no EXIF is published"
    );
    assert_eq!(
        images::dimensions(served.body.as_ref()),
        images::dimensions(&tagged),
        "the pixels are what was sent"
    );
    assert_eq!(
        served.header("content-type"),
        Some("image/jpeg"),
        "under the type of the re-encoding"
    );
}

/// A preview over `MAX_IMAGE_PIXELS` is the upload endpoint's 422 "Image too
/// large", and neither the file nor the row changes; one exactly at the cap
/// is stored.
#[tokio::test]
async fn an_oversized_replacement_preview_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = TempDir::new().expect("temp dir for uploads");
    let (alice, token) = writer(&pool).await;
    let row = insert_media(&pool, &alice).await;
    let hundred = images::still(10, 10, ImageFormat::Png);

    let app = app_capped_at(&pool, &dir, 99);
    let response = put_thumbnail(&app, &token, &row, &hundred).await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "100 pixels pass a cap of 99"
    );
    assert_eq!(
        response.text(),
        r#"{"error":"Image too large"}"#,
        "with the upload endpoint's wording"
    );
    assert_eq!(stored_files(dir.path()), 0, "nothing was written");
    let unchanged = media_attachment::get_by_id(&pool, row.id)
        .await
        .expect("the row reads")
        .expect("the row exists");
    assert!(unchanged.preview_url.is_none(), "no preview was recorded");

    let at_cap = app_capped_at(&pool, &dir, 100);
    let accepted = put_thumbnail(&at_cap, &token, &row, &hundred).await;
    assert_eq!(accepted.status, StatusCode::OK, "exactly the cap fits");
    assert_eq!(stored_files(dir.path()), 1, "and is stored");
}

/// Magic bytes with no image behind them are the upload endpoint's 422
/// "Invalid image", and nothing is stored.
#[tokio::test]
async fn an_undecodable_replacement_preview_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = TempDir::new().expect("temp dir for uploads");
    let (alice, token) = writer(&pool).await;
    let app = app_capped_at(&pool, &dir, 40_000_000);
    let row = insert_media(&pool, &alice).await;

    let response = put_thumbnail(
        &app,
        &token,
        &row,
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01",
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a truncated PNG is no preview"
    );
    assert_eq!(
        response.text(),
        r#"{"error":"Invalid image"}"#,
        "with the upload endpoint's wording"
    );
    assert_eq!(stored_files(dir.path()), 0, "nothing was written");
}
