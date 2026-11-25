//! Real, decodable image files for the tests that upload pictures, and the
//! bytes the server stores for one (profile images and media previews are
//! decoded and encoded again before they are written).

use std::io::Cursor;

use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use wired_be::services::media_processing::normalize_image;

/// A marker standing in for the GPS coordinates a camera writes into `EXIF`.
pub const GPS_MARKER: &[u8] = b"GPSLatitude=51.5074N;GPSLongitude=0.1278W";

/// A `width` x `height` gradient encoded as `format`.
///
/// # Panics
///
/// If the encoder rejects the fixture.
#[must_use]
pub fn still(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
    let channel = |value: u32| u8::try_from(value % 256).expect("a modulo of 256 fits a u8");
    let image = DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
        Rgb([channel(x * 13), channel(y * 7), channel(x + y)])
    }));
    let mut out = Vec::new();
    image
        .write_to(Cursor::new(&mut out), format)
        .expect("the fixture image encodes");
    out
}

/// A small `PNG`.
#[must_use]
pub fn png() -> Vec<u8> {
    still(6, 4, ImageFormat::Png)
}

/// A small `JPEG`.
#[must_use]
pub fn jpeg() -> Vec<u8> {
    still(8, 6, ImageFormat::Jpeg)
}

/// A small, single-frame `GIF`.
#[must_use]
pub fn gif() -> Vec<u8> {
    still(5, 5, ImageFormat::Gif)
}

/// A small `WebP`.
#[must_use]
pub fn webp() -> Vec<u8> {
    still(7, 3, ImageFormat::WebP)
}

/// `jpeg` with an `EXIF` block that carries [`GPS_MARKER`], spliced in right
/// after the start-of-image marker, the place a camera writes it.
///
/// # Panics
///
/// If `jpeg` is shorter than its marker, or the segment does not fit the
/// length field.
#[must_use]
pub fn with_exif_gps(jpeg: &[u8]) -> Vec<u8> {
    let mut tiff = b"MM\0*\0\0\0\x08\0\0\0\0\0\0".to_vec();
    tiff.extend_from_slice(GPS_MARKER);
    let mut segment = vec![0xFF, 0xE1];
    let length = u16::try_from(2 + 6 + tiff.len()).expect("the segment is short");
    segment.extend_from_slice(&length.to_be_bytes());
    segment.extend_from_slice(b"Exif\0\0");
    segment.extend_from_slice(&tiff);
    let (soi, rest) = jpeg.split_at(2);
    [soi, segment.as_slice(), rest].concat()
}

/// Whether `needle` occurs anywhere in `haystack`.
#[must_use]
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The bytes the server stores for an upload of `bytes`: the same encoder
/// run, which is deterministic.
///
/// # Panics
///
/// If `bytes` is not an image the server normalizes.
#[must_use]
pub fn normalized(bytes: &[u8]) -> Vec<u8> {
    normalize_image(bytes, u64::MAX)
        .expect("the fixture normalizes")
        .bytes
}

/// The `(width, height)` of the image `bytes` decodes to.
///
/// # Panics
///
/// If `bytes` is not an image.
#[must_use]
pub fn dimensions(bytes: &[u8]) -> (u32, u32) {
    let image = image::load_from_memory(bytes).expect("the bytes are an image");
    (image.width(), image.height())
}
