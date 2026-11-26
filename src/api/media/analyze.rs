//! The analysis an `image/*`, `video/*` or `audio/*` upload goes through
//! before anything is stored.
//!
//! `analyze_upload` runs the blocking-pool pass — decoding the header,
//! rendering the `_small` preview and computing the `Blurhash` — and
//! `detect_stored` folds what it found into the `Detection` the row
//! records, once the generated preview did (or did not) reach storage.
//! A video or audio file is not decoded: its container headers give the
//! duration and the picture size ([`media_probe`]), and its preview is the
//! client's `thumbnail` part or, for a video, a frame `ffmpeg` renders when
//! it is available (`FFMPEG_PATH`); either goes through the same preview
//! and `Blurhash` pass an image does.
//! `detect_media_type` is the magic-byte gate every stored file passes
//! through, upload and replacement thumbnail alike. The pipeline that
//! calls these is `store_upload` in the sibling `upload`; the constants
//! they read live in this module's parent.

use axum::body::Bytes;
use tokio::sync::OwnedSemaphorePermit;
use tracing::warn;

use crate::{
    error::{AppError, AppResult},
    extract::multipart::UploadedFile,
    models::media_attachment::NewMediaAttachment,
    services::{
        media_probe,
        media_processing::{self, ImageInfo, ProcessedImage, Thumbnail},
    },
    state::AppState,
};

use super::{THUMBNAIL_MAX_EDGE, UNSUPPORTED_FILE_TYPE, upload::Upload};

/// What one blocking pass over an upload's bytes found out: for an image
/// the dimensions are required, and everything else — the preview, the
/// `Blurhash`, a video's or audio file's size and duration — is an extra
/// whose absence must not cost the upload.
#[derive(Debug)]
pub(super) struct Analyzed {
    /// Dimensions read from the file's header, when it states any.
    info: Option<ImageInfo>,
    /// Length in seconds of a video or audio file that states one.
    duration: Option<f64>,
    /// The rendered preview, or why rendering it failed; `None` when none
    /// was attempted (audio, or a video without a frame to render).
    thumbnail: Option<AppResult<Thumbnail>>,
    /// `Blurhash` string; `None` when the image cannot be hashed.
    blurhash: Option<String>,
}

/// The analysis an upload records on its row, after the generated
/// preview did (or did not) reach storage.
#[derive(Debug)]
pub(super) struct Detection {
    /// Dimensions of the original file, if it stated any.
    info: Option<ImageInfo>,
    /// Length in seconds of a video or audio file, if it stated one.
    duration: Option<f64>,
    /// `Blurhash` string of the original, if one was computed.
    blurhash: Option<String>,
    /// Stored preview of the original, if one was generated and stored.
    preview: Option<StoredPreview>,
}

/// A generated preview that made it into storage.
#[derive(Debug)]
struct StoredPreview {
    /// Public URL of the stored preview file.
    url: String,
    /// Preview width in pixels.
    width: u32,
    /// Preview height in pixels.
    height: u32,
}

impl Detection {
    /// Write what the analysis found onto `new`: the original's dimensions,
    /// the `Blurhash` and the preview's URL and dimensions — the columns
    /// behind the entity's `meta.original`, `meta.small` and `blurhash`.
    pub(super) fn record(&self, new: &mut NewMediaAttachment) {
        if let Some(info) = self.info {
            new.width = column_dimension(info.width);
            new.height = column_dimension(info.height);
        }
        new.duration = self.duration;
        new.blurhash.clone_from(&self.blurhash);
        if let Some(preview) = &self.preview {
            new.preview_url = Some(preview.url.clone());
            new.preview_width = column_dimension(preview.width);
            new.preview_height = column_dimension(preview.height);
        }
    }
}

/// A detected pixel count as the `integer` column value it needs. `None`
/// (with a log line) for an edge over `i32::MAX`, which the enforced
/// `MAX_IMAGE_PIXELS` rules out unless an operator raised the limit past
/// the column's range — the row then simply keeps that part unknown.
fn column_dimension(pixels: u32) -> Option<i32> {
    match i32::try_from(pixels) {
        Ok(value) => Some(value),
        Err(err) => {
            warn!(pixels, %err, "detected dimension does not fit an integer column");
            None
        }
    }
}

/// The pixel gate of `state`'s configuration as the decoder's `u64`.
fn pixel_limit(state: &AppState) -> u64 {
    u64::try_from(state.config.max_image_pixels).unwrap_or(u64::MAX)
}

/// Runs [`media_processing::process_image`] over `bytes` on the blocking
/// pool, holding `permit` until the pass returns even if the request is
/// cancelled meanwhile.
async fn process_blocking(
    bytes: Bytes,
    max_pixels: u64,
    permit: OwnedSemaphorePermit,
) -> AppResult<ProcessedImage> {
    tokio::task::spawn_blocking(move || {
        let processed = media_processing::process_image(&bytes, max_pixels, THUMBNAIL_MAX_EDGE);
        drop(permit);
        processed
    })
    .await
    .map_err(|err| AppError::Internal(format!("image analysis task did not finish: {err}")))?
}

/// Read the upload's dimensions and, for an `image/*` type, render its
/// preview and compute its `Blurhash` — all the CPU work in one
/// [`tokio::task::spawn_blocking`] pass, so decoders never run on the
/// async runtime. The original is decoded once, and the pass starts only
/// once it holds a permit from
/// [`media_processing::decode_semaphore`], which bounds concurrent decodes
/// process-wide. A `video/*` or `audio/*` upload is probed for its duration
/// and size, and previews as the client's `thumbnail` part or, for a video
/// without one, a frame `ffmpeg` renders; any other upload analyzes to
/// `None`: there is nothing on the row for it to fill.
///
/// # Errors
///
/// [`AppError::Unprocessable`] from [`media_processing::analyze_image`] for
/// an undecodable image ("Invalid image") or one over
/// `MAX_IMAGE_PIXELS` ("Image too large"), and the same for a video or
/// audio upload's `thumbnail` part (or "Unsupported file type" when it is
/// not an image); [`AppError::Internal`] when the blocking task itself is
/// gone. Thumbnail and `Blurhash` failures — and every failure of the video
/// and audio probing and of `ffmpeg` — are not errors here: they ride along
/// inside [`Analyzed`], because losing an extra must not fail the upload.
pub(super) async fn analyze_upload(
    state: &AppState,
    upload: &Upload,
) -> AppResult<Option<Analyzed>> {
    if upload.content_type.starts_with("image/") {
        analyze_image_upload(state, upload).await.map(Some)
    } else if upload.content_type.starts_with("video/") || upload.content_type.starts_with("audio/")
    {
        analyze_timed_upload(state, upload).await.map(Some)
    } else {
        Ok(None)
    }
}

/// The image arm of [`analyze_upload`]: header gate, preview and hash.
async fn analyze_image_upload(state: &AppState, upload: &Upload) -> AppResult<Analyzed> {
    let bytes = upload.file.bytes.clone();
    let limit = pixel_limit(state);
    let processed = process_blocking(
        bytes,
        limit,
        media_processing::acquire_decode_permit(media_processing::decode_semaphore()).await?,
    )
    .await?;
    Ok(Analyzed {
        info: Some(processed.info),
        duration: None,
        thumbnail: Some(processed.thumbnail),
        blurhash: processed.blurhash,
    })
}

/// The video and audio arm of [`analyze_upload`]: the container headers'
/// size and duration, plus the preview — the client's `thumbnail` part when
/// it sent one (no decoder needed), else a video's frame rendered by
/// `ffmpeg`. Whatever could not be found is simply left unknown.
///
/// # Errors
///
/// [`AppError::Unprocessable`] from [`supplied_preview`] for a `thumbnail`
/// part that is not a usable image: the client asked for that exact
/// picture, so a bad one is refused rather than silently replaced.
async fn analyze_timed_upload(state: &AppState, upload: &Upload) -> AppResult<Analyzed> {
    let probed = media_probe::probe(&upload.file.bytes, upload.content_type);
    let processed = if let Some(part) = &upload.thumbnail {
        Some(supplied_preview(state, part).await?)
    } else if upload.content_type.starts_with("video/") {
        video_frame(state, upload, probed.duration)
            .await
            .unwrap_or_else(|err| {
                warn!(%err, "processing the video frame failed, storing no preview");
                None
            })
    } else {
        None
    };
    let (thumbnail, blurhash) = processed.map_or((None, None), |image| {
        (Some(image.thumbnail), image.blurhash)
    });
    Ok(Analyzed {
        info: probed
            .dimensions
            .map(|(width, height)| ImageInfo { width, height }),
        duration: probed.duration,
        thumbnail,
        blurhash,
    })
}

/// The client's `thumbnail` part as the preview: the [`detect_media_type`]
/// gate, then the same scaling, encoding and `Blurhash` pass an uploaded
/// image gets, so its metadata is never published and an image bomb is
/// never served.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`UNSUPPORTED_FILE_TYPE`] for bytes that
/// are not an image, "Invalid image" for one no decoder reads and "Image
/// too large" for one over `MAX_IMAGE_PIXELS`; [`AppError::Internal`] when
/// the decode gate or the blocking task is gone.
async fn supplied_preview(state: &AppState, part: &UploadedFile) -> AppResult<ProcessedImage> {
    if !detect_media_type(&part.bytes)?.starts_with("image/") {
        return Err(AppError::Unprocessable(UNSUPPORTED_FILE_TYPE.to_owned()));
    }
    let permit =
        media_processing::acquire_decode_permit(media_processing::decode_semaphore()).await?;
    process_blocking(part.bytes.clone(), pixel_limit(state), permit).await
}

/// A video's frame at [`media_probe::frame_seek`] of `duration`, scaled and
/// processed like an uploaded image; `None` when `ffmpeg` is turned off
/// (`FFMPEG_PATH=off`), unavailable or finds no frame.
///
/// # Errors
///
/// [`AppError::Internal`] when the decode gate or the blocking task is
/// gone.
async fn video_frame(
    state: &AppState,
    upload: &Upload,
    duration: Option<f64>,
) -> AppResult<Option<ProcessedImage>> {
    let Some(program) = state.config.ffmpeg_path.as_deref() else {
        return Ok(None);
    };
    let permit =
        media_processing::acquire_decode_permit(media_processing::decode_semaphore()).await?;
    let Some(frame) = media_probe::extract_frame(
        program,
        &upload.file.bytes,
        THUMBNAIL_MAX_EDGE,
        media_probe::frame_seek(duration),
    )
    .await
    else {
        return Ok(None);
    };
    process_blocking(Bytes::from(frame), pixel_limit(state), permit)
        .await
        .map(Some)
}

/// Fold [`Analyzed`] into the [`Detection`] the row records: store the
/// generated preview under `key`'s stem with a `_small` suffix, logging and
/// dropping either failure — an upload never answers 500 over a preview it
/// could not make.
pub(super) async fn detect_stored(state: &AppState, key: &str, analyzed: Analyzed) -> Detection {
    let Analyzed {
        info,
        duration,
        thumbnail,
        blurhash,
    } = analyzed;
    let preview = match thumbnail {
        Some(Ok(thumbnail)) => store_generated_preview(state, key, &thumbnail).await,
        Some(Err(err)) => {
            warn!(%err, %key, "thumbnail generation failed, storing no preview");
            None
        }
        None => None,
    };
    Detection {
        info,
        duration,
        blurhash,
        preview,
    }
}

/// Store `thumbnail` as the preview of the file kept at `key`: the same
/// stem, a `_small` suffix and the encoded format's extension
/// (`<stem>_small.jpg` / `<stem>_small.png`). A storage failure is logged
/// and answers `None`, leaving the upload without a preview.
async fn store_generated_preview(
    state: &AppState,
    key: &str,
    thumbnail: &Thumbnail,
) -> Option<StoredPreview> {
    let preview_key = preview_key(key, thumbnail.extension());
    let body = Bytes::copy_from_slice(&thumbnail.bytes);
    if let Err(err) = state
        .uploader
        .put(&preview_key, body, thumbnail.content_type)
        .await
    {
        warn!(%err, preview_key = %preview_key, "storing the thumbnail failed, serving the file without a preview");
        return None;
    }
    Some(StoredPreview {
        url: state.uploader.public_url(&preview_key),
        width: thumbnail.width,
        height: thumbnail.height,
    })
}

/// The key of the preview generated for the file stored at `key`: the same
/// stem up to the extension, a `_small` suffix and `extension`.
fn preview_key(key: &str, extension: &str) -> String {
    let (stem, _) = key.rsplit_once('.').unwrap_or((key, ""));
    format!("{stem}_small.{extension}")
}

/// The media type of an upload's bytes, from the bytes themselves: the first
/// `infer` match whose category is image, video or audio.
///
/// Not the part's `content-type`: a client may claim anything, and what is
/// stored here is what [`crate::uploads`] names the key after, what the
/// entity reports and what a later status federates.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`UNSUPPORTED_FILE_TYPE`] when nothing
/// matches the bytes or what matches is not one of the three accepted
/// categories.
pub(super) fn detect_media_type(bytes: &[u8]) -> AppResult<&'static str> {
    let Some(detected) = infer::get(bytes).filter(|kind| is_accepted(kind.matcher_type())) else {
        return Err(AppError::Unprocessable(UNSUPPORTED_FILE_TYPE.to_owned()));
    };
    Ok(detected.mime_type())
}

/// Whether an `infer` category is one of the three a media upload may be.
const fn is_accepted(category: infer::MatcherType) -> bool {
    matches!(
        category,
        infer::MatcherType::Image | infer::MatcherType::Video | infer::MatcherType::Audio
    )
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use crate::api::media::test_support::{
        EMPTY, JPEG, MP3, MP4, NOTHING, PDF, PNG, TOO_SHORT, file_part, uploader,
    };

    use super::*;

    /// The message `bytes` is rejected with, panicking when they are not.
    fn rejection(bytes: &[u8]) -> String {
        detect_media_type(bytes)
            .expect_err("the fixture is not accepted media")
            .to_string()
    }

    #[test]
    fn the_detected_type_is_the_media_type_of_the_bytes() {
        for (bytes, content_type) in [
            (PNG, "image/png"),
            (JPEG, "image/jpeg"),
            (MP3, "audio/mpeg"),
            (MP4, "video/mp4"),
        ] {
            assert_eq!(
                detect_media_type(bytes).expect("the fixture is accepted media"),
                content_type,
                "the type comes from the magic bytes"
            );
        }
    }

    #[test]
    fn anything_that_is_not_image_video_or_audio_is_rejected() {
        for bytes in [PDF, NOTHING, TOO_SHORT, EMPTY] {
            assert_eq!(
                rejection(bytes),
                UNSUPPORTED_FILE_TYPE,
                "undetectable or non-media bytes are a 422"
            );
        }
    }

    #[test]
    fn the_preview_key_shares_its_originals_stem() {
        assert_eq!(
            preview_key("u/9b3c1d0e4f5a6b7c.png", "jpg"),
            "u/9b3c1d0e4f5a6b7c_small.jpg",
            "same stem, _small, the encoded extension"
        );
        assert_eq!(
            preview_key("u/9b3c1d0e4f5a6b7c.gif", "png"),
            "u/9b3c1d0e4f5a6b7c_small.png",
            "a GIF previews as a PNG beside the same stem"
        );
        assert_eq!(
            preview_key("u/9b3c1d0e4f5a6b7c", "jpg"),
            "u/9b3c1d0e4f5a6b7c_small.jpg",
            "a key without an extension is still a stem"
        );
    }

    #[test]
    fn detection_records_the_analysis_columns() {
        let detection = Detection {
            info: Some(ImageInfo {
                width: 1024,
                height: 512,
            }),
            duration: None,
            blurhash: Some("UBLHZ;9a?wWB%%.8t7Rj".to_owned()),
            preview: Some(StoredPreview {
                url: "https://test.example/media/u/9b3c_small.jpg".to_owned(),
                width: 400,
                height: 200,
            }),
        };
        let upload = Upload {
            file: file_part("cat.png", "image/png", PNG),
            description: None,
            focus: None,
            content_type: "image/png",
            thumbnail: None,
        };
        let new = upload
            .new_attachment(
                Uuid::now_v7(),
                &uploader(),
                "u/9b3c1d0e4f5a6b7c.png",
                Some(&detection),
            )
            .expect("the fixture size is recordable");
        assert_eq!(new.width, Some(1024), "the original width is recorded");
        assert_eq!(new.height, Some(512), "and its height");
        assert_eq!(
            new.blurhash.as_deref(),
            Some("UBLHZ;9a?wWB%%.8t7Rj"),
            "the blurhash is recorded"
        );
        assert_eq!(
            new.preview_url.as_deref(),
            Some("https://test.example/media/u/9b3c_small.jpg"),
            "the preview url is recorded"
        );
        assert_eq!(new.preview_width, Some(400), "and the preview dimensions");
        assert_eq!(new.preview_height, Some(200), "both of them");

        let without_preview = Detection {
            info: detection.info,
            duration: None,
            blurhash: None,
            preview: None,
        };
        let lost = upload
            .new_attachment(
                Uuid::now_v7(),
                &uploader(),
                "u/9b3c.png",
                Some(&without_preview),
            )
            .expect("the fixture size is recordable");
        assert!(
            lost.preview_url.is_none() && lost.preview_width.is_none(),
            "a failed thumbnail leaves the preview columns unset"
        );
        assert_eq!(
            lost.width,
            Some(1024),
            "but the dimensions are still stored"
        );
        assert!(
            lost.blurhash.is_none(),
            "and a failed hash leaves blurhash unset"
        );
    }

    #[test]
    fn a_timed_detection_records_the_duration_and_the_size_it_found() {
        let upload = Upload {
            file: file_part("clip.mp4", "video/mp4", MP4),
            description: None,
            focus: None,
            content_type: "video/mp4",
            thumbnail: None,
        };
        let detection = Detection {
            info: Some(ImageInfo {
                width: 640,
                height: 360,
            }),
            duration: Some(12.5),
            blurhash: Some("UBLHZ;9a?wWB%%.8t7Rj".to_owned()),
            preview: Some(StoredPreview {
                url: "https://test.example/media/u/9b3c_small.jpg".to_owned(),
                width: 400,
                height: 225,
            }),
        };
        let new = upload
            .new_attachment(
                Uuid::now_v7(),
                &uploader(),
                "u/9b3c1d0e4f5a6b7c.mp4",
                Some(&detection),
            )
            .expect("the fixture size is recordable");
        assert_eq!(
            (new.width, new.height, new.duration),
            (Some(640), Some(360), Some(12.5)),
            "size and duration are the probe's"
        );
        assert_eq!(
            new.preview_url.as_deref(),
            Some("https://test.example/media/u/9b3c_small.jpg"),
            "the rendered frame is the preview"
        );
        assert_eq!(
            (new.preview_width, new.preview_height),
            (Some(400), Some(225)),
            "with its own size"
        );

        let audio = Detection {
            info: None,
            duration: Some(61.0),
            blurhash: None,
            preview: None,
        };
        let song = upload
            .new_attachment(Uuid::now_v7(), &uploader(), "u/9b3c.mp3", Some(&audio))
            .expect("the fixture size is recordable");
        assert_eq!(song.duration, Some(61.0), "audio keeps its length");
        assert!(
            song.width.is_none() && song.height.is_none() && song.preview_url.is_none(),
            "and no picture of any kind"
        );
    }

    #[test]
    fn an_unrecordable_dimension_is_dropped_with_a_log() {
        assert_eq!(column_dimension(400), Some(400), "a normal edge fits");
        assert_eq!(
            column_dimension(u32::MAX),
            None,
            "an edge past i32::MAX is left unknown rather than wrapped"
        );
    }
}
