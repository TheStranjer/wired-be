//! HTTP Signatures: draft-cavage-http-signatures-12, the scheme
//! Mastodon and Pleroma use to authenticate every federation request,
//! outbound ([`sign_request`]) and inbound ([`verify`]).
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
//! verifier below can reuse it.
//!
//! The inbound half is [`parse_signature_header`] and [`verify`]: they
//! rebuild the signed text from a received request's headers and check the
//! RSA signature against a public key, the primitive behind the
//! authorized-fetch gate ([`crate::federation::authorized_fetch`]) and the
//! inbox verification of later tickets.

use axum::http::{HeaderMap, HeaderName};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Utc};
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::{
    error::{AppError, AppResult},
    util::keys::{sign_sha256, verify_sha256},
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

/// The `algorithm` parameter value newer implementations send for the same
/// RSA-SHA256 scheme, spelled through the `Signature` header draft.
const HS2019: &str = "hs2019";
/// The pseudo-header naming method and request target in a signature.
const REQUEST_TARGET: &str = "(request-target)";

/// The parameters of a received `Signature` header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignatureHeader<'a> {
    /// The `keyId` parameter: the signing actor's key URI, e.g.
    /// `https://social.example/users/alice#main-key`.
    pub key_id: &'a str,
    /// The `algorithm` parameter, `rsa-sha256` or `hs2019` in practice.
    pub algorithm: &'a str,
    /// The `headers` parameter: the signed header names in signing order.
    pub signed_headers: &'a str,
    /// The `signature` parameter: base64 of the raw RSA signature.
    pub signature: &'a str,
}

/// Parses a received `Signature` header value into its parameters.
///
/// The wire format is comma-separated `name="value"` pairs; values are
/// quoted, so a comma or equals sign inside one (a `keyId` URL may hold
/// either) cannot fake a parameter boundary. `keyId`, `algorithm`,
/// `headers` and `signature` are all required — this instance's own signer
/// always sends them, and a header missing one cannot be verified — and
/// unknown parameters are ignored, as the draft allows.
#[must_use]
pub fn parse_signature_header(value: &str) -> Option<SignatureHeader<'_>> {
    // Quoted values alternate with the `name=` bits between them, so a
    // well-formed header splits into an odd number of `"`-pieces; an even
    // one means a value was never closed.
    if value.split('"').count().is_multiple_of(2) {
        return None;
    }
    let mut key_id = None;
    let mut algorithm = None;
    let mut signed_headers = None;
    let mut signature = None;
    let mut pieces = value.split('"');
    while let Some(name_piece) = pieces.next() {
        // Even pieces are the `name=` bits between the quoted values, odd
        // ones the values themselves.
        let Some(param_value) = pieces.next() else {
            if name_piece.contains('=') {
                // A dangling `name=` with no quoted value: malformed.
                return None;
            }
            // Trailing separator or whitespace after the last parameter.
            break;
        };
        let name = name_piece
            .trim()
            .trim_start_matches(',')
            .trim()
            .trim_end_matches('=')
            .trim_end();
        match name {
            "keyId" | "keyid" => key_id = Some(param_value),
            "algorithm" => algorithm = Some(param_value),
            "headers" => signed_headers = Some(param_value),
            "signature" => signature = Some(param_value),
            _ => {}
        }
    }
    Some(SignatureHeader {
        key_id: key_id?,
        algorithm: algorithm?,
        signed_headers: signed_headers?,
        signature: signature?,
    })
}

/// Verifies the HTTP signature of an inbound request against `public_pem`.
///
/// `header` is the parsed `Signature` header, `method` and `target` the
/// received request line (`target` being the path with its query), and
/// `request` the received headers the named values are read from. The text
/// checked is exactly what [`signing_string`] builds for the same header
/// list; the signature itself is the RSASSA-PKCS1-v1_5 SHA-256 that
/// [`crate::util::keys::sign_sha256`] makes, which is also what the
/// `hs2019` label carries in practice.
///
/// Any failure — a foreign algorithm, a named header the request does not
/// carry, undecodable signature bytes, an unparseable key or a plain
/// mismatch — is `false`, never an error: untrusted input must not push a
/// verifier into the error path (see [`verify_sha256`]).
#[must_use]
pub fn verify(
    header: &SignatureHeader<'_>,
    method: &str,
    target: &str,
    request: &HeaderMap,
    public_pem: &str,
) -> bool {
    if !matches!(header.algorithm, ALGORITHM | HS2019) {
        return false;
    }
    let Some(text) = signed_text(header.signed_headers, method, target, request) else {
        return false;
    };
    let Ok(raw_signature) = BASE64.decode(header.signature) else {
        return false;
    };
    verify_sha256(public_pem, text.as_bytes(), &raw_signature)
}

/// Rebuilds the signed text for an arbitrary `headers` parameter: one
/// lowercased `name: value` line per named header in order, joined with
/// `\n`. `(request-target)` is spelled from the received `method` and
/// `target`; every other name must be present and readable in `request`,
/// or there is nothing to verify and the answer is `None`. An empty header
/// list signs nothing and is `None` as well.
fn signed_text(
    signed_headers: &str,
    method: &str,
    target: &str,
    request: &HeaderMap,
) -> Option<String> {
    let mut lines = Vec::new();
    for name in signed_headers.split_whitespace() {
        let name = name.to_ascii_lowercase();
        let value = if name == REQUEST_TARGET {
            format!("{} {target}", method.to_ascii_lowercase())
        } else {
            let header_name = HeaderName::from_bytes(name.as_bytes()).ok()?;
            request.get(header_name)?.to_str().ok()?.to_owned()
        };
        lines.push(format!("{name}: {value}"));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use axum::http::{HeaderName, HeaderValue};
    use chrono::{NaiveDateTime, TimeZone as _};
    use rsa::{pkcs1::EncodeRsaPrivateKey as _, pkcs8::LineEnding};

    use super::*;
    use crate::util::{
        keys::{
            FIXTURE_PUBLIC_PKCS1_PEM, KeyPair, generate_keypair, parse_private_key, verify_sha256,
        },
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

    /// A request [`HeaderMap`] carrying `pairs`, for the verifier's view of
    /// a received request.
    fn request_headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            let previous = headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
            drop(previous);
        }
        headers
    }

    /// The parameters [`sign_request`] just emitted, parsed back.
    fn parsed(signed: &SignedHeaders) -> SignatureHeader<'_> {
        parse_signature_header(&signed.signature)
            .expect("our own signer emits a parseable Signature header")
    }

    /// A signed `GET` of `https://peer.example/users/alice` and the request
    /// headers a peer would have received beside it.
    fn signed_get() -> (SignedHeaders, HeaderMap) {
        let signed = sign_request(
            "GET",
            "https://peer.example/users/alice",
            None,
            &signing_key(&keys().private_pem),
            now(),
        )
        .unwrap();
        let request = request_headers(&[
            ("host", signed.host.as_str()),
            ("date", signed.date.as_str()),
            ("signature", signed.signature.as_str()),
        ]);
        (signed, request)
    }

    #[test]
    fn a_signature_the_module_made_verifies() {
        let (signed, request) = signed_get();
        assert!(
            verify(
                &parsed(&signed),
                "GET",
                "/users/alice",
                &request,
                &keys().public_pem,
            ),
            "the inbound verifier accepts what the outbound signer produced"
        );
    }

    #[test]
    fn a_signed_post_verifies_over_the_digest_line_too() {
        let signed = sign_request(
            "POST",
            "https://peer.example/inbox",
            Some(BODY),
            &signing_key(&keys().private_pem),
            now(),
        )
        .unwrap();
        let digest = signed.digest.clone().expect("a POST signs a digest");
        let request = request_headers(&[
            ("host", &signed.host),
            ("date", &signed.date),
            ("digest", &digest),
            ("signature", &signed.signature),
        ]);
        assert!(
            verify(
                &parsed(&signed),
                "POST",
                "/inbox",
                &request,
                &keys().public_pem,
            ),
            "the four-line signing string verifies"
        );
    }

    #[test]
    fn a_changed_request_does_not_verify() {
        let (signed, request) = signed_get();
        let header = parsed(&signed);
        let public = &keys().public_pem;
        assert!(
            !verify(&header, "GET", "/users/mallory", &request, public),
            "a different path breaks the (request-target) line"
        );
        assert!(
            !verify(&header, "DELETE", "/users/alice", &request, public),
            "a different method breaks it too"
        );
        let no_date = request_headers(&[
            ("host", signed.host.as_str()),
            ("signature", signed.signature.as_str()),
        ]);
        assert!(
            !verify(&header, "GET", "/users/alice", &no_date, public),
            "a named header the request dropped cannot be rebuilt"
        );
        let tampered = request_headers(&[
            ("host", signed.host.as_str()),
            ("date", "Wed, 08 Jun 2022 20:51:35 GMT"),
            ("signature", signed.signature.as_str()),
        ]);
        assert!(
            !verify(&header, "GET", "/users/alice", &tampered, public),
            "a changed date invalidates the signature"
        );
        assert!(
            !verify(
                &header,
                "GET",
                "/users/alice",
                &request,
                FIXTURE_PUBLIC_PKCS1_PEM
            ),
            "an unrelated public key must not accept the signature"
        );
    }

    #[test]
    fn hs2019_labels_the_same_rsa_sha256_scheme() {
        let (signed, request) = signed_get();
        let mut header = parsed(&signed);
        header.algorithm = HS2019;
        assert!(
            verify(&header, "GET", "/users/alice", &request, &keys().public_pem,),
            "hs2019 over the same lines verifies the same bytes"
        );
    }

    #[test]
    fn foreign_algorithms_and_undecodable_signatures_fail_closed() {
        let (signed, request) = signed_get();
        let public = keys().public_pem.clone();
        let mut wrong_algorithm = parsed(&signed);
        wrong_algorithm.algorithm = "hmac-sha256";
        assert!(
            !verify(&wrong_algorithm, "GET", "/users/alice", &request, &public),
            "only the rsa-sha256 schemes are accepted"
        );
        let mut undecodable = parsed(&signed);
        undecodable.signature = "not base64 at all!";
        assert!(
            !verify(&undecodable, "GET", "/users/alice", &request, &public),
            "signature bytes that do not decode are rejected"
        );
        let mut empty_list = parsed(&signed);
        empty_list.signed_headers = "";
        assert!(
            !verify(&empty_list, "GET", "/users/alice", &request, &public),
            "a signature over no headers signs nothing"
        );
    }

    #[test]
    fn the_parser_reads_the_four_parameters() {
        let header = r#"keyId="https://peer.example/users/bob#main-key",algorithm="rsa-sha256",headers="(request-target) host date",signature="dGVzdA==""#;
        let parsed = parse_signature_header(header).expect("a well-formed Signature header");
        assert_eq!(
            parsed.key_id, "https://peer.example/users/bob#main-key",
            "keyId"
        );
        assert_eq!(parsed.algorithm, "rsa-sha256", "algorithm");
        assert_eq!(
            parsed.signed_headers, "(request-target) host date",
            "headers"
        );
        assert_eq!(parsed.signature, "dGVzdA==", "signature");
    }

    #[test]
    fn the_parser_survives_odd_spellings_and_ignores_unknowns() {
        // A lowercase `keyid`, a comma inside the quoted signature value,
        // an unknown parameter and surrounding whitespace all parse.
        let header = r#"keyid="https://x.example/a",algorithm="rsa-sha256", headers="(request-target) host date",comment="a,b,c",signature="AAA,BBB""#;
        let parsed = parse_signature_header(header).expect("the draft allows these spellings");
        assert_eq!(parsed.key_id, "https://x.example/a", "lowercase keyid");
        assert_eq!(parsed.signature, "AAA,BBB", "a comma inside a quoted value");
        assert_eq!(
            parsed.signed_headers, "(request-target) host date",
            "spaced separator"
        );
    }

    #[test]
    fn the_parser_rejects_malformed_headers() {
        for header in [
            "",
            "junk",
            r#"keyId="https://x.example/a",algorithm="rsa-sha256",headers="(request-target) host date""#,
            r#"signature="abc""#,
            r#"keyId="unterminated,algorithm="rsa-sha256",headers="h",signature="abc""#,
            r#"keyId="x",algorithm="rsa-sha256",headers="h",signature="#,
        ] {
            assert!(
                parse_signature_header(header).is_none(),
                "`{header}` is not a complete Signature header"
            );
        }
    }

    #[test]
    fn the_wire_round_trips_through_the_parser_and_verifier() {
        let signed = sign_request(
            "GET",
            "https://peer.example/collection?page=2",
            None,
            &signing_key(&keys().private_pem),
            now(),
        )
        .unwrap();
        let request = request_headers(&[
            ("host", signed.host.as_str()),
            ("date", signed.date.as_str()),
            ("signature", signed.signature.as_str()),
        ]);
        let header = parsed(&signed);
        assert_eq!(header.key_id, KEY_ID, "the parsed key id is the sent one");
        assert!(
            verify(
                &header,
                "GET",
                "/collection?page=2",
                &request,
                &keys().public_pem,
            ),
            "a request assembled from the wire strings verifies"
        );
    }
}
