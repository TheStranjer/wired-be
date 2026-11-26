//! Uploaded video and audio: the duration and picture size the container
//! headers state reach the row and the entity's `meta`, and the entity's
//! `preview_url` is never the media file.
//!
//! The fixtures are hand-built containers a few dozen bytes long — an `MP4`
//! with a `moov` box and an `MP3` of silent frames — whose headers are
//! valid and whose media is not decodable, which is all header probing
//! reads. A video frame for the preview needs a real decoder
//! (`ffmpeg`, an optional runtime dependency), so no test here depends on
//! one being installed.

use axum::{body::Bytes, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{config::TEST_BASE_URL, federation::publisher, models::follow};

use super::support::{
    PDF, SCOPE, V1, V2, entries_in, error_body, file_part, jpeg, key_extension, png, row_count,
    storage_key, stored_row, transparent_png, upload, upload_file, uploader, uploads_dir,
};
use crate::common::{self, MultipartPart};

/// One `ISO BMFF` box: size, type, payload.
fn mp4_box(kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let size = u32::try_from(payload.len() + 8).expect("a fixture box is small");
    let mut out = size.to_be_bytes().to_vec();
    out.extend_from_slice(&kind);
    out.extend_from_slice(payload);
    out
}

/// A version-0 `tkhd` for a `width` x `height` track (16.16 fixed point).
fn tkhd(width: u32, height: u32) -> Vec<u8> {
    let mut payload = vec![0; 84];
    payload[76..80].copy_from_slice(&(width << 16).to_be_bytes());
    payload[80..84].copy_from_slice(&(height << 16).to_be_bytes());
    mp4_box(*b"tkhd", &payload)
}

/// An `MP4` that is 640x360 and `seconds` long by its headers: `ftyp`, an
/// `mdat` of nothing, and a `moov` with a movie header and one track.
fn mp4(seconds: u32) -> Vec<u8> {
    let mut mvhd = vec![0; 100];
    mvhd[12..16].copy_from_slice(&1000_u32.to_be_bytes());
    mvhd[16..20].copy_from_slice(&(seconds * 1000).to_be_bytes());
    let mut moov = mp4_box(*b"mvhd", &mvhd);
    moov.extend(mp4_box(*b"trak", &tkhd(640, 360)));
    let mut file = mp4_box(*b"ftyp", b"mp42\x00\x00\x00\x00mp42isom");
    file.extend(mp4_box(*b"mdat", &[0; 16]));
    file.extend(mp4_box(*b"moov", &moov));
    file
}

/// An `MP3`: an empty `ID3v2` tag and 100 constant-bitrate frames (MPEG-1
/// Layer III, 128 kbit/s, 44.1 kHz, 417 bytes each): 2.60625 seconds.
fn mp3() -> Vec<u8> {
    let mut file = b"ID3\x03\x00\x00\x00\x00\x00\x00".to_vec();
    for _ in 0..100 {
        let mut frame = vec![0; 417];
        frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        file.extend(frame);
    }
    file
}

/// An uploaded `MP4` states its duration and size in `meta.original`, and
/// with no preview frame the entity's `preview_url` is `null`.
#[tokio::test]
async fn a_video_upload_records_what_its_headers_state() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "vera", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let entity = upload_file(&app, V1, &token, file_part("clip.mp4", "video/mp4", mp4(3))).await;
    assert_eq!(entity["type"], "video", "an mp4 is a video");
    assert_eq!(
        entity["meta"]["original"],
        json!({
            "width": 640,
            "height": 360,
            "size": "640x360",
            "aspect": 640.0 / 360.0,
            "duration": 3.0,
        }),
        "the player gets its size, aspect and length"
    );
    assert_eq!(
        entity["preview_url"],
        Value::Null,
        "never the .mp4 as an image"
    );

    let row = stored_row(&pool, &common::id_of(&entity)).await;
    assert_eq!(
        (row.width, row.height, row.duration),
        (Some(640), Some(360), Some(3.0)),
        "the detection columns hold the probe"
    );
}

/// An uploaded `MP3` states its duration and no size.
#[tokio::test]
async fn an_audio_upload_records_its_duration() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "aria", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let entity = upload_file(&app, V1, &token, file_part("tone.mp3", "audio/mpeg", mp3())).await;
    assert_eq!(entity["type"], "audio", "an mp3 is audio");
    let original = entity["meta"]["original"]
        .as_object()
        .expect("the length is known, so there is an original");
    assert_eq!(
        original.keys().map(String::as_str).collect::<Vec<_>>(),
        ["duration"],
        "audio states a duration and nothing else"
    );
    let duration = original["duration"].as_f64().expect("a number of seconds");
    assert!(
        (duration - 2.60625).abs() < 1e-6,
        "41700 bytes at 128 kbit/s: {duration}"
    );
    assert_eq!(entity["preview_url"], Value::Null, "no cover, no preview");
    assert_eq!(entity["blurhash"], Value::Null, "and nothing to hash");

    let row = stored_row(&pool, &common::id_of(&entity)).await;
    assert!(
        row.width.is_none() && row.preview_url.is_none(),
        "the row has no picture columns"
    );
}

/// The `thumbnail` file part of an upload: `bytes` sent as `filename`.
fn thumbnail_part(
    filename: &'static str,
    content_type: &'static str,
    bytes: Vec<u8>,
) -> MultipartPart {
    MultipartPart::file("thumbnail", filename, content_type, Bytes::from(bytes))
}

/// A federating local user with a remote follower and a token that may
/// upload and post, so a status of theirs queues its `Create`.
async fn federating_uploader(pool: &PgPool, nickname: &str) -> String {
    let user = common::create_federating_user_with(pool, nickname, |new| new).await;
    let token = common::create_app_and_token(pool, &user, "write:media write:statuses").await;
    let fan = common::create_remote_user(pool, &format!("{nickname}-fan"), "peer.example").await;
    follow::follow(pool, TEST_BASE_URL, &fan, &user)
        .await
        .map(drop)
        .expect("the fixture follow records");
    token
}

/// Posts `media_id` as a public status of `token`'s user and returns the
/// first `attachment` object of the `Create` that queued for delivery.
async fn federated_attachment(
    app: &axum::Router,
    pool: &PgPool,
    token: &str,
    media_id: &str,
) -> Value {
    let response = common::TestRequest::post("/api/v1/statuses")
        .bearer(token)
        .json(&json!({"status": "a clip", "media_ids": [media_id]}))
        .send(app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the status posts");
    let queued = sqlx::query_as::<_, (Value,)>(
        "SELECT args->'activity' FROM jobs WHERE kind = $1 ORDER BY inserted_at, id",
    )
    .bind(publisher::KIND_PUBLISH)
    .fetch_all(pool)
    .await
    .expect("the publish jobs read");
    let (create,) = queued
        .into_iter()
        .find(|(activity,)| activity["type"] == "Create")
        .expect("the post queued its Create");
    create["object"]["attachment"][0].clone()
}

/// A `thumbnail` part on a video upload is its preview: the entity's
/// `preview_url` is that picture (scaled, as a JPEG) with a blurhash and its
/// own `meta.small`, the file is served, and the federated `Document`
/// carries it as an `icon`.
#[tokio::test]
async fn a_thumbnail_part_is_a_videos_preview_and_its_federated_icon() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let token = federating_uploader(&pool, "thea").await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let parts = [
        file_part("clip.mp4", "video/mp4", mp4(3)),
        thumbnail_part("cover.jpg", "image/jpeg", jpeg(800, 600)),
    ];
    let response = upload(&app, V2, &token, &parts).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    let entity = response.json_value();
    let preview = entity["preview_url"]
        .as_str()
        .expect("the client's thumbnail is the preview");
    assert_ne!(preview, entity["url"], "the preview is not the video");
    let key = storage_key(preview);
    assert_eq!(
        key_extension(&key),
        "jpg",
        "an opaque picture stores as JPEG"
    );
    assert!(
        key.ends_with("_small.jpg"),
        "it sits beside the video under the preview naming: {key}"
    );
    assert!(dir.path().join(&key).is_file(), "and the file is on disk");
    assert!(
        entity["blurhash"]
            .as_str()
            .is_some_and(|hash| !hash.is_empty()),
        "a blurhash is computed from it: {}",
        entity["blurhash"]
    );
    assert_eq!(
        entity["meta"]["small"]["width"],
        json!(400),
        "the preview is scaled to the image path's preview size"
    );
    assert_eq!(
        entity["meta"]["original"]["duration"],
        json!(3.0),
        "the video keeps its own duration"
    );

    let row = stored_row(&pool, &common::id_of(&entity)).await;
    assert_eq!(
        row.preview_url.as_deref(),
        Some(preview),
        "the row stores it"
    );

    let attachment = federated_attachment(&app, &pool, &token, &common::id_of(&entity)).await;
    assert_eq!(attachment["type"], "Document", "a video is a Document");
    assert_eq!(
        attachment["icon"],
        json!({"type": "Image", "mediaType": "image/jpeg", "url": preview}),
        "the preview federates as the icon"
    );
}

/// Audio takes a `thumbnail` too (cover art), and a transparent one keeps
/// its alpha as a PNG whose `mediaType` the icon names.
#[tokio::test]
async fn a_thumbnail_part_is_audio_cover_art() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let token = federating_uploader(&pool, "arlo").await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let parts = [
        file_part("tone.mp3", "audio/mpeg", mp3()),
        thumbnail_part("cover.png", "image/png", transparent_png(100, 100)),
    ];
    let entity = upload(&app, V1, &token, &parts).await.json_value();
    let preview = entity["preview_url"]
        .as_str()
        .expect("audio previews as its cover");
    assert_eq!(
        key_extension(&storage_key(preview)),
        "png",
        "a translucent cover stays a PNG"
    );

    let attachment = federated_attachment(&app, &pool, &token, &common::id_of(&entity)).await;
    assert_eq!(
        attachment["icon"],
        json!({"type": "Image", "mediaType": "image/png", "url": preview}),
        "the icon names the stored type"
    );
}

/// A video without a preview — no `thumbnail` part and `ffmpeg` off —
/// answers `preview_url: null` and federates no `icon`.
#[tokio::test]
async fn a_video_without_a_preview_has_no_icon() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let token = federating_uploader(&pool, "nora").await;
    let mut config = common::test_config_with_uploads_dir(dir.path());
    config.ffmpeg_path = None;
    let app = common::test_app_with_config(pool.clone(), config);

    let entity = upload_file(&app, V1, &token, file_part("clip.mp4", "video/mp4", mp4(3))).await;
    assert_eq!(
        entity["preview_url"],
        Value::Null,
        "nothing to preview with"
    );
    assert_eq!(entity["blurhash"], Value::Null, "or to hash");

    let attachment = federated_attachment(&app, &pool, &token, &common::id_of(&entity)).await;
    assert!(
        attachment.get("icon").is_none(),
        "no preview, no icon: {attachment}"
    );
}

/// A `thumbnail` part that is not a usable image is refused like an
/// uploaded one, and nothing is stored.
#[tokio::test]
async fn a_bad_thumbnail_refuses_the_upload() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (user, token) = uploader(&pool, "bram", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    for (label, bytes, message) in [
        ("a document", PDF.to_vec(), "Unsupported file type"),
        ("an audio file", mp3(), "Unsupported file type"),
        (
            "a truncated image",
            png(8, 8)[..24].to_vec(),
            "Invalid image",
        ),
    ] {
        let parts = [
            file_part("clip.mp4", "video/mp4", mp4(3)),
            thumbnail_part("cover.bin", "application/octet-stream", bytes),
        ];
        let response = upload(&app, V1, &token, &parts).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{label} is no thumbnail"
        );
        assert_eq!(
            response.text(),
            error_body(message),
            "{label}: the 422 body"
        );
    }
    assert_eq!(row_count(&pool, &user).await, 0, "no row for any of them");
    assert_eq!(entries_in(&dir), 0, "and no file on disk");
}

/// An image upload's preview stays generated from the image itself: a
/// `thumbnail` part beside it is ignored.
#[tokio::test]
async fn an_image_upload_ignores_a_thumbnail_part() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (_user, token) = uploader(&pool, "ivy", SCOPE).await;
    let app = common::test_app_with_uploads_dir(pool.clone(), dir.path());

    let parts = [
        file_part("cat.png", "image/png", png(640, 480)),
        thumbnail_part("other.jpg", "image/jpeg", jpeg(100, 100)),
    ];
    let entity = upload(&app, V1, &token, &parts).await.json_value();
    assert_eq!(
        entity["meta"]["small"]["width"],
        json!(400),
        "the preview is the 640x480 image scaled, not the 100x100 part"
    );
    assert_eq!(entity["meta"]["small"]["height"], json!(300), "likewise");
}
