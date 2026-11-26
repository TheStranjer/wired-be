//! The signatures a received request presents, whichever scheme they use.
//!
//! A peer authenticates with a draft-cavage `Signature` header (or
//! `Authorization: Signature ...`), with RFC 9421 `Signature-Input` plus
//! `Signature` headers, or — as Mastodon's double-knock can leave it — with
//! both. [`present_signatures`] lists every one that parses, cavage first;
//! the inbox and the authorized-fetch gate then try them in turn, each
//! through the same key lookup, so a request is accepted when any one of
//! them verifies.

use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use tracing::debug;

use super::{
    ParsedSignature, SignatureError, SignedRequestParts, authorization_signature, bare_signature,
    parse_rfc9421, parse_signature_header, rfc9421::Rfc9421Signature, rfc9421_offered,
    signature_header, verify, verify_content_digest, verify_digest, verify_rfc9421,
};

/// One signature a request presents.
#[derive(Clone, Debug)]
pub enum Presented {
    /// A draft-cavage `Signature` header.
    Cavage(ParsedSignature),
    /// One member of an RFC 9421 `Signature-Input` / `Signature` pair.
    Rfc9421(Rfc9421Signature),
}

impl Presented {
    /// The key the signature claims: the cavage `keyId` or the RFC 9421
    /// `keyid`, which the shared key lookup resolves alike.
    #[must_use]
    pub fn key_id(&self) -> &str {
        match self {
            Self::Cavage(parsed) => &parsed.key_id,
            Self::Rfc9421(signature) => signature.key_id(),
        }
    }

    /// Verifies the signature against the received request with the
    /// scheme's own rules (see [`verify`](super::verify) and
    /// [`verify_rfc9421`]).
    ///
    /// `scheme` is the scheme of the public URL, which only RFC 9421's
    /// `@target-uri` needs.
    ///
    /// # Errors
    ///
    /// The [`SignatureError`] of the scheme's verification.
    pub fn verify(
        &self,
        request: &SignedRequestParts<'_>,
        scheme: &str,
        public_pem: &str,
        now: DateTime<Utc>,
    ) -> Result<(), SignatureError> {
        match self {
            Self::Cavage(parsed) => verify(parsed, request, public_pem, now),
            Self::Rfc9421(signature) => verify_rfc9421(signature, request, scheme, public_pem, now),
        }
    }

    /// Checks the scheme's digest header against the received `body`:
    /// `Digest` for cavage ([`verify_digest`]), `Content-Digest` for
    /// RFC 9421 ([`verify_content_digest`]).
    ///
    /// # Errors
    ///
    /// The [`SignatureError`] of the digest check.
    pub fn verify_body(&self, headers: &HeaderMap, body: &[u8]) -> Result<(), SignatureError> {
        match self {
            Self::Cavage(_) => verify_digest(headers, body),
            Self::Rfc9421(_) => verify_content_digest(headers, body),
        }
    }
}

/// The scheme of the instance's public URL (`https`), which RFC 9421's
/// `@target-uri` needs and a received request does not carry. A URL
/// without a scheme reads as `https`.
#[must_use]
pub fn public_scheme(base_url: &str) -> &str {
    base_url
        .split_once("://")
        .map_or("https", |(scheme, _rest)| scheme)
}

/// Whether the request offers any signature material, parseable or not.
#[must_use]
pub fn signatures_offered(headers: &HeaderMap) -> bool {
    signature_header(headers).is_some() || rfc9421_offered(headers)
}

/// Every signature the request presents that parses, cavage first.
///
/// The cavage one is read from the bare `Signature` header, else from
/// `Authorization: Signature ...`; each RFC 9421 member follows. Empty when
/// nothing parses, which callers treat as no signature at all.
#[must_use]
pub fn present_signatures(headers: &HeaderMap) -> Vec<Presented> {
    let cavage = [bare_signature(headers), authorization_signature(headers)]
        .into_iter()
        .flatten()
        .find_map(|raw| parse_signature_header(raw).ok());
    let rfc9421 = parse_rfc9421(headers).unwrap_or_else(|err| {
        debug!(%err, "the RFC 9421 signature headers could not be read");
        Vec::new()
    });
    cavage
        .into_iter()
        .map(Presented::Cavage)
        .chain(rfc9421.into_iter().map(Presented::Rfc9421))
        .collect()
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderName, HeaderValue, header};

    use super::super::{
        rfc9421_test_support::{Rfc9421Key, sign},
        test_support::{BODY, KEY_ID, NOW_SECONDS, now, round_trip},
    };
    use super::*;
    use crate::util::keys::{FIXTURE_PRIVATE_PKCS1_PEM, FIXTURE_PUBLIC_PKCS1_PEM};

    /// The URL both schemes sign a `POST` of to.
    const URL: &str = "https://peer.example/inbox";

    /// Sets header `name` to `value`.
    fn set(headers: &mut HeaderMap, name: HeaderName, value: &str) {
        let previous = headers.insert(name, HeaderValue::from_str(value).unwrap());
        drop(previous);
    }

    /// A request signed both ways: the cavage `Signature` moved under
    /// `Authorization` (the bare header is the RFC 9421 one) and the RFC 9421
    /// pair beside it.
    fn both() -> SignedRequestParts<'static> {
        let (_, mut request) = round_trip("POST", URL, "/inbox", Some(BODY), now());
        let cavage = request.headers.remove("signature").unwrap();
        set(
            &mut request.headers,
            header::AUTHORIZATION,
            &format!("Signature {}", cavage.to_str().unwrap()),
        );
        let key = Rfc9421Key {
            key_id: KEY_ID,
            private_pem: FIXTURE_PRIVATE_PKCS1_PEM,
            created: NOW_SECONDS,
        };
        for (name, value) in sign("POST", URL, Some(BODY), &key) {
            set(&mut request.headers, HeaderName::from_static(name), &value);
        }
        request
    }

    /// The verdicts of every presented signature of `request`, in order.
    fn verdicts(request: &SignedRequestParts<'_>) -> Vec<Result<(), SignatureError>> {
        present_signatures(&request.headers)
            .iter()
            .map(|presented| {
                presented.verify(request, "https", FIXTURE_PUBLIC_PKCS1_PEM, now())?;
                presented.verify_body(&request.headers, BODY)
            })
            .collect()
    }

    #[test]
    fn a_request_signed_both_ways_presents_both_signatures_cavage_first() {
        let request = both();
        assert!(signatures_offered(&request.headers), "something is offered");
        let presented = present_signatures(&request.headers);
        assert!(
            matches!(
                presented.as_slice(),
                [Presented::Cavage(_), Presented::Rfc9421(_)]
            ),
            "the cavage signature, then the RFC 9421 one"
        );
        assert!(
            presented.iter().all(|one| one.key_id() == KEY_ID),
            "both name the same key"
        );
        assert_eq!(verdicts(&request), vec![Ok(()), Ok(())], "both verify");
    }

    #[test]
    fn either_scheme_alone_is_presented() {
        let mut only_9421 = both();
        let removed = only_9421.headers.remove(header::AUTHORIZATION);
        drop(removed);
        assert!(
            matches!(
                present_signatures(&only_9421.headers).as_slice(),
                [Presented::Rfc9421(_)]
            ),
            "RFC 9421 alone"
        );
        let (_, bare_cavage) = round_trip("POST", URL, "/inbox", Some(BODY), now());
        assert!(
            matches!(
                present_signatures(&bare_cavage.headers).as_slice(),
                [Presented::Cavage(_)]
            ),
            "a bare cavage Signature alone"
        );
        assert!(!signatures_offered(&HeaderMap::new()), "nothing offered");
        assert!(
            present_signatures(&HeaderMap::new()).is_empty(),
            "nothing presented"
        );
    }

    #[test]
    fn an_invalid_rfc9421_signature_beside_a_valid_cavage_one_is_still_listed_invalid() {
        let mut request = both();
        set(
            &mut request.headers,
            HeaderName::from_static("content-digest"),
            "sha-256=:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=:",
        );
        assert_eq!(
            verdicts(&request),
            vec![Ok(()), Err(SignatureError::BadSignature)],
            "the cavage one verifies, the tampered RFC 9421 one does not"
        );
    }

    #[test]
    fn an_invalid_cavage_signature_beside_a_valid_rfc9421_one_is_still_listed_invalid() {
        let mut request = both();
        let tampered = request
            .headers
            .get(header::AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap()
            .replace("host date", "host date digest");
        set(&mut request.headers, header::AUTHORIZATION, &tampered);
        let outcomes = verdicts(&request);
        assert!(
            outcomes.first().is_some_and(Result::is_err),
            "the cavage one no longer verifies"
        );
        assert_eq!(outcomes.get(1), Some(&Ok(())), "the RFC 9421 one does");
    }

    #[test]
    fn the_public_scheme_is_read_off_the_base_url() {
        assert_eq!(public_scheme("https://wired.example"), "https", "https");
        assert_eq!(public_scheme("http://127.0.0.1:4000"), "http", "http");
        assert_eq!(public_scheme("wired.example"), "https", "no scheme");
    }
}
