//! Unit-test fixtures shared by the `html_pages` children: the header
//! reader every page test asserts through.

use axum::{http::HeaderMap, response::Response};

/// The headers of `response` as a debuggable map.
pub(super) fn headers_of(response: &Response) -> HeaderMap {
    response.headers().clone()
}
