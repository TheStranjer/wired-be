//! Image analysis for media uploads: dimensions, thumbnails, `Blurhash`.
//!
//! The reference is Pleroma's upload pipeline: `Mogrify`
//! ([`lib/pleroma/upload/filter/mogrify.ex`]) scales an uploaded image
//! down and stores it as the attachment's `small` preview, and
//! `StatusView.attachment`
//! ([`lib/pleroma/web/mastodon_api/views/status_view.ex`]) renders the
//! `meta.original` / `meta.small` dimension objects these functions feed.
//! Pleroma computes the `Blurhash` string over a downscaled copy of the
//! image with its `blurhash` dependency; [`compute_blurhash`] does the
//! same with the Rust `blurhash` crate.
//!
//! These functions are pure CPU work over the bytes an upload carried;
//! callers run them through [`tokio::task::spawn_blocking`] (see
//! [`crate::api::media`]) and decide what a failure costs: an undecodable
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

use std::{fmt, io::Cursor};

use image::{
    DynamicImage, ExtendedColorType, ImageFormat, ImageReader, codecs::jpeg::JpegEncoder,
    imageops::FilterType,
};
use tracing::debug;

// The resize/query/encode helpers below reach these traits' methods only.
use image::{GenericImageView as _, ImageEncoder as _, Pixel as _};

use crate::error::{AppError, AppResult};

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

/// Scales an image down to a thumbnail that fits within `max_edge` pixels
/// on both edges, keeping the aspect ratio, and encodes it.
///
/// The resize is Lanczos3 ([`image::imageops::resize`], reached through
/// [`DynamicImage::resize`]), which only ever scales down: an image
/// already inside `max_edge` comes back at its own size rather than blown
/// up. The encoding is `JPEG` at quality 80 unless any pixel of the
/// (first frame of the) image is less than fully opaque, in which case it
/// is `PNG`, because `JPEG` has no room for transparency.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_IMAGE`] for undecodable
/// bytes, and [`AppError::Internal`] when the encoder itself fails.
pub fn make_thumbnail(bytes: &[u8], max_edge: u32) -> AppResult<Thumbnail> {
    let image = decode(bytes, "thumbnailing")?;
    let resized = downscale(&image, max_edge);
    let (width, height) = resized.dimensions();
    let (encoded, content_type) = encode_thumbnail(&resized)?;
    Ok(Thumbnail {
        bytes: encoded,
        content_type,
        width,
        height,
    })
}

/// Scales `image` down to fit within `max_edge` on both edges, keeping the
/// aspect ratio — or hands back the original when it already fits, so a
/// thumbnail is never an upscale.
fn downscale(image: &DynamicImage, max_edge: u32) -> DynamicImage {
    if image.width() <= max_edge && image.height() <= max_edge {
        image.clone()
    } else {
        image.resize(max_edge, max_edge, FilterType::Lanczos3)
    }
}

/// Computes the [`Blurhash`] string of an image's placeholder preview:
/// 4x3 components over a copy downscaled to at most 32 pixels per edge.
///
/// This is an infallible helper for the upload path: any byte that cannot
/// produce a hash — undecodable input, an image too small for the
/// component grid, an encoder rejection — yields `None` and the
/// attachment simply goes without one. Callers usually run this after
/// [`analyze_image`] on the same bytes, so `None` here means "no hash",
/// not "bad image".
///
/// [`Blurhash`]: https://blurha.sh
#[must_use]
pub fn compute_blurhash(bytes: &[u8]) -> Option<String> {
    let image = decode(bytes, "hashing").ok()?;
    let small = downscale(&image, BLURHASH_EDGE);
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

/// Reads an image's `(width, height)` from its header alone.
fn read_dimensions(bytes: &[u8], context: &str) -> AppResult<(u32, u32)> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|err| undecodable(context, "guessing the format", &err))?;
    reader
        .into_dimensions()
        .map_err(|err| undecodable(context, "reading the dimensions", &err))
}

/// Decodes an image's full pixel data — its first frame, for animated
/// formats — after guessing the format from the bytes.
fn decode(bytes: &[u8], context: &str) -> AppResult<DynamicImage> {
    let image = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|err| undecodable(context, "guessing the format", &err))?
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
}
