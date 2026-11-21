//! Uploaded images, video and audio: the entity, the stored row, the
//! generated preview and the type the bytes carry.

use axum::{body::Bytes, http::StatusCode};
use serde_json::{Value, json};

use super::support::{
    MP3, MP4, SCOPE, V1, animated_gif, assert_attachment_entity, assert_both_files_serve,
    assert_stored_keys, assert_stored_png, entity_id, entries_in, error_body, file_part, jpeg,
    key_extension, mime_of, original_meta, png, row_count, small_meta, storage_key, stored_row,
    transparent_png, upload, upload_file, uploader, uploads_dir, url_of,
};
use crate::common;
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
