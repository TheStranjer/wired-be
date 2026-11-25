//! Image analysis for media uploads: dimensions, thumbnails, `Blurhash`.
//!
//! The reference is Pleroma's upload pipeline: `Mogrify`
//! ([`lib/pleroma/upload/filter/mogrify.ex`]) scales an uploaded image
//! down and stores it as the attachment's `small` preview, and
//! `StatusView.attachment`
//! ([`lib/pleroma/web/mastodon_api/views/status_view.ex`]) renders the
//! `meta.original` / `meta.small` dimension objects these functions feed.
//! Pleroma computes the `Blurhash` string over a downscaled copy of the
//! image with its `blurhash` dependency; [`process_image`] does the same
//! with the Rust `blurhash` crate, over the already-scaled thumbnail so the
//! original is decoded once.
//!
//! These functions are pure CPU work over the bytes an upload carried;
//! callers take a permit from [`decode_semaphore`] (bounding how many
//! decodes run at once) and run them through
//! [`tokio::task::spawn_blocking`] (see [`crate::api::media`]) and decide what a failure costs: an undecodable
//! or oversized image rejects the upload (their errors are the ticket's
//! fixed 422 bodies), while a thumbnail or `Blurhash` that cannot be
//! produced only costs the attachment that extra.
//!
//! Animated inputs follow what Pleroma's filters do: `image`'s decoder
//! returns the first frame, so thumbnails and hashes describe that frame
//! while the animation itself is served untouched.
//!
//! [`lib/pleroma/upload/filter/mogrify.ex`]: https://github.com/pleroma/pleroma
//! [`lib/pleroma/web/mastodon_api/views/status_view.ex`]: https://github.com/pleroma/pleroma

use std::{
    fmt,
    io::Cursor,
    num::NonZeroUsize,
    sync::{Arc, OnceLock},
    thread::available_parallelism,
};

use image::{
    DynamicImage, ExtendedColorType, ImageFormat, ImageReader, Limits, codecs::jpeg::JpegEncoder,
    imageops::FilterType,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::debug;

// The resize/query/encode helpers below reach these traits' methods only.
use image::{GenericImageView as _, ImageEncoder as _, Pixel as _};

use crate::error::{AppError, AppResult};

mod normalize;

pub use normalize::{NormalizedImage, normalize_image, normalize_upload};

/// 422 body for bytes whose content type claims an image but which no
/// decoder can read dimensions from, the wording the ticket fixes.
pub const INVALID_IMAGE: &str = "Invalid image";
/// 422 body for a decodable image whose pixel count passes the configured
/// limit, likewise.
pub const IMAGE_TOO_LARGE: &str = "Image too large";
/// JPEG encoding quality for opaque thumbnails (the ticket's q80).
const THUMBNAIL_JPEG_QUALITY: u8 = 80;
/// Edge the copy fed to the `Blurhash` encoder is downscaled to, in pixels.
const BLURHASH_EDGE: u32 = 32;
/// Most memory, in bytes, a decoder may allocate at once for one image:
/// 256 MiB, which holds a 40-megapixel `RGBA8` frame (160 MB) with room to
/// spare while refusing a lying header that asks for more.
const DECODE_MAX_ALLOC: u64 = 256 * 1024 * 1024;
/// Horizontal `Blurhash` components of the 4x3 the ticket fixes.
const BLURHASH_COMPONENTS_X: u32 = 4;
/// Vertical `Blurhash` components of the 4x3 the ticket fixes.
const BLURHASH_COMPONENTS_Y: u32 = 3;

/// The pixel dimensions of a decodable image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// An encoded thumbnail preview of an uploaded image, ready to be stored
/// under the original key's stem with a `_small` suffix.
#[derive(Clone, Debug)]
pub struct Thumbnail {
    /// The encoded bytes: `JPEG` when the image is opaque, `PNG` when it
    /// has transparency to keep.
    pub bytes: Vec<u8>,
    /// `MIME` type [`bytes`](Self::bytes) is encoded as.
    pub content_type: &'static str,
    /// Thumbnail width in pixels.
    pub width: u32,
    /// Thumbnail height in pixels.
    pub height: u32,
}

impl Thumbnail {
    /// The filename extension [`content_type`](Self::content_type) is
    /// stored under: `jpg` for the opaque `JPEG`, `png` for the
    /// transparent `PNG`.
    #[must_use]
    pub fn extension(&self) -> &'static str {
        match self.content_type {
            "image/jpeg" => "jpg",
            _ => "png",
        }
    }
}

/// Reads an image's dimensions, rejecting bytes that are not a decodable
/// image and images larger than `max_pixels`.
///
/// The dimensions come from [`image::ImageReader::into_dimensions`] after
/// [`image::ImageReader::with_guessed_format`], which reads only the
/// format header — never the pixel data — so a monster image is rejected
/// without ever being decoded in full. `max_pixels` is the instance's
/// `MAX_IMAGE_PIXELS` setting
/// ([`Config::max_image_pixels`](crate::config::Config::max_image_pixels))
/// passed in by the caller: this stays a pure function so it can run on
/// the blocking pool and be tested against any limit.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_IMAGE`] when the bytes name
/// no readable image format or report a zero edge, and with
/// [`IMAGE_TOO_LARGE`] when `width * height` exceeds `max_pixels`.
pub fn analyze_image(bytes: &[u8], max_pixels: u64) -> AppResult<ImageInfo> {
    let (width, height) = read_dimensions(bytes, "analyzing")?;
    if width == 0 || height == 0 {
        return Err(undecodable_message());
    }
    if u64::from(width) * u64::from(height) > max_pixels {
        return Err(too_large(width, height));
    }
    Ok(ImageInfo { width, height })
}

/// Everything one upload's image pass produces: the dimensions are
/// required, the thumbnail and the `Blurhash` are the extras whose failure
/// must not cost the upload.
#[derive(Debug)]
pub struct ProcessedImage {
    /// Dimensions read from the file's header.
    pub info: ImageInfo,
    /// The encoded thumbnail, or why producing it failed.
    pub thumbnail: AppResult<Thumbnail>,
    /// `Blurhash` string; `None` when the image cannot be hashed.
    pub blurhash: Option<String>,
}

/// Analyzes an image, then decodes it exactly once to build its thumbnail
/// and its `Blurhash`.
///
/// The header gate [`analyze_image`] runs first, so an image over
/// `max_pixels` is rejected before any pixel is decoded. The thumbnail is
/// the decoded image scaled down to fit within `max_edge` pixels on both
/// edges, keeping the aspect ratio and never upscaling, encoded as `JPEG`
/// at quality 80 — or `PNG` when any pixel of the (first frame of the)
/// image is less than fully opaque, because `JPEG` has no room for
/// transparency. The `Blurhash` (4x3 components) is computed over that
/// already-scaled image, scaled further to at most 32 pixels per edge, so
/// the full-size original is never decoded or resized a second time and is
/// dropped as soon as the thumbnail-sized copy exists.
///
/// A failure after the gate is not an error: an image that analyzes but
/// cannot be decoded or encoded yields `Err` in
/// [`thumbnail`](ProcessedImage::thumbnail) and no hash, and a hash the
/// encoder rejects (an image too small for the component grid) is `None`.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_IMAGE`] when the bytes name
/// no readable image format or report a zero edge, and with
/// [`IMAGE_TOO_LARGE`] when `width * height` exceeds `max_pixels`.
pub fn process_image(bytes: &[u8], max_pixels: u64, max_edge: u32) -> AppResult<ProcessedImage> {
    let info = analyze_image(bytes, max_pixels)?;
    let image = match decode(bytes, max_pixels, "processing") {
        Ok(image) => image,
        Err(err) => {
            return Ok(ProcessedImage {
                info,
                thumbnail: Err(err),
                blurhash: None,
            });
        }
    };
    let resized = downscale(image, max_edge);
    let thumbnail = thumbnail_of(&resized);
    let blurhash = blurhash_of(&downscale(resized, BLURHASH_EDGE));
    Ok(ProcessedImage {
        info,
        thumbnail,
        blurhash,
    })
}

/// Encodes an already-scaled image as the thumbnail record.
fn thumbnail_of(resized: &DynamicImage) -> AppResult<Thumbnail> {
    let (width, height) = resized.dimensions();
    let (encoded, content_type) = encode_thumbnail(resized)?;
    Ok(Thumbnail {
        bytes: encoded,
        content_type,
        width,
        height,
    })
}

/// Scales `image` down to fit within `max_edge` on both edges, keeping the
/// aspect ratio — or hands back the original when it already fits, so a
/// thumbnail is never an upscale. Consumes the input so a large original is
/// freed as soon as its scaled copy exists.
fn downscale(image: DynamicImage, max_edge: u32) -> DynamicImage {
    if image.width() <= max_edge && image.height() <= max_edge {
        image
    } else {
        image.resize(max_edge, max_edge, FilterType::Lanczos3)
    }
}

/// The [`Blurhash`] string of a small image: 4x3 components. `None` when the
/// encoder rejects it (an image too small for the component grid).
///
/// [`Blurhash`]: https://blurha.sh
fn blurhash_of(small: &DynamicImage) -> Option<String> {
    let rgba = small.to_rgba8();
    let (width, height) = rgba.dimensions();
    blurhash::encode(
        BLURHASH_COMPONENTS_X,
        BLURHASH_COMPONENTS_Y,
        width,
        height,
        rgba.as_raw(),
    )
    .ok()
}

/// The process-wide gate on concurrent image decoding: one permit per
/// available CPU (at least one), created on first use.
///
/// Every decode holds a full-size frame in memory, and the blocking pool
/// would otherwise run as many at once as there are uploads.
#[must_use]
pub fn decode_semaphore() -> &'static Arc<Semaphore> {
    static GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    GATE.get_or_init(|| {
        let permits = available_parallelism().map_or(1, NonZeroUsize::get);
        Arc::new(Semaphore::new(permits))
    })
}

/// Waits for a permit from `semaphore` and returns it; the caller holds it
/// for as long as its decode runs and releases it by dropping it.
///
/// # Errors
///
/// [`AppError::Internal`] when the semaphore has been closed, which the
/// process-wide [`decode_semaphore`] never is.
pub async fn acquire_decode_permit(semaphore: &Arc<Semaphore>) -> AppResult<OwnedSemaphorePermit> {
    Arc::clone(semaphore)
        .acquire_owned()
        .await
        .map_err(|err| AppError::Internal(format!("image decode gate closed: {err}")))
}

/// Reads an image's `(width, height)` from its header alone.
fn read_dimensions(bytes: &[u8], context: &str) -> AppResult<(u32, u32)> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|err| undecodable(context, "guessing the format", &err))?;
    reader
        .into_dimensions()
        .map_err(|err| undecodable(context, "reading the dimensions", &err))
}

/// Decoder limits for an image the pixel gate let through: no edge longer
/// than `max_pixels` and at most [`DECODE_MAX_ALLOC`] bytes allocated, so a
/// header that lies about its size cannot make the decoder allocate more
/// than the gate allowed.
fn decode_limits(max_pixels: u64) -> Limits {
    let max_edge = u32::try_from(max_pixels).unwrap_or(u32::MAX);
    let mut limits = Limits::no_limits();
    limits.max_image_width = Some(max_edge);
    limits.max_image_height = Some(max_edge);
    limits.max_alloc = Some(DECODE_MAX_ALLOC);
    limits
}

/// Decodes an image's full pixel data — its first frame, for animated
/// formats — after guessing the format from the bytes, under
/// [`decode_limits`] for `max_pixels`.
fn decode(bytes: &[u8], max_pixels: u64, context: &str) -> AppResult<DynamicImage> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|err| undecodable(context, "guessing the format", &err))?;
    reader.limits(decode_limits(max_pixels));
    let image = reader
        .decode()
        .map_err(|err| undecodable(context, "decoding", &err))?;
    if image.width() == 0 || image.height() == 0 {
        return Err(undecodable_message());
    }
    Ok(image)
}

/// Encodes `image` for storage as a thumbnail, choosing the format by
/// transparency: opaque images become quality-80 `JPEG` (after a
/// conversion to `RGB`), anything with a translucent pixel stays `PNG`.
fn encode_thumbnail(image: &DynamicImage) -> AppResult<(Vec<u8>, &'static str)> {
    let mut out = Vec::new();
    if has_transparency(image) {
        image
            .write_to(Cursor::new(&mut out), ImageFormat::Png)
            .map_err(|err| encoding_failed("PNG", &err))?;
        return Ok((out, "image/png"));
    }
    let rgb = image.to_rgb8();
    JpegEncoder::new_with_quality(&mut out, THUMBNAIL_JPEG_QUALITY)
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|err| encoding_failed("JPEG", &err))?;
    Ok((out, "image/jpeg"))
}

/// Whether any pixel of an alpha-carrying image is less than fully
/// opaque; an image without an alpha channel is opaque by definition.
fn has_transparency(image: &DynamicImage) -> bool {
    image.has_alpha() && image.to_rgba8().pixels().any(|pixel| pixel.alpha() != 255)
}

/// The 422 for bytes no decoder names an image for, with the cause logged
/// — the client only ever sees the ticket's [`INVALID_IMAGE`].
fn undecodable(context: &str, operation: &str, cause: &dyn fmt::Display) -> AppError {
    debug!(context, operation, %cause, "undecodable image upload");
    undecodable_message()
}

/// The ticket's 422 for undecodable image bytes.
fn undecodable_message() -> AppError {
    AppError::Unprocessable(INVALID_IMAGE.to_owned())
}

/// The 422 for an image over the instance's pixel limit; the dimensions
/// are logged, the client only sees the ticket's [`IMAGE_TOO_LARGE`].
fn too_large(width: u32, height: u32) -> AppError {
    debug!(width, height, "image over MAX_IMAGE_PIXELS");
    AppError::Unprocessable(IMAGE_TOO_LARGE.to_owned())
}

/// Wraps an encoder failure as a 500: reaching it means the image decoded
/// fine but could not be re-encoded, which is a server-side problem, not
/// the upload's.
fn encoding_failed(format: &str, cause: &dyn fmt::Display) -> AppError {
    AppError::Internal(format!("thumbnail {format} encoding failed: {cause}"))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{Frame, ImageBuffer, Rgba, RgbaImage};

    use super::*;

    /// No pixel limit worth hitting in a unit test.
    const ANY_SIZE: u64 = u64::MAX;

    /// The deterministic gradient color of pixel `(x, y)`.
    fn shade(x: u32, y: u32) -> (u8, u8, u8) {
        let channel = |value: u32| u8::try_from(value % 256).expect("a modulo of 256 fits a u8");
        (channel(x * 7 + y * 3), channel(x + y * 11), channel(x * y))
    }

    /// A `width` x `height` gradient, fully opaque.
    fn opaque_raster(width: u32, height: u32) -> RgbaImage {
        ImageBuffer::from_fn(width, height, |x, y| {
            let (r, g, b) = shade(x, y);
            Rgba([r, g, b, 255])
        })
    }

    /// Like [`opaque_raster`], but every other column is fully
    /// transparent, so the image has real alpha to keep.
    fn transparent_raster(width: u32, height: u32) -> RgbaImage {
        ImageBuffer::from_fn(width, height, |x, y| {
            let (r, g, b) = shade(x, y);
            let alpha = if x % 2 == 0 { 0 } else { 255 };
            Rgba([r, g, b, alpha])
        })
    }

    /// Encodes `image` as `format`'s bytes. Through `DynamicImage` so the
    /// encoder picks the color model the format accepts (`JPEG` has no
    /// alpha, `PNG`/`GIF`/`WebP` do).
    fn encode(image: &RgbaImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Vec::new();
        DynamicImage::ImageRgba8(image.clone())
            .write_to(Cursor::new(&mut out), format)
            .expect("the fixture encodes");
        out
    }

    /// A two-frame animated `GIF`: a 40x30 first frame, then a smaller
    /// 20x15 second frame. The encoder writes the trailer on drop.
    fn animated_gif() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut out);
            let first = RgbaImage::from_pixel(40, 30, Rgba([200, 10, 10, 255]));
            let second = RgbaImage::from_pixel(20, 15, Rgba([10, 10, 200, 255]));
            encoder
                .encode_frame(Frame::new(first))
                .expect("the first frame encodes");
            encoder
                .encode_frame(Frame::new(second))
                .expect("the second frame encodes");
        }
        out
    }

    /// The thumbnail `process_image` builds for `bytes` under `max_edge`.
    fn make_thumbnail(bytes: &[u8], max_edge: u32) -> AppResult<Thumbnail> {
        process_image(bytes, ANY_SIZE, max_edge)?.thumbnail
    }

    /// The `Blurhash` `process_image` computes for `bytes`.
    fn compute_blurhash(bytes: &[u8]) -> Option<String> {
        process_image(bytes, ANY_SIZE, 400).ok()?.blurhash
    }

    /// The dimensions `bytes` analyze as, panicking on any rejection.
    fn dimensions(bytes: &[u8]) -> ImageInfo {
        analyze_image(bytes, ANY_SIZE).expect("the fixture is a valid image")
    }

    /// The message `bytes` are rejected with under `max_pixels`,
    /// panicking when they are not.
    fn rejection(bytes: &[u8], max_pixels: u64) -> String {
        analyze_image(bytes, max_pixels)
            .expect_err("the fixture is not a valid image")
            .to_string()
    }

    #[test]
    fn dimensions_are_read_from_the_header_of_every_supported_format() {
        for format in [
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::Gif,
            ImageFormat::WebP,
        ] {
            let bytes = encode(&opaque_raster(64, 32), format);
            assert_eq!(
                dimensions(&bytes),
                ImageInfo {
                    width: 64,
                    height: 32
                },
                "{format:?} dimensions are read"
            );
        }
    }

    #[test]
    fn over_the_pixel_limit_is_the_ticket_message() {
        let bytes = encode(&opaque_raster(300, 200), ImageFormat::Png);
        assert_eq!(
            rejection(&bytes, 59_999),
            IMAGE_TOO_LARGE,
            "one pixel over the limit is a 422"
        );
        assert_eq!(
            analyze_image(&bytes, 60_000)
                .map(|info| (info.width, info.height))
                .expect("exactly the limit is not over it"),
            (300, 200),
            "an image of exactly the limit analyzes"
        );
    }

    #[test]
    fn bytes_that_are_not_an_image_are_the_ticket_message() {
        for (name, bytes) in [
            ("empty", Vec::new()),
            (
                "text",
                b"just some plain text, definitely not media\n".to_vec(),
            ),
            (
                "truncated png",
                b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00".to_vec(),
            ),
            ("png signature only", b"\x89PNG\r\n\x1a\n".to_vec()),
        ] {
            assert_eq!(
                rejection(&bytes, ANY_SIZE),
                INVALID_IMAGE,
                "`{name}` is not a decodable image"
            );
        }
    }

    #[test]
    fn the_thumbnail_keeps_the_aspect_and_the_edge_bound() {
        let bytes = encode(&opaque_raster(900, 600), ImageFormat::Png);
        let thumb = make_thumbnail(&bytes, 400).expect("the fixture thumbnails");
        assert_eq!(
            (thumb.width, thumb.height),
            (400, 267),
            "900x600 scales to 400 wide, 600*400/900 = 266.7 rounded"
        );
        assert_eq!(thumb.content_type, "image/jpeg", "opaque is jpeg");
        assert_eq!(thumb.extension(), "jpg", "and keyed as .jpg");
        let decoded = dimensions(&thumb.bytes);
        assert_eq!(
            (decoded.width, decoded.height),
            (thumb.width, thumb.height),
            "the reported size is the encoded one"
        );
    }

    #[test]
    fn a_smaller_image_is_not_upscaled() {
        let bytes = encode(&opaque_raster(100, 80), ImageFormat::Jpeg);
        let thumb = make_thumbnail(&bytes, 400).expect("the fixture thumbnails");
        assert_eq!(
            (thumb.width, thumb.height),
            (100, 80),
            "an image inside the bound keeps its size"
        );
    }

    #[test]
    fn transparency_survives_as_png() {
        let bytes = encode(&transparent_raster(500, 300), ImageFormat::Png);
        let thumb = make_thumbnail(&bytes, 400).expect("the fixture thumbnails");
        assert_eq!(thumb.content_type, "image/png", "alpha is kept");
        assert_eq!(thumb.extension(), "png", "and keyed as .png");
        assert_eq!((thumb.width, thumb.height), (400, 240), "aspect kept");
        let decoded = dimensions(&thumb.bytes);
        assert_eq!(
            (decoded.width, decoded.height),
            (thumb.width, thumb.height),
            "the reported size is the encoded one"
        );
    }

    #[test]
    fn an_opaque_source_encodes_as_jpeg() {
        let bytes = encode(&opaque_raster(480, 120), ImageFormat::Gif);
        let thumb = make_thumbnail(&bytes, 400).expect("the fixture thumbnails");
        assert_eq!(thumb.content_type, "image/jpeg", "no alpha, jpeg");
        assert_eq!((thumb.width, thumb.height), (400, 100), "aspect kept");
    }

    #[test]
    fn animated_gifs_thumbnail_their_first_frame() {
        let bytes = animated_gif();
        assert_eq!(
            dimensions(&bytes),
            ImageInfo {
                width: 40,
                height: 30
            },
            "the header describes the first frame's canvas"
        );
        let thumb = make_thumbnail(&bytes, 400).expect("the gif thumbnails");
        assert_eq!(
            (thumb.width, thumb.height),
            (40, 30),
            "the thumbnail is the first frame alone"
        );
    }

    #[test]
    fn the_blurhash_is_a_non_empty_string() {
        let bytes = encode(&opaque_raster(64, 48), ImageFormat::Png);
        let hash = compute_blurhash(&bytes).expect("a decodable image hashes");
        assert!(!hash.is_empty(), "the hash string is not empty");
        assert!(
            hash.len() >= 10,
            "a real payload follows the header: {hash}"
        );
        let again = compute_blurhash(&bytes).expect("hashing is deterministic");
        assert_eq!(again, hash, "the same bytes hash the same");
    }

    #[test]
    fn an_undecodable_image_has_no_blurhash() {
        assert!(
            compute_blurhash(b"definitely not a png").is_none(),
            "the hash is best-effort: no panic, no error, just None"
        );
    }

    #[test]
    fn thumbnailing_undecodable_bytes_fails_like_analysis() {
        let err = make_thumbnail(b"definitely not a png", 400).expect_err("no image in there");
        assert_eq!(err.to_string(), INVALID_IMAGE);
    }

    #[test]
    fn process_image_matches_the_separate_passes() {
        let bytes = encode(&opaque_raster(64, 48), ImageFormat::Png);
        let processed = process_image(&bytes, ANY_SIZE, 400).expect("the fixture processes");
        assert_eq!(
            processed.info,
            ImageInfo {
                width: 64,
                height: 48
            },
            "the dimensions are the header's"
        );
        let thumb = processed.thumbnail.expect("the fixture thumbnails");
        assert_eq!(
            (thumb.width, thumb.height),
            (64, 48),
            "an image inside the bound keeps its size"
        );
        assert_eq!(thumb.content_type, "image/jpeg", "opaque is jpeg");
        let old_path = decode(&bytes, ANY_SIZE, "test")
            .ok()
            .and_then(|image| blurhash_of(&downscale(image, BLURHASH_EDGE)));
        let hash = processed.blurhash.expect("a decodable image hashes");
        assert_eq!(hash.len(), 28, "a 4x3 hash is 28 characters");
        assert_eq!(
            Some(hash),
            old_path,
            "a small image hashes as the old decode-and-downscale path did"
        );
    }

    #[test]
    fn process_image_hashes_the_scaled_thumbnail() {
        let bytes = encode(&opaque_raster(900, 600), ImageFormat::Png);
        let processed = process_image(&bytes, ANY_SIZE, 400).expect("the fixture processes");
        let thumb = processed.thumbnail.expect("the fixture thumbnails");
        assert_eq!((thumb.width, thumb.height), (400, 267), "aspect kept");
        assert_eq!(
            processed.blurhash.as_deref().map(str::len),
            Some(28),
            "the hash is the same fixed 4x3 length"
        );
    }

    #[test]
    fn process_image_rejects_an_image_over_the_pixel_limit() {
        let bytes = encode(&opaque_raster(300, 200), ImageFormat::Png);
        let err = process_image(&bytes, 59_999, 400).expect_err("one pixel over the limit");
        assert_eq!(err.to_string(), IMAGE_TOO_LARGE);
    }

    #[test]
    fn process_image_rejects_undecodable_bytes() {
        let err =
            process_image(b"definitely not a png", ANY_SIZE, 400).expect_err("no image in there");
        assert_eq!(err.to_string(), INVALID_IMAGE);
    }

    #[test]
    fn decode_refuses_an_image_over_the_limits() {
        let bytes = encode(&opaque_raster(300, 200), ImageFormat::Png);
        let decoded = decode(&bytes, ANY_SIZE, "test").expect("no limit in the way");
        assert_eq!(decoded.width(), 300, "inside the limits it decodes");
        let err = decode(&bytes, 100, "test").expect_err("300 wide is past a 100 edge limit");
        assert_eq!(err.to_string(), INVALID_IMAGE);
    }

    #[test]
    fn the_decode_limits_follow_the_pixel_budget() {
        let limits = decode_limits(40_000_000);
        assert_eq!(limits.max_image_width, Some(40_000_000), "width budget");
        assert_eq!(limits.max_image_height, Some(40_000_000), "height budget");
        assert_eq!(limits.max_alloc, Some(DECODE_MAX_ALLOC), "alloc cap");
        assert_eq!(
            decode_limits(u64::MAX).max_image_width,
            Some(u32::MAX),
            "a budget past u32 saturates"
        );
    }

    #[test]
    fn the_process_wide_gate_has_a_permit() {
        assert!(
            decode_semaphore().available_permits() >= 1,
            "at least one decode may run"
        );
    }

    #[tokio::test]
    async fn a_second_permit_waits_for_the_first_to_drop() {
        let semaphore = Arc::new(Semaphore::new(1));
        let first = acquire_decode_permit(&semaphore)
            .await
            .expect("the first permit is free");
        assert!(
            semaphore.try_acquire().is_err(),
            "no permit is left while the first is held"
        );
        let completed =
            tokio::time::timeout(std::time::Duration::ZERO, acquire_decode_permit(&semaphore))
                .await
                .is_ok();
        assert!(!completed, "the second acquire does not complete");
        drop(first);
        let second = acquire_decode_permit(&semaphore)
            .await
            .expect("the permit is back once the first drops");
        assert!(
            semaphore.try_acquire().is_err(),
            "the second holder has taken the only permit"
        );
        drop(second);
    }

    #[tokio::test]
    async fn acquiring_from_a_closed_gate_is_an_internal_error() {
        let semaphore = Arc::new(Semaphore::new(1));
        semaphore.close();
        let err = acquire_decode_permit(&semaphore)
            .await
            .expect_err("a closed gate hands out nothing");
        assert!(
            matches!(err, AppError::Internal(_)),
            "a closed gate is a server-side problem"
        );
    }
}
