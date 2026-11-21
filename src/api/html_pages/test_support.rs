//! Unit-test fixtures shared by the `html_pages` children: the header
//! reader every page test asserts through, and the fixed-instant
//! attachment the card and player tests build their cases on.

use axum::{http::HeaderMap, response::Response};
use chrono::{DateTime, TimeZone as _, Utc};
use uuid::Uuid;

use crate::models::media_attachment::MediaAttachment;

/// The headers of `response` as a debuggable map.
pub(super) fn headers_of(response: &Response) -> HeaderMap {
    response.headers().clone()
}

/// The fixed publication instant of the card fixtures:
/// `2024-01-31T12:34:56.000Z` in the Mastodon spelling.
pub(super) fn published() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 31, 12, 34, 56).unwrap()
}

/// A minimal attachment row of `content_type` at `url`, at `position`
/// and with `description` as alt text: everything but the fields the
/// renderer reads.
pub(super) fn attachment(
    content_type: &str,
    url: &str,
    position: i32,
    description: Option<&str>,
) -> MediaAttachment {
    MediaAttachment {
        id: Uuid::now_v7(),
        user_id: None,
        status_id: None,
        position,
        storage_key: None,
        remote_url: None,
        url: url.to_owned(),
        preview_url: None,
        content_type: content_type.to_owned(),
        description: description.map(str::to_owned),
        blurhash: None,
        width: None,
        height: None,
        preview_width: None,
        preview_height: None,
        duration: None,
        focus_x: None,
        focus_y: None,
        file_size: None,
        sensitive_hint: false,
        inserted_at: published(),
        updated_at: published(),
    }
}
