//! The shape of the received request: the body readers (the size-capped
//! read, its JSON parse, the media-type gate). The signatures it presents
//! are read by the shared
//! [`present_signatures`](crate::federation::signature::verify::present_signatures).

use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, header},
};
use bytes::Bytes;
use http_body_util::{BodyExt as _, LengthLimitError, Limited};
use serde_json::Value;
use tracing::debug;

use super::{InboxAuthError, JSON_MEDIA_TYPES};
use crate::{error::AppError, util::media_type::essence};

/// Whether the request labels its body one of the JSON `ActivityPub` media
/// types: the `Content-Type` up to its first `;`, trimmed and
/// case-folded, names exactly one of [`JSON_MEDIA_TYPES`].
pub(super) fn declares_json_type(headers: &HeaderMap) -> bool {
    header_text(headers, &header::CONTENT_TYPE).is_some_and(|value| {
        let media_type = essence(value);
        JSON_MEDIA_TYPES
            .iter()
            .any(|wanted| media_type.eq_ignore_ascii_case(wanted))
    })
}

/// Reads the request body into memory, stopping with the 413 as soon as
/// more than `max_bytes` have arrived — the cap is enforced while
/// streaming ([`Limited`]), not after buffering an endless body.
pub(super) async fn read_limited(stream: Body, max_bytes: usize) -> Result<Bytes, InboxAuthError> {
    Limited::new(stream, max_bytes)
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .map_err(|err| {
            if err.downcast_ref::<LengthLimitError>().is_some() {
                InboxAuthError::too_large()
            } else {
                debug!("inbox request body could not be read");
                InboxAuthError::Rejected(AppError::BadRequest(
                    "the request body could not be read".to_owned(),
                ))
            }
        })
}

/// Parses the capped body as the JSON activity document.
pub(super) fn parse_json_body(body: &[u8]) -> Result<Value, InboxAuthError> {
    serde_json::from_slice(body).map_err(|_err| InboxAuthError::invalid_json())
}

/// The text of header `name`, `None` when absent or not visible ASCII.
fn header_text<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn the_json_media_types_are_the_three_peers_send() {
        let typed = |value: &str| {
            let mut headers = HeaderMap::new();
            let previous =
                headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(value).unwrap());
            drop(previous);
            headers
        };
        for value in [
            "application/activity+json",
            "application/ld+json",
            "application/json",
            "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"",
            "Application/Activity+JSON; charset=utf-8",
        ] {
            assert!(declares_json_type(&typed(value)), "{value} is accepted");
        }
        for value in ["text/html", "application/xml", "application/jsonld"] {
            assert!(!declares_json_type(&typed(value)), "{value} is refused");
        }
        assert!(
            !declares_json_type(&HeaderMap::new()),
            "a body with no declared type is refused"
        );
    }
}
