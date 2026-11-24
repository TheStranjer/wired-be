//! How a JSON request is told from a browser's.
//!
//! `wants_json` reads the `Accept` header that asks `GET /oauth/authorize`
//! and `GET /oauth/registration_details` for JSON instead of a redirect or
//! a page; `is_json_media_type` reads one media type, such as a body's.
//! Shared by the JSON half of `/oauth/authorize` and
//! [`crate::api::oauth_external`]'s frontend endpoints.

use axum::http::{HeaderMap, header::ACCEPT};

/// The one media type a JSON request or `Accept` header names.
const JSON_MEDIA_TYPE: &str = "application/json";

/// Whether `value` names `application/json`: its media type (parameters
/// dropped) compared case-insensitively, exactly.
pub(crate) fn is_json_media_type(value: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|media| media.trim().eq_ignore_ascii_case(JSON_MEDIA_TYPE))
}

/// Whether the `Accept` header lists exactly `application/json`.
pub(crate) fn wants_json(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(is_json_media_type)
}
