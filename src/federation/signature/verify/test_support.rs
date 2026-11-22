//! Fixtures the signature verification unit tests share: the fixed clock,
//! the fixture key and the signed-request builders.

use axum::http::{HeaderMap, HeaderValue, header::DATE};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Utc};

use super::super::{SignedHeaders, SigningKey, sign_request, signing_string};
use super::{
    DATE_HEADER, HOST_HEADER, REQUEST_TARGET, SignatureError, SignedRequestParts,
    parse::{ParsedSignature, parse_signature_header},
    verify,
};
use crate::util::{
    keys::{FIXTURE_PRIVATE_PKCS1_PEM, FIXTURE_PUBLIC_PKCS1_PEM, sign_sha256},
    time::parse_time,
};

/// The key id every signature in these tests presents.
pub(super) const KEY_ID: &str = "https://wired.example/actor#main-key";
/// The fixed signing moment (`2022-06-07T20:51:35Z`).
pub(super) const NOW_RFC3339: &str = "2022-06-07T20:51:35Z";
/// The same moment as unix seconds.
pub(super) const NOW_SECONDS: i64 = 1_654_635_095;
/// The same moment as the `Date` header spelling.
pub(super) const NOW_FIXDATE: &str = "Tue, 07 Jun 2022 20:51:35 GMT";
/// The host every round-tripped URL points at.
pub(super) const HOST: &str = "peer.example";
/// A body with a well-known SHA-256 digest.
pub(super) const BODY: &[u8] = b"hello";
/// `SHA-256=<base64 of sha256("hello")>`, as [`sign_request`] sends it.
pub(super) const HELLO_DIGEST: &str = "SHA-256=LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=";
/// The base64 inside [`HELLO_DIGEST`].
pub(super) const HELLO_B64: &str = "LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=";
/// Base64 of the bytes `hi`, for parameter-only parse tests.
pub(super) const HI_B64: &str = "aGk=";

/// The fixed [`NOW_RFC3339`] as the verification clock.
pub(super) fn now() -> DateTime<Utc> {
    parse_time(NOW_RFC3339).unwrap()
}

/// The [`KEY_ID`] identity backed by the fast fixture private key.
pub(super) fn fixture_key() -> SigningKey<'static> {
    SigningKey {
        key_id: KEY_ID,
        private_pem: FIXTURE_PRIVATE_PKCS1_PEM,
    }
}

/// The headers a received signed request carries: the `Signature`
/// text, the optional IMF-fixdate `Date`, the fixed [`HOST`] and the
/// optional `Digest`.
pub(super) fn received_headers(
    signature: &str,
    date: Option<&str>,
    digest: Option<&str>,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(value) = date {
        let _date: Option<HeaderValue> =
            headers.insert(DATE, HeaderValue::from_str(value).unwrap());
    }
    let _host: Option<HeaderValue> = headers.insert("host", HeaderValue::from_str(HOST).unwrap());
    if let Some(value) = digest {
        let _digest: Option<HeaderValue> =
            headers.insert("digest", HeaderValue::from_str(value).unwrap());
    }
    let _signature: Option<HeaderValue> =
        headers.insert("signature", HeaderValue::from_str(signature).unwrap());
    headers
}

/// Signs one request with [`sign_request`] and assembles exactly what
/// the peer would hand to [`verify`]: the parsed `Signature` header
/// and the request parts it arrived with.
pub(super) fn round_trip<'a>(
    method: &'a str,
    url: &str,
    path_and_query: &'a str,
    body: Option<&[u8]>,
    then: DateTime<Utc>,
) -> (ParsedSignature, SignedRequestParts<'a>) {
    let signed = sign_request(method, url, body, &fixture_key(), then).unwrap();
    let parsed = parse_signature_header(&signed.signature).unwrap();
    (parsed, parts_for(&signed, method, path_and_query))
}

/// Rebuilds the received request `signed` authenticates.
pub(super) fn parts_for<'a>(
    signed: &SignedHeaders,
    method: &'a str,
    path_and_query: &'a str,
) -> SignedRequestParts<'a> {
    SignedRequestParts {
        method,
        path_and_query,
        headers: received_headers(
            &signed.signature,
            Some(&signed.date),
            signed.digest.as_deref(),
        ),
    }
}

/// A `Signature` header value over `lines`, signed with the fixture
/// key; `params` is spliced after `keyId` verbatim (e.g.
/// `,algorithm="hs2019"` or `,created=1654635095`, comma included).
pub(super) fn crafted_signature(
    headers_param: &str,
    lines: &[(&str, &str)],
    params: &str,
) -> String {
    let raw = sign_sha256(FIXTURE_PRIVATE_PKCS1_PEM, signing_string(lines).as_bytes()).unwrap();
    format!(
        r#"keyId="{KEY_ID}"{params},headers="{headers_param}",signature="{}""#,
        BASE64.encode(raw),
    )
}

/// The canonical three-line GET signing entries at [`NOW_FIXDATE`].
pub(super) fn get_lines(target: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![
        (REQUEST_TARGET, target),
        (HOST_HEADER, HOST),
        (DATE_HEADER, NOW_FIXDATE),
    ]
}

/// Verifies `parsed` against `request` with the fixture public key at
/// [`now`].
pub(super) fn verify_now(
    parsed: &ParsedSignature,
    request: &SignedRequestParts<'_>,
) -> Result<(), SignatureError> {
    verify(parsed, request, FIXTURE_PUBLIC_PKCS1_PEM, now())
}

/// A `HeaderMap` carrying only the given `Digest` value.
pub(super) fn digest_headers(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let _digest: Option<HeaderValue> =
        headers.insert("digest", HeaderValue::from_str(value).unwrap());
    headers
}
