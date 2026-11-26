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

use serde_json::{Value, json};

use super::support::{SCOPE, V1, file_part, stored_row, upload_file, uploader, uploads_dir};
use crate::common;

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
