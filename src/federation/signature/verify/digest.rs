//! Checking the `Digest` header against the body actually received.

use axum::http::HeaderMap;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use sha2::{Digest as _, Sha256};

use super::super::DIGEST_SCHEME;
use super::{DIGEST_HEADER, MALFORMED_BASE64, MALFORMED_DIGEST, SignatureError};

/// Checks the request's `Digest` header against the received `body`.
///
/// Accepts the cavage spelling, `SHA-256=<base64>` in any case of the
/// scheme, and the RFC 9530 structured-field spelling,
/// `sha-256=:<base64>:`. A missing `Digest` header is an error exactly for
/// requests that carry a body: bodyless requests never send one, so an
/// empty `body` with no header verifies.
///
/// # Errors
///
/// [`SignatureError::Missing`] for a headerless request with a body,
/// [`SignatureError::Malformed`] for an unparsable header,
/// [`SignatureError::UnsupportedAlgorithm`] for a scheme other than
/// `SHA-256`, [`SignatureError::BadDigest`] when the digest does not
/// describe `body`.
pub fn verify_digest(headers: &HeaderMap, body: &[u8]) -> Result<(), SignatureError> {
    let Some(value) = headers.get(DIGEST_HEADER) else {
        return if body.is_empty() {
            Ok(())
        } else {
            Err(SignatureError::Missing)
        };
    };
    let Ok(text) = value.to_str() else {
        return Err(SignatureError::Malformed(
            "the Digest header is not visible ASCII",
        ));
    };
    let (scheme, encoded) = split_digest(text)?;
    if !scheme.eq_ignore_ascii_case(DIGEST_SCHEME) {
        return Err(SignatureError::UnsupportedAlgorithm);
    }
    let claimed = BASE64.decode(encoded).map_err(|_err| MALFORMED_BASE64)?;
    let actual: [u8; 32] = Sha256::digest(body).into();
    if claimed == actual {
        return Ok(());
    }
    Err(SignatureError::BadDigest)
}

/// The scheme and base64 text of one `Digest` header value, in either the
/// `scheme=value` or the RFC 9530 `scheme=:value:` spelling.
fn split_digest(text: &str) -> Result<(&str, &str), SignatureError> {
    match text.trim().split_once('=') {
        None => Err(MALFORMED_DIGEST),
        Some((scheme, rest)) if rest.starts_with(':') => {
            let encoded = rest
                .strip_prefix(':')
                .and_then(|value| value.strip_suffix(':'))
                .ok_or(MALFORMED_DIGEST)?;
            Ok((scheme, encoded))
        }
        Some((scheme, encoded)) => Ok((scheme, encoded.trim_end())),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::super::test_support::{
        BODY, HELLO_B64, digest_headers, now, round_trip, verify_now,
    };
    use super::*;

    #[test]
    fn a_tampered_body_fails_the_digest_check() {
        let (_, request) = round_trip(
            "POST",
            "https://peer.example/inbox",
            "/inbox",
            Some(BODY),
            now(),
        );
        assert_eq!(
            verify_digest(&request.headers, b"hellp"),
            Err(SignatureError::BadDigest),
            "one flipped byte breaks the digest"
        );
        assert_eq!(
            verify_digest(&request.headers, BODY),
            Ok(()),
            "and the original body still matches"
        );
    }

    #[test]
    fn a_tampered_digest_header_fails_both_checks() {
        let (parsed, mut request) = round_trip(
            "POST",
            "https://peer.example/inbox",
            "/inbox",
            Some(BODY),
            now(),
        );
        let _digest: Option<HeaderValue> = request.headers.insert(
            "digest",
            HeaderValue::from_static("SHA-256=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="),
        );
        assert_eq!(
            verify_now(&parsed, &request),
            Err(SignatureError::BadSignature),
            "the digest is a signed header"
        );
        assert_eq!(
            verify_digest(&request.headers, BODY),
            Err(SignatureError::BadDigest),
            "and no longer describes the body"
        );
    }

    #[test]
    fn digest_headers_verify_against_the_body_in_either_spelling() {
        for value in [
            format!("SHA-256={HELLO_B64}"),
            format!("sha-256={HELLO_B64}"),
            format!("Sha-256={HELLO_B64}"),
            format!("SHA-256=:{HELLO_B64}:"),
            format!("sha-256=:{HELLO_B64}:"),
        ] {
            assert_eq!(
                verify_digest(&digest_headers(&value), BODY),
                Ok(()),
                "`{value}` describes the body"
            );
        }
    }

    #[test]
    fn a_missing_digest_header_needs_a_body_to_be_an_error() {
        assert_eq!(
            verify_digest(&HeaderMap::new(), BODY),
            Err(SignatureError::Missing),
            "a request with a body must carry a Digest"
        );
        assert_eq!(
            verify_digest(&HeaderMap::new(), b""),
            Ok(()),
            "a bodyless request need not"
        );
    }

    #[test]
    fn other_digest_schemes_and_junk_values_are_rejected() {
        assert_eq!(
            verify_digest(&digest_headers("SHA-512=QUJD"), BODY),
            Err(SignatureError::UnsupportedAlgorithm),
            "only SHA-256 digests are spoken"
        );
        assert_eq!(
            verify_digest(&digest_headers("SHA-256=not base64!"), BODY),
            Err(MALFORMED_BASE64),
            "junk base64"
        );
        assert_eq!(
            verify_digest(&digest_headers("nonsense"), BODY),
            Err(MALFORMED_DIGEST),
            "no scheme separator"
        );
        assert_eq!(
            verify_digest(&digest_headers("sha-256=:unclosed"), BODY),
            Err(MALFORMED_DIGEST),
            "a bracketed value that never closes"
        );
    }
}
