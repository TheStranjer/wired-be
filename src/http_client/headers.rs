//! The request-building helpers behind [`HttpClient`](super::HttpClient):
//! assembling a [`HeaderMap`] from `(name, value)` pairs and reading a
//! response body under a byte cap.

use bytes::BytesMut;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use super::{HttpError, HttpResponse};

/// Collects `(name, value)` pairs into a header map; a repeated name keeps
/// its last value.
///
/// # Errors
///
/// [`HttpError::Connect`] naming the first entry HTTP cannot represent.
pub(super) fn header_map(headers: &[(&str, &str)]) -> Result<HeaderMap, HttpError> {
    let mut map = HeaderMap::with_capacity(headers.len());
    for &(key, value) in headers {
        let name = HeaderName::try_from(key)
            .map_err(|err| HttpError::Connect(format!("invalid header name `{key}`: {err}")))?;
        let value = HeaderValue::from_str(value).map_err(|err| {
            HttpError::Connect(format!("invalid value for header `{key}`: {err}"))
        })?;
        let replaced = map.insert(name, value);
        drop(replaced);
    }
    Ok(map)
}

/// Reads `response`'s body incrementally into memory, stopping with
/// [`HttpError::TooLarge`] once more than `max_bytes` have arrived.
pub(super) async fn read_body(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<HttpResponse, HttpError> {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let url = response.url().clone();
    let mut body = BytesMut::new();
    while let Some(chunk) = response.chunk().await? {
        body.extend_from_slice(&chunk);
        if body.len() > max_bytes {
            return Err(HttpError::TooLarge);
        }
    }
    Ok(HttpResponse {
        status,
        headers,
        url,
        body: body.freeze(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_client::{HttpClient, test_support::*};

    #[tokio::test]
    async fn unsendable_headers_fail_as_connect() {
        let client = HttpClient::new_for_tests(TEST_UA);
        let result = client
            .get_limited("https://example.com/x", &[("x bad name", "ok")], 1024)
            .await;
        assert!(
            matches!(result, Err(HttpError::Connect(_))),
            "a header HTTP cannot represent never leaves the process"
        );
    }
}
