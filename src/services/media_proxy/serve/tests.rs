//! Tests for the proxy route's response shaping and header forwarding.

use axum::{
    http::HeaderValue,
    response::{IntoResponse as _, Response},
};

use super::*;

/// An [`HttpResponse`] for the response-shaping helpers.
fn upstream(status: u16, headers: &[(&str, &str)], body: &[u8]) -> HttpResponse {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::try_from(*name).unwrap();
        drop(map.insert(name, HeaderValue::from_str(value).unwrap()));
    }
    HttpResponse {
        status,
        headers: map,
        url: url::Url::parse("http://test.example/").unwrap(),
        body: bytes::Bytes::copy_from_slice(body),
    }
}

/// Renders a handler error the way axum would.
fn render(err: AppError) -> Response {
    err.into_response()
}

#[test]
fn only_media_top_level_types_are_served() {
    for (content_type, media) in [
        ("image/png", true),
        ("VIDEO/MP4; codecs=avc1", true),
        (" audio/ogg ", true),
        ("text/html", false),
        ("application/json", false),
        ("", false),
        ("image", false),
    ] {
        assert_eq!(
            is_media_type(content_type),
            media,
            "{content_type:?} proxiable: {media}"
        );
    }
}

#[test]
fn media_response_forces_inline_and_the_safety_headers() {
    let up = upstream(
        200,
        &[
            ("content-type", "image/png"),
            ("etag", "\"v1\""),
            ("last-modified", "Sun, 01 Jun 2025 00:00:00 GMT"),
            ("content-disposition", "attachment; filename=\"x\""),
        ],
        b"fake-png",
    );
    let response = media_response(&up).unwrap();
    assert_eq!(response.status(), StatusCode::OK, "upstream status kept");
    let headers = response.headers();
    assert_eq!(headers[header::CONTENT_TYPE], "image/png", "type passes");
    assert_eq!(
        headers[header::CONTENT_DISPOSITION],
        INLINE,
        "inline is forced over the upstream attachment"
    );
    assert_eq!(
        headers[header::CACHE_CONTROL],
        CACHE_CONTROL,
        "one day of cache"
    );
    assert_eq!(
        headers[header::X_CONTENT_TYPE_OPTIONS],
        NOSNIFF,
        "no sniffing"
    );
    assert_eq!(
        headers[header::CONTENT_SECURITY_POLICY],
        CSP,
        "the locked-down CSP"
    );
    assert_eq!(headers[header::ETAG], "\"v1\"", "validators ride along");
    assert_eq!(
        headers[header::LAST_MODIFIED],
        "Sun, 01 Jun 2025 00:00:00 GMT",
        "last-modified passes"
    );
}

#[test]
fn media_response_passes_partial_content_through() {
    let up = upstream(
        206,
        &[
            ("content-type", "video/webm"),
            ("content-range", "bytes 0-3/10"),
        ],
        b"webm",
    );
    let response = media_response(&up).unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT, "206 kept");
    assert_eq!(
        response.headers()[header::CONTENT_RANGE],
        "bytes 0-3/10",
        "the range answer passes through"
    );
}

#[test]
fn non_media_types_are_answered_424() {
    for content_type in ["text/html", "application/octet-stream", ""] {
        let up = upstream(200, &[("content-type", content_type)], b"x");
        let err = media_response(&up).unwrap_err();
        assert!(
            matches!(err, AppError::FailedDependency(_)),
            "`{content_type:?}` is not media, got {err:?}"
        );
        assert_eq!(
            err.to_string(),
            UNSUPPORTED_MESSAGE,
            "the body message is the ticket's"
        );
        assert_eq!(
            render(err).status(),
            StatusCode::FAILED_DEPENDENCY,
            "`{content_type:?}` answers 424"
        );
    }
}

#[test]
fn not_modified_responses_keep_the_validators_without_a_type() {
    let up = upstream(304, &[("etag", "\"v1\"")], b"");
    let response = not_modified_response(&up).unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "304 kept");
    assert_eq!(
        response.headers().get(header::ETAG),
        Some(&HeaderValue::from_static("\"v1\"")),
        "the revalidated etag comes back"
    );
    assert!(
        !response.headers().contains_key(header::CONTENT_TYPE),
        "a 304 carries no content type"
    );
}

#[test]
fn upstream_statuses_map_per_the_ticket() {
    let cases: [(u16, StatusCode); 5] = [
        (404, StatusCode::NOT_FOUND),
        (500, StatusCode::BAD_GATEWAY),
        (503, StatusCode::BAD_GATEWAY),
        (418, StatusCode::BAD_GATEWAY),
        (302, StatusCode::BAD_GATEWAY),
    ];
    for (status, want) in cases {
        let up = upstream(status, &[("content-type", "image/png")], b"");
        let rendered = match upstream_response(&up) {
            Ok(response) => response,
            Err(err) => render(err),
        };
        assert_eq!(rendered.status(), want, "upstream {status} becomes {want}");
    }
}

#[test]
fn only_range_and_validator_request_headers_are_forwarded() {
    let mut headers = HeaderMap::new();
    drop(headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-1")));
    drop(headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"v1\"")));
    drop(headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer secret"),
    ));
    drop(headers.insert(header::COOKIE, HeaderValue::from_static("session=x")));
    assert_eq!(
        forwarded_headers(&headers),
        vec![("range", "bytes=0-1"), ("if-none-match", "\"v1\"")],
        "credentials never reach the upstream, range and revalidation do"
    );
}
