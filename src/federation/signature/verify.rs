//! Inbound HTTP Signatures: verifying a peer's
//! `draft-cavage-http-signatures-12` authentication.
//!
//! Pleroma verifies through `lib/pleroma/signature.ex`, Mastodon through
//! `app/lib/http_signature_verification.rb`; this module accepts what either
//! sends. A received request carries a `Signature` header — or the same
//! parameters under the `Authorization: Signature ...` scheme — whose
//! [`ParsedSignature`] names the key, the signed header list and the base64
//! RSA signature. [`verify`] rebuilds the signing text from the request
//! through the shared [`signing_string`], in the order the sender's
//! `headers` parameter gives, and checks it against the sender's public
//! key; [`verify_digest`] separately checks the `Digest` header against the
//! body actually received.
//!
//! The rules, following both implementations:
//!
//! - `(request-target)` must be among the signed headers, `host` too, and
//!   one of `date` or the `(created)` pseudo-header (Pleroma's Mastodon API
//!   uses `(created)` when the HTTP client already owns the `Date` header).
//! - a `POST` must also sign `digest`, so the activity body is covered.
//! - the signed moment must be within ±12 hours of `now` (Mastodon's
//!   tolerance), and a `(expires)` parameter in the past rejects the
//!   request.
//! - the algorithm is `rsa-sha256` or `hs2019`; `hs2019` means "the
//!   algorithm comes from the key", and fediverse keys are all RSA, where
//!   both mean RSASSA-PKCS1-v1_5 over SHA-256. An absent parameter is the
//!   draft's default, `rsa-sha256`.
//! - the signing text is first rebuilt with the path *including* its
//!   query, as the newer drafts and Mastodon sign it, and — on failure —
//!   without it, as Pleroma sometimes omits it.

use std::{error::Error, fmt};

use axum::http::HeaderMap;
use chrono::{DateTime, TimeDelta, Utc};

mod checks;
mod digest;
mod freshness;
mod parse;
#[cfg(test)]
mod test_support;

pub use digest::verify_digest;
pub use parse::{ParsedSignature, parse_signature_header};

use self::{
    checks::{check_algorithm, check_signed_headers, signing_text, target_candidates},
    freshness::check_freshness,
};
#[cfg(doc)]
use super::{ALGORITHM, signing_string};
use crate::util::keys::verify_sha256;

/// `Authorization` scheme a `Signature` header value may arrive under.
const SCHEME: &str = "Signature";
/// The pseudo-header every fediverse signature covers: method + target.
const REQUEST_TARGET: &str = "(request-target)";
/// Pleroma's stand-in for `date` when the `Date` header is not signed.
const CREATED: &str = "(created)";
/// Pseudo-header whose unix seconds, once past, reject the signature.
const EXPIRES: &str = "(expires)";
/// Header names the signature rules refer to, lowercase.
const HOST_HEADER: &str = "host";
/// See [`HOST_HEADER`].
const DATE_HEADER: &str = "date";
/// See [`HOST_HEADER`].
const DIGEST_HEADER: &str = "digest";
/// The method whose requests must sign the body's digest.
const POST_METHOD: &str = "POST";
/// The algorithm [`hs2019`] negotiates from the key for RSA keys, which
/// every fediverse key is: the same as the outbound [`ALGORITHM`].
///
/// [`hs2019`]: https://datatracker.ietf.org/doc/html/draft-cavage-http-signatures-12#section-2.1.6
const HS2019: &str = "hs2019";
/// How far in the past the signed moment may lie: 12 hours, as in Mastodon.
const DATE_TOLERANCE: TimeDelta = TimeDelta::hours(12);
/// How far in the future the signed moment may lie: one hour of clock skew,
/// as in Mastodon's `CLOCK_SKEW_MARGIN`.
const CLOCK_SKEW_MARGIN: TimeDelta = TimeDelta::hours(1);

/// [`SignatureError::Malformed`] for a parameter that has no `=`.
const MALFORMED_PAIR: SignatureError = SignatureError::Malformed("a parameter without `=`");
/// [`SignatureError::Malformed`] for a parameter name that is not a token.
const MALFORMED_NAME: SignatureError =
    SignatureError::Malformed("a parameter name that is not a token");
/// [`SignatureError::Malformed`] for a quoted value that never closes.
const MALFORMED_QUOTE: SignatureError = SignatureError::Malformed("an unterminated quoted value");
/// [`SignatureError::Malformed`] for a signature or digest that is not base64.
const MALFORMED_BASE64: SignatureError = SignatureError::Malformed("the value is not base64");
/// [`SignatureError::Malformed`] for a non-numeric `created`/`expires`.
const MALFORMED_TIMESTAMP: SignatureError =
    SignatureError::Malformed("a timestamp parameter that is not an integer");
/// [`SignatureError::Malformed`] for a signed `Date` header that is no date.
const MALFORMED_DATE: SignatureError =
    SignatureError::Malformed("the Date header is not a timestamp");
/// [`SignatureError::Malformed`] for a `Digest` header in no known shape.
const MALFORMED_DIGEST: SignatureError =
    SignatureError::Malformed("the Digest header is neither `scheme=value` nor `scheme=:value:`");

/// One way inbound signature parsing or verification can fail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SignatureError {
    /// A required `Signature` parameter, signed header or `Digest` header
    /// was not present at all.
    Missing,
    /// Something was present but unparsable; the payload names the part.
    Malformed(&'static str),
    /// The `(expires)` moment has passed, or the signed moment is further
    /// from `now` than Mastodon's ±12-hour tolerance.
    Expired,
    /// The `Digest` header does not describe the request body.
    BadDigest,
    /// The cryptographic check against the sender's public key failed.
    BadSignature,
    /// The signature (or digest) names an algorithm other than
    /// `rsa-sha256` or `hs2019` (or `SHA-256` for digests).
    UnsupportedAlgorithm,
}

/// `Display` states the failure; every payload is log-safe: no key
/// material, no request secrets.
impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => {
                f.write_str("a required signature parameter or signed header is missing")
            }
            Self::Malformed(detail) => write!(f, "malformed signature: {detail}"),
            Self::Expired => {
                f.write_str("the signature is expired or outside the accepted time window")
            }
            Self::BadDigest => f.write_str("the digest does not match the request body"),
            Self::BadSignature => {
                f.write_str("the signature does not verify against the public key")
            }
            Self::UnsupportedAlgorithm => f.write_str("the signature algorithm is not supported"),
        }
    }
}

impl Error for SignatureError {}

/// The parts of a received request [`verify`] needs to rebuild the signing
/// text. The body is deliberately not here: [`verify_digest`] checks it
/// against the `Digest` header, while the signature covers the headers.
#[derive(Clone, Debug)]
pub struct SignedRequestParts<'a> {
    /// The method as received, e.g. `POST`; matched case-insensitively.
    pub method: &'a str,
    /// The request target as received: the path with its query string.
    pub path_and_query: &'a str,
    /// The request headers, where the signed values are looked up.
    pub headers: HeaderMap,
}

/// Verifies a parsed inbound signature against the received request.
///
/// Checks the algorithm, the mandatory signed headers and the time rules
/// (see the [module docs][self]), then rebuilds the signing text from
/// `parsed.headers` with [`signing_string`] — the request target *with*
/// its query first, the path alone as a second candidate when the target
/// carries a query — and verifies the RSA-SHA256 signature over it with
/// `public_pem` at `now`.
///
/// # Errors
///
/// [`SignatureError::UnsupportedAlgorithm`], [`SignatureError::Missing`],
/// [`SignatureError::Malformed`] or [`SignatureError::Expired`] when the
/// signature breaks a rule; [`SignatureError::BadSignature`] when its text
/// does not verify against `public_pem` — including when that key itself
/// does not parse.
pub fn verify(
    parsed: &ParsedSignature,
    request: &SignedRequestParts<'_>,
    public_pem: &str,
    now: DateTime<Utc>,
) -> Result<(), SignatureError> {
    check_algorithm(parsed.algorithm.as_deref())?;
    check_signed_headers(parsed, request)?;
    check_freshness(parsed, request, now)?;
    for target in target_candidates(request.path_and_query) {
        let text = signing_text(parsed, request, target)?;
        if verify_sha256(public_pem, text.as_bytes(), &parsed.signature) {
            return Ok(());
        }
    }
    Err(SignatureError::BadSignature)
}

#[cfg(test)]
mod tests {
    use super::test_support::{BODY, KEY_ID, now, round_trip, verify_now};
    use super::*;
    use crate::util::keys::FIXTURE_PRIVATE_PKCS1_PEM;

    #[test]
    fn a_signed_get_verifies_and_parses_completely() {
        let (parsed, request) = round_trip(
            "GET",
            "https://peer.example/users/alice",
            "/users/alice",
            None,
            now(),
        );
        assert_eq!(
            verify_now(&parsed, &request),
            Ok(()),
            "sign then verify round-trips"
        );
        assert_eq!(parsed.key_id, KEY_ID, "the key id round-trips");
        assert_eq!(
            parsed.algorithm.as_deref(),
            Some("rsa-sha256"),
            "the algorithm round-trips"
        );
        assert_eq!(
            parsed.headers,
            ["(request-target)", "host", "date"].map(str::to_owned),
            "the signed header list round-trips"
        );
        assert_eq!(
            parsed.signature.len(),
            128,
            "an RSA-1024 signature is 128 bytes"
        );
        assert_eq!(parsed.created, None, "sign_request sends no created");
        assert_eq!(parsed.expires, None, "and no expires");
    }

    #[test]
    fn a_signed_post_verifies_and_its_digest_matches_the_body() {
        let (parsed, request) = round_trip(
            "POST",
            "https://peer.example/inbox",
            "/inbox",
            Some(BODY),
            now(),
        );
        assert_eq!(
            verify_now(&parsed, &request),
            Ok(()),
            "the four-line POST round-trips"
        );
        assert_eq!(
            verify_digest(&request.headers, BODY),
            Ok(()),
            "the digest describes the body"
        );
    }

    #[test]
    fn the_wrong_key_fails_to_verify() {
        let (parsed, request) = round_trip(
            "GET",
            "https://peer.example/users/alice",
            "/users/alice",
            None,
            now(),
        );
        assert_eq!(
            verify(&parsed, &request, FIXTURE_PRIVATE_PKCS1_PEM, now()),
            Err(SignatureError::BadSignature),
            "a private key is not its public key and never verifies"
        );
        assert_eq!(
            verify(&parsed, &request, "not a public key at all", now()),
            Err(SignatureError::BadSignature),
            "and neither does unparseable key material"
        );
    }
}
