//! The storage half of the profile images: naming the media type from
//! the bytes ([`detect_image_type`]), writing one file
//! ([`store_image`]) and removing the files a replacement or a clear
//! left behind ([`delete_replaced_files`]). The resolution of the three
//! slots is [`profile_images`]'s.
//!
//! [`profile_images`]: super::profile_images

use bytes::Bytes;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{config::Config, error::AppResult, state::AppState, uploads};

/// The media types a profile image may be, as the MIME types [`infer`]
/// reads from the bytes: the four the ticket names (Pleroma accepts these
/// and nothing else for `avatar`, `header` and `pleroma_background_image`).
const ACCEPTED_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// The media type of profile-image bytes, from the bytes themselves and not
/// from the `content-type` the part was sent with: the `infer` match when its
/// type is one of [`ACCEPTED_IMAGE_TYPES`].
///
/// # Errors
///
/// [`AppError::Unprocessable`](crate::error::AppError::Unprocessable) with
/// `Unsupported file type` for bytes no matcher names, and for the media
/// types a profile image may not be (see [`uploads::sniff_allowed`]).
pub(super) fn detect_image_type(bytes: &[u8]) -> AppResult<&'static str> {
    uploads::sniff_allowed(bytes, &ACCEPTED_IMAGE_TYPES)
}

/// Stores one profile image: a fresh [`media_key`](uploads::media_key) under
/// the uploader's id, the bytes at that key, and the key's public URL as the
/// value the column holds — the address [`crate::api::media_files`] serves
/// the file from.
///
/// # Errors
///
/// As [`uploads::store_public`]'s: [`AppError::Internal`] when the
/// filesystem rejects the write.
///
/// [`AppError::Internal`]: crate::error::AppError::Internal
pub(super) async fn store_image(
    state: &AppState,
    user_id: Uuid,
    bytes: Bytes,
    filename: Option<&str>,
    content_type: &str,
) -> AppResult<String> {
    uploads::store_public(state, user_id, bytes, filename, content_type).await
}

/// Removes the files the profile-image writes replaced, run after the row
/// stopped naming them: see [`delete_replaced_file`] for one URL.
pub(super) async fn delete_replaced_files(state: &AppState, replaced: Vec<String>) {
    for stored_url in replaced {
        delete_replaced_file(state, &stored_url).await;
    }
}

/// Best-effort removal of one replaced profile image. Only a URL under this
/// instance's own media prefix names a file we hold; a foreign or hand-set
/// URL is left alone, and a removal that fails is logged, not an error — the
/// profile update it follows already succeeded.
async fn delete_replaced_file(state: &AppState, stored_url: &str) {
    let Some(key) = own_media_key(&state.config, stored_url) else {
        debug!(
            stored_url,
            "keeping a replaced profile image we do not store"
        );
        return;
    };
    if let Err(err) = state.uploader.delete(&key).await {
        warn!(%err, %key, "could not delete the replaced profile image");
    }
}

/// The uploads key `stored_url` names when it is one of this instance's own
/// media addresses — `{MEDIA_BASE_URL}/{key}` with a `key`
/// [`check_key`](uploads::check_key) accepts — and `None` for no URL, for a
/// file on another host, and for a remainder that is not a usable key.
#[must_use]
fn own_media_key(config: &Config, stored_url: &str) -> Option<String> {
    let prefix = format!("{}/", config.media_base_url);
    let key = stored_url.strip_prefix(prefix.as_str())?;
    uploads::check_key(key).is_ok().then(|| key.to_owned())
}

#[cfg(test)]
mod tests {
    use crate::api::accounts::{
        profile_images::{AVATAR_FIELD, UNSUPPORTED_FILE_TYPE, resolve_images},
        test_support::{BMP, GIF, JPEG, PDF, PNG, TEXT, WEBP, no_params, part},
    };

    use super::*;

    #[test]
    fn the_four_accepted_image_types_are_read_from_the_bytes() {
        for (bytes, mime) in [
            (PNG, "image/png"),
            (JPEG, "image/jpeg"),
            (GIF, "image/gif"),
            (WEBP, "image/webp"),
        ] {
            assert_eq!(
                detect_image_type(bytes).unwrap(),
                mime,
                "the magic bytes name {mime}"
            );
        }
    }

    #[test]
    fn anything_else_is_the_unsupported_file_type() {
        // `BMP` proves the allowlist, not merely the "is it an image"
        // question: `infer` names it, and the endpoint still refuses it.
        for bytes in [BMP, PDF, TEXT, b""] {
            let err = detect_image_type(bytes).expect_err("no profile image");
            assert_eq!(
                err.to_string(),
                UNSUPPORTED_FILE_TYPE,
                "bytes {bytes:?} are refused with the type message"
            );
        }
    }

    #[test]
    fn the_part_content_type_claim_is_ignored() {
        // The extractor's `content_type` says `image/png` for every fixture
        // part; what decides is the bytes, so a text body under an image
        // claim is still the 422.
        let mut files = vec![part(AVATAR_FIELD, TEXT)];
        let err = resolve_images(&Config::for_tests(), &mut files, &no_params())
            .expect_err("a text file is no avatar");
        assert_eq!(err.to_string(), UNSUPPORTED_FILE_TYPE);
    }

    #[test]
    fn only_this_instances_own_media_urls_name_a_file_we_hold() {
        let config = Config::for_tests();
        assert_eq!(
            own_media_key(&config, "https://test.example/media/alice/a.png").as_deref(),
            Some("alice/a.png"),
            "our own media URL is a key"
        );
        for stored_url in [
            "",
            "https://elsewhere.example/media/alice/a.png",
            "https://test.example/media",
            "https://test.example/mediazonk/a.png",
            "https://test.example/media/../escape.png",
            "https://test.example/media/alice//a.png",
        ] {
            assert_eq!(
                own_media_key(&config, stored_url),
                None,
                "`{stored_url}` names no file of ours",
            );
        }
        assert_eq!(
            own_media_key(&config, "https://test.example/media/"),
            None,
            "an empty remainder is no key either"
        );
    }
}
