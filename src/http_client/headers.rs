//! The request-building helpers behind [`HttpClient`](super::HttpClient):
//! assembling a [`HeaderMap`] from `(name, value)` pairs, signing a
//! request for the federation verbs, spelling out the header pairs a
//! signed exchange carries, and reading a response body under a byte cap.

use bytes::BytesMut;
use chrono::Utc;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::federation::signature::{SignedHeaders, SigningKey, sign_request};

use super::{ACTIVITY_JSON, HttpError, HttpResponse};

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

/// Signs `method url` [`at the current moment`](Utc::now), folding a
/// signing failure into [`HttpError::Connect`]: the request never left
/// this process because it could not be built.
///
/// # Errors
///
/// [`HttpError::Connect`] if [`sign_request`] rejects `url` or
/// `private_pem`, or the RSA operation fails.
pub(super) fn sign_for_send(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    key_id: &str,
    private_pem: &str,
) -> Result<SignedHeaders, HttpError> {
    let key = SigningKey {
        key_id,
        private_pem,
    };
    sign_request(method, url, body, &key, Utc::now())
        .map_err(|err| HttpError::Connect(format!("could not sign the request: {err}")))
}

/// The header pairs of a signed federation exchange: `Accept` (and, with
/// `with_body`, the `Content-Type` the body is labelled by) plus the
/// signed `Host`, `Date`, optional `Digest` and `Signature`. `Host` is
/// sent explicitly so the peer sees exactly the host that was signed.
#[must_use]
pub(super) fn ap_headers(signed: &SignedHeaders, with_body: bool) -> Vec<(&str, &str)> {
    let mut headers = vec![("accept", ACTIVITY_JSON)];
    if with_body {
        headers.push(("content-type", ACTIVITY_JSON));
    }
    headers.push(("host", &signed.host));
    headers.push(("date", &signed.date));
    if let Some(digest) = &signed.digest {
        headers.push(("digest", digest));
    }
    headers.push(("signature", &signed.signature));
    headers
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
    use axum::{
        Router,
        routing::{get, post},
    };
    use serde_json::Value;

    use super::*;
    use crate::{
        federation::signature::digest_header,
        http_client::{HttpClient, test_support::*},
        util::keys::{FIXTURE_PRIVATE_PKCS1_PEM, FIXTURE_PUBLIC_PKCS1_PEM, verify_sha256},
    };

    #[tokio::test]
    async fn signed_get_sends_activity_json_and_a_verifiable_signature() {
        let listener = bind().await;
        let url = url_for(&listener, "/actor?page=2");
        let server = spawn_server(
            listener,
            Router::new().route("/actor", get(echo_ap_request)),
        );
        let client = HttpClient::new_for_tests(TEST_UA);

        let response = client
            .signed_get(&url, TEST_KEY_ID, FIXTURE_PRIVATE_PKCS1_PEM, 4096)
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let seen: Value = response.json().unwrap();
        server.abort();

        assert_eq!(seen["accept"], ACTIVITY_JSON, "asks for AP JSON");
        assert!(
            seen.get("content-type").is_none(),
            "a GET sends no body to label"
        );
        assert_eq!(
            seen["request-target"], "get /actor?page=2",
            "the peer sees method and path with the query"
        );
        assert!(seen.get("digest").is_none(), "a GET signs no digest");
        assert!(
            seen["date"].as_str().unwrap().ends_with(" GMT"),
            "an IMF-fixdate date"
        );
        let signature = observed_signature(seen["signature"].as_str().unwrap(), GET_HEADERS_PARAM);
        assert!(
            verify_sha256(
                FIXTURE_PUBLIC_PKCS1_PEM,
                peer_signing_string(&seen).as_bytes(),
                &signature,
            ),
            "the signature verifies over exactly what the peer saw"
        );
    }

    #[tokio::test]
    async fn signed_get_sends_the_host_it_signed() {
        let listener = bind().await;
        let url = url_for(&listener, "/actor");
        let port = listener.local_addr().unwrap().port();
        let server = spawn_server(
            listener,
            Router::new().route("/actor", get(echo_ap_request)),
        );
        let client = HttpClient::new_for_tests(TEST_UA);

        let response = client
            .signed_get(&url, TEST_KEY_ID, FIXTURE_PRIVATE_PKCS1_PEM, 4096)
            .await
            .unwrap();
        let seen: Value = response.json().unwrap();
        server.abort();

        assert_eq!(
            seen["host"],
            format!("127.0.0.1:{port}"),
            "the non-default port rides into the signed host"
        );
    }

    #[tokio::test]
    async fn signed_post_adds_digest_and_content_type_to_the_signature() {
        let listener = bind().await;
        let url = url_for(&listener, "/inbox");
        let server = spawn_server(
            listener,
            Router::new().route("/inbox", post(echo_ap_request)),
        );
        let client = HttpClient::new_for_tests(TEST_UA);
        let body = br#"{"type":"Create"}"#;

        let response = client
            .signed_post(&url, body, TEST_KEY_ID, FIXTURE_PRIVATE_PKCS1_PEM)
            .await
            .unwrap();
        let seen: Value = response.json().unwrap();
        server.abort();

        assert_eq!(seen["accept"], ACTIVITY_JSON, "asks for AP JSON");
        assert_eq!(
            seen["content-type"], ACTIVITY_JSON,
            "labels the activity body"
        );
        assert_eq!(
            seen["digest"],
            digest_header(body),
            "the peer sees the digest we signed"
        );
        let signature = observed_signature(seen["signature"].as_str().unwrap(), POST_HEADERS_PARAM);
        assert!(
            verify_sha256(
                FIXTURE_PUBLIC_PKCS1_PEM,
                peer_signing_string(&seen).as_bytes(),
                &signature,
            ),
            "the four-line signing string verifies"
        );
    }

    #[tokio::test]
    async fn unsignable_requests_fail_before_any_connection() {
        let client = HttpClient::new_for_tests(TEST_UA);
        let bad_url = client
            .signed_get(
                "ftp://example.com/pub",
                TEST_KEY_ID,
                FIXTURE_PRIVATE_PKCS1_PEM,
                4096,
            )
            .await;
        assert!(
            matches!(bad_url, Err(HttpError::Connect(_))),
            "a non-http(s) URL cannot be signed, got {bad_url:?}"
        );
        let junk_key = client
            .signed_get("https://peer.invalid/actor", TEST_KEY_ID, "not a key", 4096)
            .await;
        assert!(
            matches!(junk_key, Err(HttpError::Connect(_))),
            "unparseable key material fails the same way, got {junk_key:?}"
        );
    }

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
