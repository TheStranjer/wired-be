//! Outbound HTTP Signatures: draft-cavage-http-signatures-12, the scheme
//! Mastodon and Pleroma use to authenticate every federation request.
//!
//! Pleroma signs through `lib/pleroma/signature.ex` and its
//! `http_signatures` dep; Mastodon through `app/lib/request.rb`. Both
//! accept exactly what this module emits.
//!
//! A signed request carries a `Date` (RFC 7231 IMF-fixdate), a `Host`
//! (`host[:port]`, port only when the URL spells out a non-default one), a
//! `Digest` (`SHA-256=<base64>` of the body, requests with a body only) and
//! a `Signature` header holding the `keyId`, `algorithm`, `headers` and
//! base64 `signature` parameters. The signed text lists one `name: value`
//! line per signed header in order; the fediverse convention is
//! `(request-target) host date`, plus `digest` when there is a body, where
//! `(request-target)` is the lowercased method and the path *with* its
//! query (the newer drafts and Mastodon include the query; Pleroma
//! verifies with and without it).
//!
//! [`sign_request`] produces all four headers for a sender;
//! [`signing_string`] is the same text builder split out so the inbound
//! verifier ticket can reuse it.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Utc};
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::{
    error::{AppError, AppResult},
    util::keys::sign_sha256,
};

/// Digest scheme named in every `Digest` header value the fediverse accepts.
const DIGEST_SCHEME: &str = "SHA-256";
/// Value of the `algorithm` Signature parameter this module emits; Mastodon
/// also accepts `hs2019`, but `rsa-sha256` is what every implementation
/// speaks.
const ALGORITHM: &str = "rsa-sha256";
/// `chrono` format of the RFC 7231 IMF-fixdate the `date` header carries,
/// e.g. `Tue, 07 Jun 2022 20:51:35 GMT` (Pleroma's `Signature.signed_date`).
const IMF_FIXDATE: &str = "%a, %d %b %Y %H:%M:%S GMT";
/// The `headers` Signature parameter of a bodyless request, in signing
/// order.
const SIGNED_HEADERS: &str = "(request-target) host date";
/// The same, with `digest` appended for requests that carry a body.
const SIGNED_HEADERS_WITH_DIGEST: &str = "(request-target) host date digest";

/// The identity that signs an outbound request.
///
/// An actor's key URI and the private key that proves it, grouped into one
/// parameter because the lint gate caps plain functions at five arguments
/// (`docs/ARCHITECTURE.md` decision 14).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SigningKey<'a> {
    /// The `keyId` Signature parameter, e.g.
    /// `https://social.example/users/alice#main-key`.
    pub key_id: &'a str,
    /// The signing key as PEM: PKCS#8 or legacy PKCS#1 (see
    /// [`crate::util::keys::parse_private_key`]).
    pub private_pem: &'a str,
}

/// The four headers [`sign_request`] produces for one outbound request.
/// A sender adds them to the request it builds for the signed URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignedHeaders {
    /// The `Date` header: the signing moment as an RFC 7231 IMF-fixdate.
    pub date: String,
    /// The `Host` as signed: `host[:port]` of the target URL, with the port
    /// only when it is not the scheme's default.
    pub host: String,
    /// The `Digest` header, present exactly for requests with a body.
    pub digest: Option<String>,
    /// The full `Signature` header value, parameters and base64 signature
    /// included.
    pub signature: String,
}

/// Computes the `Digest` header value for `body`:
/// `SHA-256=<base64 of the SHA-256 hash>`, the only digest form Mastodon
/// and Pleroma accept.
#[must_use]
pub fn digest_header(body: &[u8]) -> String {
    format!("{DIGEST_SCHEME}={}", BASE64.encode(Sha256::digest(body)))
}

/// Formats `now` as the IMF-fixdate the `date` header carries.
#[must_use]
fn date_header(now: DateTime<Utc>) -> String {
    now.format(IMF_FIXDATE).to_string()
}

/// The `(request-target)` pseudo-header value: `method` lowercased, a
/// space, then the URL's path with its query when it has one.
#[must_use]
fn request_target(method: &str, url: &Url) -> String {
    let path_with_query = url.query().map_or_else(
        || url.path().to_owned(),
        |query| format!("{}?{query}", url.path()),
    );
    format!("{} {path_with_query}", method.to_lowercase())
}

/// The `host` header value to sign: the URL's host, plus `:port` only when
/// the URL spells out a non-default port (`Url::port` reports `None` for
/// an absent or default one). Only called with URLs [`sign_request`] has
/// accepted, which always carry a host.
#[must_use]
fn host_of(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    url.port()
        .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"))
}

/// Assembles the exact text that gets signed from the signed header values.
///
/// One `name: value` line per header in `(request-target) host date
/// [digest]` order, joined with `\n` and without a trailing newline.
/// Split out of [`sign_request`] so the inbound verifier can rebuild the
/// same text from a received request.
#[must_use]
pub fn signing_string(
    request_target: &str,
    host: &str,
    date: &str,
    digest: Option<&str>,
) -> String {
    let mut lines = vec![
        format!("(request-target): {request_target}"),
        format!("host: {host}"),
        format!("date: {date}"),
    ];
    if let Some(digest_value) = digest {
        lines.push(format!("digest: {digest_value}"));
    }
    lines.join("\n")
}

/// Signs an outbound federation request with `key`'s private key.
///
/// The signed headers are `(request-target) host date`, plus `digest` when
/// `body` is `Some`, built per [`signing_string`] from the lowercased
/// `method` and `url`, the IMF-fixdate of `now` and [`digest_header`] of
/// the body. The RSA signature (RSASSA-PKCS1-v1_5 over SHA-256) is base64'd
/// into a `Signature` header value of the shape
/// `keyId="...",algorithm="rsa-sha256",headers="...",signature="..."`.
///
/// # Errors
///
/// [`AppError::BadRequest`] if `url` is unparsable or not an http(s) URL
/// with a host, or if `key.private_pem` does not parse as an RSA private
/// key; [`AppError::Internal`] if the signing operation itself fails.
pub fn sign_request(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    key: &SigningKey<'_>,
    now: DateTime<Utc>,
) -> AppResult<SignedHeaders> {
    let parsed = Url::parse(url)
        .map_err(|err| AppError::BadRequest(format!("cannot sign `{url}`: {err}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(AppError::BadRequest(format!(
            "cannot sign `{url}`: only http and https URLs with a host can be signed"
        )));
    }
    let target = request_target(method, &parsed);
    let host = host_of(&parsed);
    let date = date_header(now);
    let digest = body.map(digest_header);
    let to_sign = signing_string(&target, &host, &date, digest.as_deref());
    let raw_signature = sign_sha256(key.private_pem, to_sign.as_bytes())?;
    let signature = format!(
        r#"keyId="{}",algorithm="{ALGORITHM}",headers="{}",signature="{}""#,
        key.key_id,
        if digest.is_some() {
            SIGNED_HEADERS_WITH_DIGEST
        } else {
            SIGNED_HEADERS
        },
        BASE64.encode(raw_signature),
    );
    Ok(SignedHeaders {
        date,
        host,
        digest,
        signature,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use chrono::{NaiveDateTime, TimeZone as _};
    use rsa::{pkcs1::EncodeRsaPrivateKey as _, pkcs8::LineEnding};

    use super::*;
    use crate::util::{
        keys::{KeyPair, generate_keypair, parse_private_key, verify_sha256},
        time::parse_time,
    };

    /// The key id [`sign_request`] is called with in these tests.
    const KEY_ID: &str = "https://wired.example/actor#main-key";
    /// The fixed signing moment (`2022-06-07T20:51:35Z`) in the RFC 3339
    /// spelling [`parse_time`] reads.
    const NOW_RFC3339: &str = "2022-06-07T20:51:35Z";
    /// The same moment as the expected `Date` header (IMF-fixdate).
    const NOW_FIXDATE: &str = "Tue, 07 Jun 2022 20:51:35 GMT";
    /// A body with a well-known SHA-256 digest, for the `Digest` checks.
    const BODY: &[u8] = b"hello";
    /// `SHA-256=<base64 of sha256("hello")>`, computed independently.
    const HELLO_DIGEST: &str = "SHA-256=LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=";

    /// RSA-2048 generation is slow in debug builds, so every test that
    /// needs a modern PKCS#8 key shares this one pair.
    static KEYS: OnceLock<KeyPair> = OnceLock::new();

    /// The shared key pair, generated on first use.
    fn keys() -> &'static KeyPair {
        KEYS.get_or_init(|| generate_keypair().unwrap())
    }

    /// The [`KEY_ID`]/`private_pem` pair these tests sign with.
    fn signing_key(private_pem: &str) -> SigningKey<'_> {
        SigningKey {
            key_id: KEY_ID,
            private_pem,
        }
    }

    /// The fixed [`NOW_RFC3339`] as the timestamp [`sign_request`] takes.
    fn now() -> DateTime<Utc> {
        parse_time(NOW_RFC3339).unwrap()
    }

    /// The raw signature bytes out of a well-formed `Signature` header,
    /// also checking the `keyId`, `algorithm` and `headers` parameters
    /// equal [`KEY_ID`], `rsa-sha256` and `headers_param`.
    fn signature_of(header: &str, headers_param: &str) -> Vec<u8> {
        let expected_prefix = format!(r#"keyId="{KEY_ID}",algorithm="rsa-sha256",headers=""#);
        let after_key = header
            .strip_prefix(expected_prefix.as_str())
            .unwrap_or_else(|| panic!("signature header starts with keyId/algorithm: {header}"));
        let (named, encoded) = after_key.split_once("\",signature=\"").unwrap_or_else(|| {
            panic!("signature header has the headers and signature parameters: {header}")
        });
        let encoded = encoded
            .strip_suffix('"')
            .unwrap_or_else(|| panic!("signature header ends with the quoted signature: {header}"));
        assert_eq!(named, headers_param, "the headers parameter");
        BASE64.decode(encoded).unwrap()
    }

    #[test]
    fn digest_header_is_sha256_base64_of_the_body() {
        assert_eq!(
            digest_header(BODY),
            HELLO_DIGEST,
            "digest of a known string"
        );
        assert_eq!(
            digest_header(b""),
            "SHA-256=47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
            "the empty body has the well-known all-Zeros digest"
        );
    }

    #[test]
    fn signing_string_lists_the_signed_headers_in_order_for_a_get() {
        assert_eq!(
            signing_string("get /users/alice", "wired.example", NOW_FIXDATE, None),
            "(request-target): get /users/alice\n\
             host: wired.example\n\
             date: Tue, 07 Jun 2022 20:51:35 GMT",
            "three lines, no digest, no trailing newline"
        );
    }

    #[test]
    fn signing_string_appends_the_digest_line_for_a_post() {
        assert_eq!(
            signing_string(
                "post /inbox",
                "peer.example:4436",
                NOW_FIXDATE,
                Some(HELLO_DIGEST),
            ),
            "(request-target): post /inbox\n\
             host: peer.example:4436\n\
             date: Tue, 07 Jun 2022 20:51:35 GMT\n\
             digest: SHA-256=LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=",
            "the digest is the fourth line"
        );
    }

    #[test]
    fn get_is_signed_over_request_target_host_date() {
        let signed = sign_request(
            "GET",
            "https://peer.example/users/alice",
            None,
            &signing_key(&keys().private_pem),
            now(),
        )
        .unwrap();
        assert_eq!(signed.date, NOW_FIXDATE, "date is an IMF-fixdate");
        assert_eq!(signed.host, "peer.example", "host without a default port");
        assert_eq!(signed.digest, None, "a GET signs no digest");
        let signature = signature_of(&signed.signature, SIGNED_HEADERS);
        assert!(
            verify_sha256(
                &keys().public_pem,
                signing_string("get /users/alice", "peer.example", NOW_FIXDATE, None).as_bytes(),
                &signature,
            ),
            "the signature verifies over the GET signing string"
        );
    }

    #[test]
    fn post_signs_the_digest_too() {
        let signed = sign_request(
            "POST",
            "https://peer.example/users/bob/inbox",
            Some(BODY),
            &signing_key(&keys().private_pem),
            now(),
        )
        .unwrap();
        assert_eq!(
            signed.digest.as_deref(),
            Some(HELLO_DIGEST),
            "digest header"
        );
        let signature = signature_of(&signed.signature, SIGNED_HEADERS_WITH_DIGEST);
        assert!(
            verify_sha256(
                &keys().public_pem,
                signing_string(
                    "post /users/bob/inbox",
                    "peer.example",
                    NOW_FIXDATE,
                    Some(HELLO_DIGEST),
                )
                .as_bytes(),
                &signature,
            ),
            "the signature verifies over the four-line POST signing string"
        );
        let tampered = signing_string(
            "post /users/bob/inbox",
            "peer.example",
            NOW_FIXDATE,
            Some("SHA-256=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="),
        );
        assert!(
            !verify_sha256(&keys().public_pem, tampered.as_bytes(), &signature),
            "a different digest invalidates the signature"
        );
    }

    #[test]
    fn a_non_default_port_is_signed_into_the_host() {
        let signed = sign_request(
            "GET",
            "http://peer.example:4436/users/alice",
            None,
            &signing_key(&keys().private_pem),
            now(),
        )
        .unwrap();
        assert_eq!(signed.host, "peer.example:4436", "explicit port is kept");
        let signature = signature_of(&signed.signature, SIGNED_HEADERS);
        assert!(
            verify_sha256(
                &keys().public_pem,
                signing_string("get /users/alice", "peer.example:4436", NOW_FIXDATE, None,)
                    .as_bytes(),
                &signature,
            ),
            "the port is part of the signed host"
        );
        let spelled_default = sign_request(
            "GET",
            "https://peer.example:443/users/alice",
            None,
            &signing_key(&keys().private_pem),
            now(),
        )
        .unwrap();
        assert_eq!(
            spelled_default.host, "peer.example",
            "a port spelled out but equal to the scheme default is dropped"
        );
    }

    #[test]
    fn the_query_string_is_part_of_the_request_target() {
        let signed = sign_request(
            "GET",
            "https://peer.example/users/alice?page=2&x=1#frag",
            None,
            &signing_key(&keys().private_pem),
            now(),
        )
        .unwrap();
        let signature = signature_of(&signed.signature, SIGNED_HEADERS);
        assert!(
            verify_sha256(
                &keys().public_pem,
                signing_string(
                    "get /users/alice?page=2&x=1",
                    "peer.example",
                    NOW_FIXDATE,
                    None,
                )
                .as_bytes(),
                &signature,
            ),
            "the signature covers the request target with its query"
        );
        assert!(
            !verify_sha256(
                &keys().public_pem,
                signing_string("get /users/alice", "peer.example", NOW_FIXDATE, None).as_bytes(),
                &signature,
            ),
            "and the path-only spelling does not verify"
        );
    }

    #[test]
    fn the_method_is_lowercased_for_the_request_target() {
        let signed = sign_request(
            "POST",
            "https://peer.example/inbox",
            Some(BODY),
            &signing_key(&keys().private_pem),
            now(),
        )
        .unwrap();
        let signature = signature_of(&signed.signature, SIGNED_HEADERS_WITH_DIGEST);
        assert!(
            verify_sha256(
                &keys().public_pem,
                signing_string(
                    "post /inbox",
                    "peer.example",
                    NOW_FIXDATE,
                    Some(HELLO_DIGEST)
                )
                .as_bytes(),
                &signature,
            ),
            "an uppercase method signs as its lowercase spelling"
        );
    }

    #[test]
    fn date_header_formats_the_utc_moment_as_imf_fixdate() {
        let naive =
            NaiveDateTime::parse_from_str("2022-06-07 20:51:35", "%Y-%m-%d %H:%M:%S").unwrap();
        let moment = Utc.from_utc_datetime(&naive);
        assert_eq!(date_header(moment), NOW_FIXDATE, "RFC 7231 IMF-fixdate");
    }

    #[test]
    fn the_host_helper_builds_host_and_optional_port() {
        let plain = Url::parse("http://a.example/x").unwrap();
        assert_eq!(host_of(&plain), "a.example");
        let ported = Url::parse("http://a.example:8080/x").unwrap();
        assert_eq!(host_of(&ported), "a.example:8080");
        let default = Url::parse("http://a.example:80/x").unwrap();
        assert_eq!(
            host_of(&default),
            "a.example",
            "the http default port is never signed"
        );
    }

    #[test]
    fn the_request_target_helper_builds_path_with_query() {
        let plain = Url::parse("https://a.example/users/alice").unwrap();
        assert_eq!(request_target("GET", &plain), "get /users/alice");
        let queried = Url::parse("https://a.example/collection?type=Note").unwrap();
        assert_eq!(
            request_target("GET", &queried),
            "get /collection?type=Note",
            "the query rides along"
        );
        let root = Url::parse("https://a.example").unwrap();
        assert_eq!(request_target("get", &root), "get /", "an empty path is /");
    }

    #[test]
    fn unsignable_urls_are_rejected() {
        for url in ["ftp://example.com/pub", "not a url", "https://"] {
            let outcome = sign_request("GET", url, None, &signing_key(&keys().private_pem), now());
            assert!(
                matches!(outcome, Err(AppError::BadRequest(_))),
                "`{url}` must not be signed, got a different outcome"
            );
        }
    }

    #[test]
    fn a_private_key_that_does_not_parse_is_rejected() {
        let outcome = sign_request(
            "GET",
            "https://peer.example/users/alice",
            None,
            &signing_key("-----BEGIN PRIVATE KEY-----\nZZZZ\n-----END PRIVATE KEY-----"),
            now(),
        );
        assert!(
            matches!(outcome, Err(AppError::BadRequest(_))),
            "junk key material is a bad request, not a panic"
        );
    }

    #[test]
    fn a_legacy_pkcs1_spelling_of_the_key_signs_too() {
        let private = parse_private_key(&keys().private_pem).unwrap();
        let pkcs1_pem = private.to_pkcs1_pem(LineEnding::LF).unwrap();
        let signed = sign_request(
            "GET",
            "https://peer.example/x",
            None,
            &signing_key(pkcs1_pem.as_str()),
            now(),
        )
        .unwrap();
        let signature = signature_of(&signed.signature, SIGNED_HEADERS);
        assert!(
            verify_sha256(
                &keys().public_pem,
                signing_string("get /x", "peer.example", NOW_FIXDATE, None).as_bytes(),
                &signature,
            ),
            "a PKCS#1 PEM is accepted and signs verifiably"
        );
        assert_eq!(
            signature.len(),
            256,
            "an RSA-2048 signature is as long as the modulus"
        );
    }
}
