//! The default images the API advertises, served from `/media/default/…`.
//!
//! An account without an avatar or banner, the instance thumbnail and the
//! default mascot all point at a fixed URL under
//! [`Config::media_base_url`]. The backend ships those four images itself:
//! they are rendered once, on first use, as flat-colour placeholders with the
//! `image` crate (so the repository holds no binary files) and served by
//! [`crate::api::media_files`] ahead of the uploads directory, at
//! `GET /media/default/{name}`.
//!
//! The `*_PATH` constants are the paths appended to the media base URL;
//! [`url()`] builds the full address and [`lookup`] answers a `/media` key.

use std::{io::Cursor, sync::LazyLock};

use bytes::Bytes;
use image::{ImageFormat, Rgb, RgbImage};

use crate::config::Config;

/// Path under the media base URL of the default avatar, matching Pleroma's
/// `[:assets, :default_user_avatar]` fallback.
pub const AVATAR_PATH: &str = "/default/avi.png";
/// Path under the media base URL of the default banner.
pub const BANNER_PATH: &str = "/default/banner.png";
/// Path under the media base URL of the instance thumbnail.
pub const THUMBNAIL_PATH: &str = "/default/thumbnail.jpg";
/// Path under the media base URL of the default mascot.
pub const MASCOT_PATH: &str = "/default/mascot.png";

/// One default image: where it is served, how big it is and what it is made
/// of.
#[derive(Debug, Clone, Copy)]
struct Spec {
    /// Path under the media base URL, with its leading slash.
    path: &'static str,
    /// Width in pixels.
    width: u32,
    /// Height in pixels.
    height: u32,
    /// Encoding of the served bytes.
    format: ImageFormat,
    /// The flat colour the image is filled with.
    color: [u8; 3],
}

/// Every default image.
const SPECS: [Spec; 4] = [
    Spec {
        path: AVATAR_PATH,
        width: 400,
        height: 400,
        format: ImageFormat::Png,
        color: [0x5b, 0x6c, 0x8f],
    },
    Spec {
        path: BANNER_PATH,
        width: 1500,
        height: 500,
        format: ImageFormat::Png,
        color: [0x2e, 0x3b, 0x55],
    },
    Spec {
        path: THUMBNAIL_PATH,
        width: 1200,
        height: 630,
        format: ImageFormat::Jpeg,
        color: [0x2e, 0x3b, 0x55],
    },
    Spec {
        path: MASCOT_PATH,
        width: 512,
        height: 512,
        format: ImageFormat::Png,
        color: [0x7a, 0x5c, 0xc2],
    },
];

/// The encoded images, in [`SPECS`] order; `None` for one that failed to
/// encode (logged), which then answers 404 rather than failing the process.
static RENDERED: LazyLock<Vec<Option<Bytes>>> =
    LazyLock::new(|| SPECS.iter().map(render).collect());

/// `spec` as encoded bytes.
fn render(spec: &Spec) -> Option<Bytes> {
    let image = RgbImage::from_pixel(spec.width, spec.height, Rgb(spec.color));
    let mut out = Cursor::new(Vec::new());
    image
        .write_to(&mut out, spec.format)
        .inspect_err(
            |error| tracing::error!(%error, path = spec.path, "default asset failed to encode"),
        )
        .ok()?;
    Some(Bytes::from(out.into_inner()))
}

/// The full URL of the default asset at `path` (one of the `*_PATH`
/// constants): [`Config::media_base_url`] followed by the path.
#[must_use]
pub fn url(config: &Config, path: &str) -> String {
    format!("{}{path}", config.media_base_url)
}

/// The bytes of the default asset a `/media/{key}` request names, e.g.
/// `default/avi.png`; `None` for any other key.
#[must_use]
pub fn lookup(key: &str) -> Option<Bytes> {
    SPECS
        .iter()
        .zip(RENDERED.iter())
        .find(|(spec, _)| spec.path.strip_prefix('/') == Some(key))
        .and_then(|(_, bytes)| bytes.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_asset_decodes_to_its_size() {
        for spec in SPECS {
            let key = spec.path.trim_start_matches('/');
            let bytes = lookup(key).unwrap_or_else(|| panic!("{key} is not served"));
            let decoded = image::load_from_memory_with_format(&bytes, spec.format)
                .unwrap_or_else(|error| panic!("{key} does not decode: {error}"));
            assert_eq!(
                (decoded.width(), decoded.height()),
                (spec.width, spec.height),
                "{key} has the size of its spec"
            );
        }
    }

    #[test]
    fn the_formats_match_the_extensions() {
        for spec in SPECS {
            let key = spec.path.trim_start_matches('/');
            let bytes = lookup(key).unwrap();
            let guessed = image::guess_format(&bytes).unwrap();
            assert_eq!(
                guessed, spec.format,
                "{key} is encoded as its extension says"
            );
        }
    }

    #[test]
    fn other_keys_are_not_default_assets() {
        for key in [
            "",
            "avi.png",
            "default/",
            "default/other.png",
            "/default/avi.png",
        ] {
            assert!(lookup(key).is_none(), "{key:?} is not a default asset");
        }
    }

    #[test]
    fn the_url_hangs_off_the_media_base_url() {
        let mut config = Config::for_tests();
        assert_eq!(
            url(&config, AVATAR_PATH),
            "https://test.example/media/default/avi.png",
            "the default media base URL"
        );
        config.media_base_url = "https://cdn.example/files".to_owned();
        assert_eq!(
            url(&config, MASCOT_PATH),
            "https://cdn.example/files/default/mascot.png",
            "a configured media base URL"
        );
    }
}
