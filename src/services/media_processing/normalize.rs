//! Re-encoding of the images an instance publishes on a user's behalf.
//!
//! An avatar, a banner, a profile background and a replacement media
//! preview are served from the instance's own origin to anyone who fetches
//! the URL. Storing the uploaded bytes as they arrived would publish
//! whatever rides in them: `EXIF` blocks with GPS coordinates, text
//! chunks, comment extensions, trailing data that makes a file a polyglot.
//! [`normalize_image`] runs the same header gate as a regular upload
//! ([`analyze_image`](super::analyze_image), the `MAX_IMAGE_PIXELS` cap),
//! decodes the pixels under the decoder limits, applies an `EXIF`
//! orientation the re-encode would otherwise lose, and encodes the pixels
//! again in the format they came in. Nothing but pixels (and, for a `GIF`,
//! frame delays) survives.
//!
//! A `PNG`, `JPEG` or `WebP` keeps its format; an animated `GIF` keeps its
//! frames, bounded by [`MAX_GIF_FRAMES`] and a total pixel budget. The
//! `WebP` encoder writes stills only, so an animated `WebP` is stored as
//! its first frame.

use std::io::Cursor;

use bytes::Bytes;
use image::{
    AnimationDecoder as _, DynamicImage, ExtendedColorType, ImageDecoder as _, ImageEncoder as _,
    ImageFormat, ImageReader,
    codecs::{
        gif::{GifDecoder, GifEncoder, Repeat},
        jpeg::JpegEncoder,
    },
    metadata::Orientation,
};

use crate::{
    config::Config,
    error::{AppError, AppResult},
};

use super::{
    ImageInfo, acquire_decode_permit, analyze_image, decode_limits, decode_semaphore, too_large,
    undecodable, undecodable_message,
};

/// Quality the re-encoded `JPEG` is written at.
const JPEG_QUALITY: u8 = 90;
/// The `GIF` quantizer speed (1 is slowest and best, 30 the fastest).
const GIF_ENCODE_SPEED: i32 = 10;
/// Most frames of an animated `GIF` that are re-encoded.
const MAX_GIF_FRAMES: usize = 500;
/// How many times `MAX_IMAGE_PIXELS` the frames of one `GIF` may add up to.
const GIF_PIXEL_BUDGET_FACTOR: u64 = 4;
/// The still formats [`encode_still`] writes, with their `MIME` types.
const STILL_FORMATS: [(ImageFormat, &str); 2] = [
    (ImageFormat::Png, "image/png"),
    (ImageFormat::WebP, "image/webp"),
];
/// What the decode failures log as the place they happened.
const CONTEXT: &str = "normalizing";

/// An image after [`normalize_image`]: freshly encoded bytes and their type.
#[derive(Debug)]
pub struct NormalizedImage {
    /// The re-encoded file.
    pub bytes: Vec<u8>,
    /// The `MIME` type of [`bytes`](Self::bytes).
    pub content_type: &'static str,
}

/// Normalizes `bytes` on the blocking pool, holding a permit from the
/// process-wide decode gate for as long as the decode runs.
///
/// # Errors
///
/// As [`normalize_image`]'s, plus [`AppError::Internal`] when the blocking
/// task is gone.
pub async fn normalize_upload(config: &Config, bytes: Bytes) -> AppResult<NormalizedImage> {
    let max_pixels = u64::try_from(config.max_image_pixels).unwrap_or(u64::MAX);
    let permit = acquire_decode_permit(decode_semaphore()).await?;
    tokio::task::spawn_blocking(move || {
        // Held until the pass returns, even if the request is cancelled.
        let normalized = normalize_image(&bytes, max_pixels);
        drop(permit);
        normalized
    })
    .await
    .map_err(|err| AppError::Internal(format!("image normalization task did not finish: {err}")))?
}

/// Decodes `bytes` and encodes the pixels again, so no metadata survives.
///
/// The header gate of [`analyze_image`] runs first: an image over
/// `max_pixels` is refused before a pixel is decoded.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `Invalid image` for bytes no decoder
/// reads (or a format other than `PNG`, `JPEG`, `GIF` and `WebP`) and with
/// `Image too large` for a pixel count over `max_pixels` or an animation
/// over its frame or pixel budget; [`AppError::Internal`] when the decoded
/// pixels cannot be encoded.
pub fn normalize_image(bytes: &[u8], max_pixels: u64) -> AppResult<NormalizedImage> {
    let _gate: ImageInfo = analyze_image(bytes, max_pixels)?;
    let format = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|err| undecodable(CONTEXT, "guessing the format", &err))?
        .format()
        .ok_or_else(undecodable_message)?;
    if format == ImageFormat::Gif {
        return reencode_gif(bytes, max_pixels);
    }
    if format == ImageFormat::Jpeg {
        return reencode_jpeg(bytes, max_pixels);
    }
    let content_type = STILL_FORMATS
        .iter()
        .find(|(known, _)| *known == format)
        .map(|(_, content_type)| *content_type)
        .ok_or_else(undecodable_message)?;
    encode_still(bytes, max_pixels, format, content_type)
}

/// Decodes the first frame of `bytes` under the decode limits and turns it
/// the way an `EXIF` orientation says, the one piece of metadata a viewer
/// needs to show the pixels the way their owner saw them.
fn decode_oriented(bytes: &[u8], max_pixels: u64) -> AppResult<DynamicImage> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|err| undecodable(CONTEXT, "guessing the format", &err))?;
    reader.limits(decode_limits(max_pixels));
    let mut decoder = reader
        .into_decoder()
        .map_err(|err| undecodable(CONTEXT, "opening the decoder", &err))?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder)
        .map_err(|err| undecodable(CONTEXT, "decoding", &err))?;
    if image.width() == 0 || image.height() == 0 {
        return Err(undecodable_message());
    }
    image.apply_orientation(orientation);
    Ok(image)
}

/// `image` as 8-bit samples, with an alpha channel only when it had one: the
/// color model every still encoder here accepts.
fn eight_bit(image: &DynamicImage) -> DynamicImage {
    if image.color().has_alpha() {
        DynamicImage::ImageRgba8(image.to_rgba8())
    } else {
        DynamicImage::ImageRgb8(image.to_rgb8())
    }
}

/// Re-encodes a `PNG` or `WebP` still as `format`.
fn encode_still(
    bytes: &[u8],
    max_pixels: u64,
    format: ImageFormat,
    content_type: &'static str,
) -> AppResult<NormalizedImage> {
    let image = eight_bit(&decode_oriented(bytes, max_pixels)?);
    let mut out = Vec::new();
    image
        .write_to(Cursor::new(&mut out), format)
        .map_err(|err| encoding_failed(content_type, &err))?;
    Ok(NormalizedImage {
        bytes: out,
        content_type,
    })
}

/// Re-encodes a `JPEG` at [`JPEG_QUALITY`], the orientation applied.
fn reencode_jpeg(bytes: &[u8], max_pixels: u64) -> AppResult<NormalizedImage> {
    let rgb = decode_oriented(bytes, max_pixels)?.to_rgb8();
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|err| encoding_failed("image/jpeg", &err))?;
    Ok(NormalizedImage {
        bytes: out,
        content_type: "image/jpeg",
    })
}

/// Re-encodes every frame of a `GIF`, one at a time so only one is in memory,
/// looping forever as profile animations do.
fn reencode_gif(bytes: &[u8], max_pixels: u64) -> AppResult<NormalizedImage> {
    let mut decoder = GifDecoder::new(Cursor::new(bytes))
        .map_err(|err| undecodable(CONTEXT, "opening the GIF decoder", &err))?;
    decoder
        .set_limits(decode_limits(max_pixels))
        .map_err(|err| undecodable(CONTEXT, "limiting the GIF decoder", &err))?;
    let budget = max_pixels.saturating_mul(GIF_PIXEL_BUDGET_FACTOR);
    let mut spent = 0_u64;
    let mut out = Vec::new();
    let mut encoder = GifEncoder::new_with_speed(&mut out, GIF_ENCODE_SPEED);
    encoder
        .set_repeat(Repeat::Infinite)
        .map_err(|err| encoding_failed("image/gif", &err))?;
    let mut frames = 0_usize;
    for frame in decoder.into_frames() {
        let frame = frame.map_err(|err| undecodable(CONTEXT, "decoding a GIF frame", &err))?;
        let (width, height) = frame.buffer().dimensions();
        spent = spent.saturating_add(u64::from(width).saturating_mul(u64::from(height)));
        frames = frames.saturating_add(1);
        if frames > MAX_GIF_FRAMES || spent > budget {
            return Err(too_large(width, height));
        }
        encoder
            .encode_frame(frame)
            .map_err(|err| encoding_failed("image/gif", &err))?;
    }
    // Dropping the encoder writes the trailer.
    drop(encoder);
    if frames == 0 {
        return Err(undecodable_message());
    }
    Ok(NormalizedImage {
        bytes: out,
        content_type: "image/gif",
    })
}

/// Wraps an encoder failure as a 500: the image decoded fine, so the fault
/// is the server's.
fn encoding_failed(content_type: &str, cause: &dyn std::fmt::Display) -> AppError {
    AppError::Internal(format!("{content_type} re-encoding failed: {cause}"))
}

#[cfg(test)]
mod tests {
    use image::{Frame, Rgb, RgbImage, Rgba, RgbaImage};

    use super::*;

    /// A marker no encoder would write on its own.
    const SECRET: &[u8] = b"GPS-51.5074N-0.1278W";

    /// Whether `needle` occurs in `haystack`.
    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    /// `bytes` with `extra` spliced in `at` bytes from the start.
    fn spliced(bytes: &[u8], at: usize, extra: &[u8]) -> Vec<u8> {
        let (head, tail) = bytes.split_at(at);
        [head, extra, tail].concat()
    }

    /// `image` encoded as `format`.
    fn encoded(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Vec::new();
        image
            .write_to(Cursor::new(&mut out), format)
            .expect("the fixture encodes");
        out
    }

    /// A `width` x `height` opaque gradient.
    fn raster(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
            let channel = |value: u32| u8::try_from(value % 256).expect("a modulo fits a u8");
            Rgb([channel(x * 9), channel(y * 5), channel(x + y)])
        }))
    }

    /// An `APP1` segment holding an `EXIF` block with `Orientation` `6`
    /// (rotate 90° clockwise) and `SECRET` as trailing data.
    fn exif_segment() -> Vec<u8> {
        let mut tiff = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0".to_vec();
        tiff.extend_from_slice(SECRET);
        let mut segment = vec![0xFF, 0xE1];
        let length = u16::try_from(2 + 6 + tiff.len()).expect("the segment is short");
        segment.extend_from_slice(&length.to_be_bytes());
        segment.extend_from_slice(b"Exif\0\0");
        segment.extend_from_slice(&tiff);
        segment
    }

    /// The decoded dimensions of `bytes`.
    fn dimensions_of(bytes: &[u8]) -> (u32, u32) {
        image::load_from_memory(bytes)
            .map(|image| (image.width(), image.height()))
            .expect("the output decodes")
    }

    /// The CRC-32 a `PNG` chunk carries.
    fn crc32(data: &[u8]) -> u32 {
        let step = |crc: u32, _bit: u32| (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        let crc = data.iter().fold(u32::MAX, |crc, byte| {
            (0..8).fold(crc ^ u32::from(*byte), step)
        });
        !crc
    }

    #[test]
    fn a_jpeg_loses_its_exif_and_keeps_its_orientation() {
        let plain = encoded(&raster(4, 2), ImageFormat::Jpeg);
        let tagged = spliced(&plain, 2, &exif_segment());
        assert!(contains(&tagged, SECRET), "the fixture carries the secret");

        let normalized = normalize_image(&tagged, u64::MAX).expect("a valid JPEG normalizes");
        assert_eq!(normalized.content_type, "image/jpeg", "the format is kept");
        assert!(
            !contains(&normalized.bytes, SECRET) && !contains(&normalized.bytes, b"Exif"),
            "no EXIF block is left"
        );
        assert_eq!(
            dimensions_of(&normalized.bytes),
            (2, 4),
            "orientation 6 was applied to the pixels before it was dropped"
        );
    }

    #[test]
    fn a_png_loses_its_text_chunks() {
        let plain = encoded(&raster(3, 3), ImageFormat::Png);
        let mut chunk_body = b"tEXt".to_vec();
        chunk_body.extend_from_slice(b"Comment\0");
        chunk_body.extend_from_slice(SECRET);
        let length = u32::try_from(chunk_body.len() - 4).expect("the chunk is short");
        let mut chunk = length.to_be_bytes().to_vec();
        chunk.extend_from_slice(&chunk_body);
        chunk.extend_from_slice(&crc32(&chunk_body).to_be_bytes());
        // After the 8-byte signature and the 25-byte IHDR chunk.
        let tagged = spliced(&plain, 33, &chunk);
        assert!(contains(&tagged, SECRET), "the fixture carries the secret");

        let normalized = normalize_image(&tagged, u64::MAX).expect("a valid PNG normalizes");
        assert_eq!(normalized.content_type, "image/png", "the format is kept");
        assert!(
            !contains(&normalized.bytes, SECRET),
            "the text chunk is gone"
        );
        assert_eq!(dimensions_of(&normalized.bytes), (3, 3), "the pixels stay");
    }

    #[test]
    fn a_png_keeps_its_transparency() {
        let source = RgbaImage::from_pixel(2, 2, Rgba([10, 20, 30, 0]));
        let bytes = encoded(&DynamicImage::ImageRgba8(source), ImageFormat::Png);
        let normalized = normalize_image(&bytes, u64::MAX).expect("an RGBA PNG normalizes");
        let decoded = image::load_from_memory(&normalized.bytes).expect("the output decodes");
        assert_eq!(
            decoded.to_rgba8().get_pixel(0, 0).0[3],
            0,
            "a transparent pixel is still transparent"
        );
    }

    #[test]
    fn a_webp_stays_a_webp() {
        let bytes = encoded(&raster(5, 4), ImageFormat::WebP);
        let normalized = normalize_image(&bytes, u64::MAX).expect("a WebP normalizes");
        assert_eq!(normalized.content_type, "image/webp", "the format is kept");
        assert_eq!(dimensions_of(&normalized.bytes), (5, 4), "the pixels stay");
    }

    #[test]
    fn an_animated_gif_keeps_its_frames_and_loses_its_comment() {
        let mut gif = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut gif);
            for color in [[200, 10, 10, 255], [10, 10, 200, 255]] {
                encoder
                    .encode_frame(Frame::new(RgbaImage::from_pixel(6, 4, Rgba(color))))
                    .expect("a frame encodes");
            }
        }
        // A comment extension just before the trailer byte.
        let mut comment = vec![0x21, 0xFE, u8::try_from(SECRET.len()).expect("short")];
        comment.extend_from_slice(SECRET);
        comment.push(0);
        let tagged = spliced(&gif, gif.len() - 1, &comment);
        assert!(contains(&tagged, SECRET), "the fixture carries the secret");

        let normalized = normalize_image(&tagged, u64::MAX).expect("a GIF normalizes");
        assert_eq!(normalized.content_type, "image/gif", "the format is kept");
        assert!(!contains(&normalized.bytes, SECRET), "the comment is gone");
        let frames = GifDecoder::new(Cursor::new(normalized.bytes.as_slice()))
            .expect("the output is a GIF")
            .into_frames()
            .count();
        assert_eq!(frames, 2, "both frames survive");
    }

    #[test]
    fn an_image_over_the_pixel_cap_is_refused() {
        let bytes = encoded(&raster(10, 10), ImageFormat::Png);
        let err = normalize_image(&bytes, 99).expect_err("100 pixels pass a cap of 99");
        assert_eq!(
            err.to_string(),
            "Image too large",
            "the upload endpoint's 422"
        );
        let fits: NormalizedImage = normalize_image(&bytes, 100).expect("exactly the cap fits");
        assert_eq!(fits.content_type, "image/png", "and is stored");
    }

    #[test]
    fn a_gif_over_its_pixel_budget_is_refused() {
        let mut gif = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut gif);
            for _ in 0..5 {
                encoder
                    .encode_frame(Frame::new(RgbaImage::from_pixel(
                        10,
                        10,
                        Rgba([1, 2, 3, 255]),
                    )))
                    .expect("a frame encodes");
            }
        }
        let err = normalize_image(&gif, 100).expect_err("500 pixels pass a budget of 400");
        assert_eq!(err.to_string(), "Image too large", "the same 422");
        let fits: NormalizedImage =
            normalize_image(&gif, 125).expect("500 pixels fit a budget of 500");
        assert_eq!(fits.content_type, "image/gif", "and is stored");
    }

    #[test]
    fn bytes_that_are_not_an_image_are_invalid() {
        for bytes in [
            &b"not an image"[..],
            b"",
            b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01",
        ] {
            let err = normalize_image(bytes, u64::MAX).expect_err("no image here");
            assert_eq!(err.to_string(), "Invalid image", "{bytes:?} is undecodable");
        }
    }

    #[tokio::test]
    async fn the_async_entry_point_uses_the_configured_cap() {
        let bytes = Bytes::from(encoded(&raster(10, 10), ImageFormat::Png));
        let mut config = Config::for_tests();
        config.max_image_pixels = 99;
        let err = normalize_upload(&config, bytes.clone())
            .await
            .expect_err("over the configured cap");
        assert_eq!(
            err.to_string(),
            "Image too large",
            "the configured cap rules"
        );
        config.max_image_pixels = 100;
        let fits: NormalizedImage = normalize_upload(&config, bytes)
            .await
            .expect("within the configured cap");
        assert_eq!(fits.content_type, "image/png", "and is stored");
    }
}
