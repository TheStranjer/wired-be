//! The static `GET /emoji/{pack}/{file}` route: pack images served from
//! the instance's own origin with the media safety headers.

use axum::{
    extract::{Path, State},
    http::{
        HeaderValue,
        header::{self, CONTENT_DISPOSITION},
    },
    response::{IntoResponse as _, Response},
};

use super::{CACHE_CONTROL, INLINE};
use crate::{
    error::{AppError, AppResult},
    services::emoji_packs,
    state::AppState,
    uploads,
    util::headers::NOSNIFF,
};

/// `GET /emoji/{pack}/{file}` — one image file of a pack, served with the
/// media safety headers. No authentication: the address is what
/// `custom_emojis.image_url` carries into client HTML.
///
/// # Errors
///
/// [`AppError::NotFound`] for an unusable name, a non-image file type or an
/// absent file — one answer for every miss, so the route cannot probe the
/// filesystem; other read failures arrive as [`AppError::Internal`].
pub(super) async fn serve_emoji_file(
    State(state): State<AppState>,
    Path((pack, file)): Path<(String, String)>,
) -> AppResult<Response> {
    let content_type = emoji_content_type(&file)?;
    let read = emoji_packs::read_pack_file(&state.config.emoji_dir, &pack, &file).await;
    let bytes = match read {
        Ok(bytes) => bytes,
        Err(AppError::BadRequest(_)) => return Err(AppError::NotFound),
        Err(err) => return Err(err),
    };
    Ok(emoji_response(content_type, bytes))
}

/// The image content type to serve `file` as, from its extension alone
/// ([`uploads::content_type_for_key`]): a name the allowlist does not map
/// to `image/*` — HTML, SVG, the manifests, byte soup — answers the same
/// 404 as an absent file, so nothing stored in a pack can render in this
/// instance's origin.
fn emoji_content_type(file: &str) -> AppResult<&'static str> {
    let content_type = uploads::content_type_for_key(file);
    content_type
        .starts_with("image/")
        .then_some(content_type)
        .ok_or(AppError::NotFound)
}

/// The stored bytes of a pack image with the safety headers of the [module
/// docs].
///
/// [module docs]: super
fn emoji_response(content_type: &'static str, bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static(CACHE_CONTROL),
            ),
            (header::X_CONTENT_TYPE_OPTIONS, NOSNIFF.clone()),
            (CONTENT_DISPOSITION, HeaderValue::from_static(INLINE)),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_image_extensions_are_served_and_everything_else_404s() {
        for (file, want) in [
            ("cat.png", "image/png"),
            ("cat.gif", "image/gif"),
            ("sub/cat.webp", "image/webp"),
        ] {
            assert_eq!(
                emoji_content_type(file).unwrap(),
                want,
                "{file} is an image"
            );
        }
        for file in [
            "pack.json",
            "page.html",
            "icon.svg",
            "notes.txt",
            "noext",
            "script.js",
        ] {
            let err = emoji_content_type(file).unwrap_err();
            assert!(
                matches!(err, AppError::NotFound),
                "{file} answers the absent-file 404, not {err}"
            );
        }
    }

    #[test]
    fn served_images_carry_the_media_safety_headers() {
        let response = emoji_response("image/png", vec![9]).into_response();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "image/png",
            "the type comes from the extension"
        );
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            CACHE_CONTROL,
            "pack images cache for a day"
        );
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            NOSNIFF,
            "the type may not be guessed over"
        );
        assert_eq!(
            response.headers()[CONTENT_DISPOSITION],
            INLINE,
            "images render"
        );
    }
}
