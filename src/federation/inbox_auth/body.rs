//! The shape of the received request: the body readers (the size-capped
//! read, its JSON parse, the media-type gate), the `Signature` header's
//! presentation (bare or under `Authorization`), and the two small text
//! helpers both signature steps and [`super::signer`] read.

use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, Uri, header},
};
use bytes::Bytes;
use http_body_util::{BodyExt as _, LengthLimitError, Limited};
use serde_json::Value;
use tracing::debug;

use super::{InboxAuthError, JSON_MEDIA_TYPES, SIGNATURE_HEADER, SIGNATURE_SCHEME};
use crate::{
    error::AppError,
    federation::signature::verify::{ParsedSignature, parse_signature_header},
    util::media_type::essence,
};

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

/// The request target [`verify`] signs over: the path with its query
/// string when it has one.
///
/// [`verify`]: crate::federation::signature::verify::verify
pub(super) fn request_target(uri: &Uri) -> String {
    uri.query().map_or_else(
        || uri.path().to_owned(),
        |query| format!("{}?{query}", uri.path()),
    )
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

/// The parsed `Signature` parameters the request presents: the bare
/// `Signature` header first, then an `Authorization: Signature ...`
/// carrying the same ones. A header that is present but unparsable has
/// failed to present a signature at all, so it reports the same 401.
pub(super) fn present_signature(headers: &HeaderMap) -> Result<ParsedSignature, InboxAuthError> {
    let raw = header_text(headers, &SIGNATURE_HEADER)
        .or_else(|| authorization_signature_text(headers))
        .ok_or_else(InboxAuthError::missing_signature)?;
    parse_signature_header(raw).map_err(|_err| InboxAuthError::missing_signature())
}

/// The `Authorization` header's value when it carries the `Signature`
/// scheme; [`parse_signature_header`] drops the scheme itself.
fn authorization_signature_text(headers: &HeaderMap) -> Option<&str> {
    let value = header_text(headers, &header::AUTHORIZATION)?;
    value
        .trim_start()
        .split_once(char::is_whitespace)
        .is_some_and(|(scheme, _rest)| scheme.eq_ignore_ascii_case(SIGNATURE_SCHEME))
        .then_some(value)
}

/// The text of header `name`, `None` when absent or not visible ASCII.
fn header_text<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// The `keyId` minus its fragment: Mastodon's `#main-key` names a key
/// embedded in the actor document, which is addressed by the bare URL.
pub(super) fn strip_fragment(key_id: &str) -> &str {
    key_id.split_once('#').map_or(key_id, |(base, _)| base)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn strip_fragment_cuts_the_key_fragment() {
        assert_eq!(
            strip_fragment("https://peer.example/users/alice#main-key"),
            "https://peer.example/users/alice",
            "Mastodon's embedded-key spelling"
        );
        assert_eq!(
            strip_fragment("https://peer.example/users/alice"),
            "https://peer.example/users/alice",
            "a bare actor url as keyId is unchanged"
        );
    }

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

    #[test]
    fn the_authorization_signature_scheme_is_recognised() {
        for value in [
            r#"Signature keyId="k""#,
            r#"signature keyId="k""#,
            r#"  SIGNATURE keyId="k""#,
        ] {
            let mut headers = HeaderMap::new();
            let previous =
                headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            drop(previous);
            assert_eq!(
                authorization_signature_text(&headers),
                Some(value),
                "`{value}` is a Signature authorization"
            );
        }
        for value in ["Bearer tok3n", "Basic dXNlcjpwYXNz", "Signature"] {
            let mut headers = HeaderMap::new();
            let previous =
                headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            drop(previous);
            assert_eq!(
                authorization_signature_text(&headers),
                None,
                "`{value}` presents no Signature parameters"
            );
        }
    }
}
