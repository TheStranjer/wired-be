//! Fixtures shared by the `ActivityPub` unit tests: the `Accept`
//! header builder the negotiation tests hand their cases through.

use axum::http::{HeaderMap, HeaderValue, header};

/// A [`HeaderMap`] holding only `Accept: <accept>` (`None` sends a map
/// with no `Accept` at all).
pub(super) fn headers_with_accept(accept: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(accept) = accept {
        let value = HeaderValue::from_str(accept).unwrap();
        let previous = headers.insert(header::ACCEPT, value);
        drop(previous);
    }
    headers
}
