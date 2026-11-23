//! `GET /media/{*key}`: the files [`crate::uploads`] stored, at the URLs
//! [`crate::uploads::UploaderBackend::public_url`] hands out.
//!
//! Pleroma serves uploads from `Pleroma.Web.Plugs.UploadedMedia`
//! (`lib/pleroma/web/plugs/uploaded_media.ex`): the `/media` path, a
//! `public, max-age=…, immutable` `cache-control` header and a
//! `content-security-policy: sandbox` header so a stored document cannot run
//! scripts. This route keeps the caching behaviour and replaces the sandbox
//! with a stricter rule of its own. It is merged at the top level of
//! [`crate::app`] rather than under `/api`, because the URLs stored in
//! activities point there.
//!
//! Two rules make a file store safe to serve from the instance's own origin:
//!
//! - the type comes from the key's extension, never from what the uploader
//!   claimed (`crate::uploads::content_type_for_key`), and only the media
//!   types that extension map knows — images, video and audio — is offered
//!   inline; HTML, SVG, scripts and byte soup are sent as an `attachment`, so
//!   a stored file cannot run scripts here;
//! - an unusable key answers the same 404 as an absent file, so the route
//!   cannot be used to probe the filesystem or enumerate other users' uploads.
//!
//! Every response is `immutable` for a year: [`crate::uploads::media_key`]
//! gives each upload a fresh random name, so a key never changes contents.

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderValue, header},
    response::{IntoResponse as _, Response},
    routing::get,
};
use bytes::Bytes;

use crate::{
    error::{AppError, AppResult},
    state::AppState,
    uploads,
    util::headers::NOSNIFF,
};

/// `cache-control` for stored media: keys are random and never reused, so a
/// cached copy never goes stale.
const CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
/// How a browser shows a file it renders by itself.
const INLINE: &str = "inline";
/// How a browser downloads anything else instead of rendering it.
const ATTACHMENT: &str = "attachment";

/// The `/media` router, merged at the top level by [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new().route("/media/{*key}", get(serve_media))
}

/// `GET /media/{*key}` — the file stored under `key`. No authentication: a
/// media key is unguessable and holding it is the permission to read it.
///
/// # Errors
///
/// [`AppError::NotFound`] for a key that is not stored or that could not name
/// a file inside the uploads directory; other storage failures arrive through
/// `?` as [`AppError::Internal`].
async fn serve_media(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> AppResult<Response> {
    if uploads::check_key(&key).is_err() {
        return Err(AppError::NotFound);
    }
    let bytes = state.uploader.read(&key).await?;
    Ok(media_response(&key, bytes))
}

/// The stored bytes for `key` with the headers of the [module docs].
///
/// [module docs]: self
fn media_response(key: &str, bytes: Bytes) -> Response {
    let content_type = uploads::content_type_for_key(key);
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static(CACHE_CONTROL),
            ),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static(disposition_for(content_type)),
            ),
            (header::X_CONTENT_TYPE_OPTIONS, NOSNIFF.clone()),
        ],
        bytes,
    )
        .into_response()
}

/// Whether a browser renders `content_type` or downloads it.
fn disposition_for(content_type: &str) -> &'static str {
    if uploads::is_inline_media(content_type) {
        INLINE
    } else {
        ATTACHMENT
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderName;

    use super::*;

    /// The value of header `name` in the response the route renders for `key`.
    fn header_of(key: &str, name: &str) -> String {
        let name = HeaderName::try_from(name).unwrap();
        media_response(key, Bytes::new())
            .headers()
            .get(&name)
            .unwrap_or_else(|| panic!("{name} is missing from the response for {key}"))
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn media_files_are_inline_media() {
        for key in ["u/a.png", "u/a.mp4", "u/a.wav"] {
            assert_eq!(
                header_of(key, "content-disposition"),
                INLINE,
                "{key} renders"
            );
            assert_eq!(
                header_of(key, "cache-control"),
                CACHE_CONTROL,
                "{key} caches"
            );
            assert_eq!(
                header_of(key, "x-content-type-options"),
                NOSNIFF.to_str().unwrap(),
                "{key} is fixed"
            );
        }
    }

    #[test]
    fn anything_else_is_an_attachment() {
        for key in ["u/page.html", "u/icon.svg", "u/data.bin", "u/noext"] {
            assert_eq!(
                header_of(key, "content-disposition"),
                ATTACHMENT,
                "{key} must not render"
            );
            assert_eq!(
                header_of(key, "content-type"),
                "application/octet-stream",
                "{key} has no rendered type"
            );
        }
    }

    #[test]
    fn the_served_type_follows_the_extension() {
        assert_eq!(header_of("u/a.jpg", "content-type"), "image/jpeg");
        assert_eq!(header_of("u/a.webm", "content-type"), "video/webm");
    }
}
