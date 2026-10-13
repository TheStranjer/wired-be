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

use axum::http::{HeaderMap, HeaderName};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, NaiveDateTime, TimeDelta, Utc};
use sha2::{Digest as _, Sha256};

use super::{ALGORITHM, DIGEST_SCHEME, IMF_FIXDATE, signing_string};
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
/// Mastodon's clock-skew tolerance for the signed moment: ±12 hours.
const DATE_TOLERANCE: TimeDelta = TimeDelta::hours(12);

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

/// The parameters of one received `Signature` (or `Authorization:
/// Signature`) header value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedSignature {
    /// The `keyId`/`key_id` parameter: which key claims to have signed.
    pub key_id: String,
    /// The `algorithm` parameter as sent, lowercased; `None` when absent,
    /// which [`verify`] reads as the draft default `rsa-sha256`.
    pub algorithm: Option<String>,
    /// The `headers` parameter split on whitespace, lowercased, in the
    /// signing order the sender used.
    pub headers: Vec<String>,
    /// The decoded `signature` bytes.
    pub signature: Vec<u8>,
    /// The `created` parameter as unix seconds, when the sender sent one.
    pub created: Option<i64>,
    /// The `expires` parameter as unix seconds, when the sender sent one.
    pub expires: Option<i64>,
}

/// Reads one `Signature` header value into its parameters — and the same
/// parameters carried in an `Authorization: Signature ...` header.
///
/// Parameters are accepted in any order and separated by optional spaces
/// around the commas; values are quoted (`keyId="..."`) or bare
/// (`created=1717`), and both the `keyId` and `key_id` spellings are read.
/// Unknown parameters are ignored. `keyId`, `headers` and `signature` are
/// required.
///
/// # Errors
///
/// [`SignatureError::Missing`] if a required parameter is absent,
/// [`SignatureError::Malformed`] if any parameter is unparsable.
pub fn parse_signature_header(value: &str) -> Result<ParsedSignature, SignatureError> {
    let params = scan_parameters(strip_scheme(value))?;
    let headers = parameter(&params, "headers").ok_or(SignatureError::Missing)?;
    let signature = parameter(&params, "signature").ok_or(SignatureError::Missing)?;
    Ok(ParsedSignature {
        key_id: parameter(&params, "keyid")
            .ok_or(SignatureError::Missing)?
            .to_owned(),
        algorithm: parameter(&params, "algorithm").map(str::to_lowercase),
        headers: headers.split_whitespace().map(str::to_lowercase).collect(),
        signature: BASE64.decode(signature).map_err(|_err| MALFORMED_BASE64)?,
        created: unix_seconds(parameter(&params, "created"))?,
        expires: unix_seconds(parameter(&params, "expires"))?,
    })
}

/// Drops a `Signature ` scheme prefix (case-insensitive), which the
/// `Authorization` form carries before the parameters.
fn strip_scheme(value: &str) -> &str {
    let trimmed = value.trim_start();
    match trimmed.split_once([' ', '\t']) {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case(SCHEME) => rest,
        Some(_) | None => trimmed,
    }
}

/// One `Signature` parameter as a borrowed `(name, value)` pair.
type Param<'a> = (&'a str, &'a str);

/// Splits a parameter list into `(name, value)` pairs, unquoting quoted
/// values and trimming bare ones; a comma inside a quoted value (legal in
/// a `keyId` URL) never splits.
fn scan_parameters(value: &str) -> Result<Vec<Param<'_>>, SignatureError> {
    let mut params = Vec::new();
    let mut rest = value;
    loop {
        rest = rest.trim_start_matches([' ', '\t', ',']);
        if rest.is_empty() {
            break;
        }
        let (name, after) = rest.split_once('=').ok_or(MALFORMED_PAIR)?;
        let name = name.trim();
        if !is_token(name) {
            return Err(MALFORMED_NAME);
        }
        let (raw, tail) = scan_value(after)?;
        params.push((name, raw.trim()));
        rest = tail;
    }
    Ok(params)
}

/// The value part of one parameter plus the header text following it:
/// through the closing quote when quoted, up to the next comma when bare.
fn scan_value(after: &str) -> Result<Param<'_>, SignatureError> {
    if let Some(quoted) = after.strip_prefix('"') {
        return quoted.split_once('"').ok_or(MALFORMED_QUOTE);
    }
    Ok(after.split_once(',').unwrap_or((after, "")))
}

/// Whether `name` is a legal parameter name: a nonempty token.
fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// A parameter name for lookup: lowercased, and `key_id`'s underscore
/// folded away so it reads the same as `keyId`.
fn normalize_name(name: &str) -> String {
    name.to_ascii_lowercase().replace('_', "")
}

/// The value of the parameter whose normalized name is `name`; a
/// duplicated parameter keeps its first spelling.
fn parameter<'a>(params: &[Param<'a>], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(key, _)| normalize_name(key) == name)
        .map(|(_, value)| *value)
}

/// An optional `created`/`expires` parameter as unix seconds.
fn unix_seconds(value: Option<&str>) -> Result<Option<i64>, SignatureError> {
    value
        .map(|text| text.parse::<i64>().map_err(|_err| MALFORMED_TIMESTAMP))
        .transpose()
}

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

/// The algorithms this module verifies: the outbound [`ALGORITHM`] and
/// `hs2019`, which negotiates the algorithm from the key — for every RSA
/// fediverse key that is the same RSA-SHA256. An absent parameter is the
/// draft default.
fn check_algorithm(algorithm: Option<&str>) -> Result<(), SignatureError> {
    if algorithm.is_none_or(|name| {
        name.eq_ignore_ascii_case(ALGORITHM) || name.eq_ignore_ascii_case(HS2019)
    }) {
        return Ok(());
    }
    Err(SignatureError::UnsupportedAlgorithm)
}

/// `(request-target)`, `host` and one of `date`/`(created)` must be
/// signed, and a `POST` the `digest` too.
fn check_signed_headers(
    parsed: &ParsedSignature,
    request: &SignedRequestParts<'_>,
) -> Result<(), SignatureError> {
    let signs = |name: &str| parsed.headers.iter().any(|signed| signed == name);
    let digest_needed = request.method.eq_ignore_ascii_case(POST_METHOD) && !signs(DIGEST_HEADER);
    if signs(REQUEST_TARGET)
        && signs(HOST_HEADER)
        && (signs(DATE_HEADER) || signs(CREATED))
        && !digest_needed
    {
        return Ok(());
    }
    Err(SignatureError::Missing)
}

/// A past `(expires)` parameter is [`SignatureError::Expired`], and the
/// signed moment — the `Date` header when it is signed, else the `created`
/// parameter — must sit within [`DATE_TOLERANCE`] of `now`.
fn check_freshness(
    parsed: &ParsedSignature,
    request: &SignedRequestParts<'_>,
    now: DateTime<Utc>,
) -> Result<(), SignatureError> {
    if parsed
        .expires
        .is_some_and(|expires| expires <= now.timestamp())
    {
        return Err(SignatureError::Expired);
    }
    let signed_at = signed_moment(parsed, request)?;
    if (signed_at - now).abs() > DATE_TOLERANCE {
        return Err(SignatureError::Expired);
    }
    Ok(())
}

/// The instant the signature claims freshness with: the `Date` header when
/// it is among the signed headers, else the `(created)` parameter. One of
/// the two is in the list [`check_signed_headers`] accepted.
fn signed_moment(
    parsed: &ParsedSignature,
    request: &SignedRequestParts<'_>,
) -> Result<DateTime<Utc>, SignatureError> {
    if !parsed.headers.iter().any(|signed| signed == DATE_HEADER) {
        return created_moment(parsed);
    }
    let text = header_value(&request.headers, DATE_HEADER).ok_or(SignatureError::Missing)?;
    parse_date_text(&text).ok_or(MALFORMED_DATE)
}

/// The `(created)` parameter as a timestamp; only reached where the signed
/// header list promised it, so its absence is malformed.
fn created_moment(parsed: &ParsedSignature) -> Result<DateTime<Utc>, SignatureError> {
    let created = parsed.created.ok_or(MALFORMED_TIMESTAMP)?;
    DateTime::from_timestamp(created, 0).ok_or(MALFORMED_TIMESTAMP)
}

/// The IMF-fixdate ([`super`]'s outbound `Date` spelling) as a timestamp.
fn parse_date_text(text: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(text, IMF_FIXDATE)
        .ok()
        .map(|naive| naive.and_utc())
}

/// The request targets to try, in order: the target as received, then the
/// path without its query (Pleroma signs and verifies some requests
/// query-less).
fn target_candidates(path_and_query: &str) -> Vec<&str> {
    match path_and_query.split_once('?') {
        Some((path, _)) => vec![path_and_query, path],
        None => vec![path_and_query],
    }
}

/// The signing text of `request` for the request target `target`: one
/// line per signed header, in the sender's order.
fn signing_text(
    parsed: &ParsedSignature,
    request: &SignedRequestParts<'_>,
    target: &str,
) -> Result<String, SignatureError> {
    let mut lines = Vec::with_capacity(parsed.headers.len());
    for name in &parsed.headers {
        let line = signing_line(parsed, request, name, target).ok_or(SignatureError::Missing)?;
        lines.push(line);
    }
    let borrowed = lines
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<Vec<(&str, &str)>>();
    Ok(signing_string(&borrowed))
}

/// The `(name, value)` line one signed header contributes, `None` when
/// neither the request nor the signature parameters carry a value for it.
fn signing_line(
    parsed: &ParsedSignature,
    request: &SignedRequestParts<'_>,
    name: &str,
    target: &str,
) -> Option<(String, String)> {
    let value = match name {
        REQUEST_TARGET => Some(format!("{} {target}", request.method.to_lowercase())),
        CREATED => parsed.created.map(|seconds| seconds.to_string()),
        EXPIRES => parsed.expires.map(|seconds| seconds.to_string()),
        other => header_value(&request.headers, other),
    }?;
    Some((name.to_owned(), value))
}

/// Every value of header `name` as one string — multiple ones joined with
/// `", "`, the way Mastodon's verifier reads them. `None` when the name is
/// not a header name, the request carries no such header, or a value is
/// not visible ASCII.
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let Ok(name) = HeaderName::try_from(name) else {
        return None;
    };
    let values = headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<&str>>();
    (!values.is_empty()).then(|| values.join(", "))
}

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
    use axum::http::{HeaderMap, HeaderValue, header::DATE};
    use chrono::TimeDelta;

    use super::*;
    use crate::{
        federation::signature::{SignedHeaders, SigningKey, sign_request},
        util::{
            keys::{FIXTURE_PRIVATE_PKCS1_PEM, FIXTURE_PUBLIC_PKCS1_PEM, sign_sha256},
            time::parse_time,
        },
    };

    /// The key id every signature in these tests presents.
    const KEY_ID: &str = "https://wired.example/actor#main-key";
    /// The fixed signing moment (`2022-06-07T20:51:35Z`).
    const NOW_RFC3339: &str = "2022-06-07T20:51:35Z";
    /// The same moment as unix seconds.
    const NOW_SECONDS: i64 = 1_654_635_095;
    /// The same moment as the `Date` header spelling.
    const NOW_FIXDATE: &str = "Tue, 07 Jun 2022 20:51:35 GMT";
    /// The host every round-tripped URL points at.
    const HOST: &str = "peer.example";
    /// A body with a well-known SHA-256 digest.
    const BODY: &[u8] = b"hello";
    /// `SHA-256=<base64 of sha256("hello")>`, as [`sign_request`] sends it.
    const HELLO_DIGEST: &str = "SHA-256=LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=";
    /// The base64 inside [`HELLO_DIGEST`].
    const HELLO_B64: &str = "LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=";
    /// Base64 of the bytes `hi`, for parameter-only parse tests.
    const HI_B64: &str = "aGk=";

    /// The fixed [`NOW_RFC3339`] as the verification clock.
    fn now() -> DateTime<Utc> {
        parse_time(NOW_RFC3339).unwrap()
    }

    /// The [`KEY_ID`] identity backed by the fast fixture private key.
    fn fixture_key() -> SigningKey<'static> {
        SigningKey {
            key_id: KEY_ID,
            private_pem: FIXTURE_PRIVATE_PKCS1_PEM,
        }
    }

    /// The headers a received signed request carries: the `Signature`
    /// text, the optional IMF-fixdate `Date`, the fixed [`HOST`] and the
    /// optional `Digest`.
    fn received_headers(signature: &str, date: Option<&str>, digest: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(value) = date {
            let _date: Option<HeaderValue> =
                headers.insert(DATE, HeaderValue::from_str(value).unwrap());
        }
        let _host: Option<HeaderValue> =
            headers.insert("host", HeaderValue::from_str(HOST).unwrap());
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
    fn round_trip<'a>(
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
    fn parts_for<'a>(
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
    fn crafted_signature(headers_param: &str, lines: &[(&str, &str)], params: &str) -> String {
        let raw = sign_sha256(FIXTURE_PRIVATE_PKCS1_PEM, signing_string(lines).as_bytes()).unwrap();
        format!(
            r#"keyId="{KEY_ID}"{params},headers="{headers_param}",signature="{}""#,
            BASE64.encode(raw),
        )
    }

    /// The canonical three-line GET signing entries at [`NOW_FIXDATE`].
    fn get_lines(target: &'static str) -> Vec<(&'static str, &'static str)> {
        vec![
            (REQUEST_TARGET, target),
            (HOST_HEADER, HOST),
            (DATE_HEADER, NOW_FIXDATE),
        ]
    }

    /// Verifies `parsed` against `request` with the fixture public key at
    /// [`now`].
    fn verify_now(
        parsed: &ParsedSignature,
        request: &SignedRequestParts<'_>,
    ) -> Result<(), SignatureError> {
        verify(parsed, request, FIXTURE_PUBLIC_PKCS1_PEM, now())
    }

    /// A `HeaderMap` carrying only the given `Digest` value.
    fn digest_headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let _digest: Option<HeaderValue> =
            headers.insert("digest", HeaderValue::from_str(value).unwrap());
        headers
    }

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
    fn a_signed_get_with_a_query_verifies_on_the_first_candidate() {
        let (parsed, request) = round_trip(
            "GET",
            "https://peer.example/users/alice?page=2&x=1",
            "/users/alice?page=2&x=1",
            None,
            now(),
        );
        assert_eq!(
            verify_now(&parsed, &request),
            Ok(()),
            "the target is signed with its query"
        );
    }

    #[test]
    fn a_signature_made_without_the_query_verifies_by_the_fallback() {
        let lines = get_lines("get /inbox");
        let header = crafted_signature("(request-target) host date", &lines, "");
        let parsed = parse_signature_header(&header).unwrap();
        let request = SignedRequestParts {
            method: "GET",
            path_and_query: "/inbox?page=2",
            headers: received_headers(&header, Some(NOW_FIXDATE), None),
        };
        assert_eq!(
            verify_now(&parsed, &request),
            Ok(()),
            "the query-less spelling is the second candidate"
        );
    }

    #[test]
    fn a_path_matching_neither_candidate_fails_to_verify() {
        let lines = get_lines("get /other");
        let header = crafted_signature("(request-target) host date", &lines, "");
        let parsed = parse_signature_header(&header).unwrap();
        let request = SignedRequestParts {
            method: "GET",
            path_and_query: "/inbox?page=2",
            headers: received_headers(&header, Some(NOW_FIXDATE), None),
        };
        assert_eq!(
            verify_now(&parsed, &request),
            Err(SignatureError::BadSignature),
            "with or without the query the target differs"
        );
    }

    #[test]
    fn signed_headers_can_be_listed_in_any_order() {
        let lines = [
            ("digest", HELLO_DIGEST),
            (REQUEST_TARGET, "post /inbox"),
            (DATE_HEADER, NOW_FIXDATE),
            (HOST_HEADER, HOST),
        ];
        let header = crafted_signature("digest (request-target) date host", &lines, "");
        let parsed = parse_signature_header(&header).unwrap();
        let request = SignedRequestParts {
            method: "POST",
            path_and_query: "/inbox",
            headers: received_headers(&header, Some(NOW_FIXDATE), Some(HELLO_DIGEST)),
        };
        assert_eq!(
            verify_now(&parsed, &request),
            Ok(()),
            "the text is rebuilt in the sender's order, not the canonical one"
        );
    }

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
    fn a_tampered_header_fails_to_verify() {
        let (parsed, mut request) = round_trip(
            "GET",
            "https://peer.example/users/alice",
            "/users/alice",
            None,
            now(),
        );
        let _host: Option<HeaderValue> = request
            .headers
            .insert("host", HeaderValue::from_static("evil.example"));
        assert_eq!(
            verify_now(&parsed, &request),
            Err(SignatureError::BadSignature),
            "the host is signed"
        );
    }

    #[test]
    fn a_tampered_method_or_path_fails_to_verify() {
        let (parsed, request) = round_trip(
            "GET",
            "https://peer.example/users/alice",
            "/users/alice",
            None,
            now(),
        );
        let other_method = SignedRequestParts {
            method: "DELETE",
            ..request.clone()
        };
        assert_eq!(
            verify_now(&parsed, &other_method),
            Err(SignatureError::BadSignature),
            "the method rides in (request-target)"
        );
        let other_path = SignedRequestParts {
            path_and_query: "/users/bob",
            ..request
        };
        assert_eq!(
            verify_now(&parsed, &other_path),
            Err(SignatureError::BadSignature),
            "and so does the path"
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

    #[test]
    fn date_skew_beyond_twelve_hours_is_expired() {
        for skew in [13_i64, -13] {
            let then = now() + TimeDelta::hours(skew);
            let (parsed, request) = round_trip(
                "GET",
                "https://peer.example/users/alice",
                "/users/alice",
                None,
                then,
            );
            assert_eq!(
                verify_now(&parsed, &request),
                Err(SignatureError::Expired),
                "{skew}h of skew is past the window"
            );
        }
    }

    #[test]
    fn date_skew_inside_twelve_hours_passes() {
        for skew in [11_i64, -11, 0] {
            let then = now() + TimeDelta::hours(skew);
            let (parsed, request) = round_trip(
                "GET",
                "https://peer.example/users/alice",
                "/users/alice",
                None,
                then,
            );
            assert_eq!(
                verify_now(&parsed, &request),
                Ok(()),
                "{skew}h of skew is inside the window"
            );
        }
    }

    #[test]
    fn the_mandatory_signed_headers_are_enforced() {
        let no_target = [(HOST_HEADER, HOST), (DATE_HEADER, NOW_FIXDATE)];
        let no_host = [(REQUEST_TARGET, "get /x"), (DATE_HEADER, NOW_FIXDATE)];
        let no_moment = [(REQUEST_TARGET, "get /x"), (HOST_HEADER, HOST)];
        let plain_get = get_lines("get /x");
        for (headers_param, lines, method) in [
            ("host date", no_target.as_slice(), "GET"),
            ("(request-target) date", no_host.as_slice(), "GET"),
            ("(request-target) host", no_moment.as_slice(), "GET"),
            ("(request-target) host date", plain_get.as_slice(), "POST"),
        ] {
            let header = crafted_signature(headers_param, lines, "");
            let parsed = parse_signature_header(&header).unwrap();
            let request = SignedRequestParts {
                method,
                path_and_query: "/x",
                headers: received_headers(&header, Some(NOW_FIXDATE), None),
            };
            assert_eq!(
                verify_now(&parsed, &request),
                Err(SignatureError::Missing),
                "`{headers_param}` signed by a {method} lacks a mandatory header"
            );
        }
    }

    #[test]
    fn a_signed_date_header_that_is_absent_or_unparsable_fails() {
        let lines = get_lines("get /x");
        let header = crafted_signature("(request-target) host date", &lines, "");
        let parsed = parse_signature_header(&header).unwrap();
        let no_date = SignedRequestParts {
            method: "GET",
            path_and_query: "/x",
            headers: received_headers(&header, None, None),
        };
        assert_eq!(
            verify_now(&parsed, &no_date),
            Err(SignatureError::Missing),
            "a signed Date header must be in the request"
        );
        let mut junk = received_headers(&header, Some(NOW_FIXDATE), None);
        let _junk: Option<HeaderValue> = junk.insert(DATE, HeaderValue::from_static("yesterday"));
        let bad_date = SignedRequestParts {
            method: "GET",
            path_and_query: "/x",
            headers: junk,
        };
        assert_eq!(
            verify_now(&parsed, &bad_date),
            Err(MALFORMED_DATE),
            "and must parse as a timestamp"
        );
    }

    #[test]
    fn a_created_pseudo_header_stands_in_for_the_date() {
        let created_text = NOW_SECONDS.to_string();
        let lines = [
            (REQUEST_TARGET, "get /x"),
            (HOST_HEADER, HOST),
            (CREATED, created_text.as_str()),
        ];
        let header = crafted_signature(
            "(request-target) host (created)",
            &lines,
            &format!(",created={NOW_SECONDS}"),
        );
        let parsed = parse_signature_header(&header).unwrap();
        assert_eq!(parsed.created, Some(NOW_SECONDS), "the parameter parses");
        let request = SignedRequestParts {
            method: "GET",
            path_and_query: "/x",
            headers: received_headers(&header, None, None),
        };
        assert_eq!(
            verify_now(&parsed, &request),
            Ok(()),
            "(created) carries both freshness and a signed line"
        );
        let stale = ParsedSignature {
            created: Some((now() - TimeDelta::hours(13)).timestamp()),
            ..parsed
        };
        assert_eq!(
            verify_now(&stale, &request),
            Err(SignatureError::Expired),
            "a stale (created) is outside the window"
        );
    }

    #[test]
    fn a_past_expires_parameter_is_expired_a_future_one_passes() {
        let lines = get_lines("get /x");
        for (offset, expected) in [(-3600_i64, Err(SignatureError::Expired)), (3600, Ok(()))] {
            let header = crafted_signature(
                "(request-target) host date",
                &lines,
                &format!(
                    ",created={},expires={}",
                    NOW_SECONDS - 60,
                    NOW_SECONDS + offset
                ),
            );
            let parsed = parse_signature_header(&header).unwrap();
            let request = SignedRequestParts {
                method: "GET",
                path_and_query: "/x",
                headers: received_headers(&header, Some(NOW_FIXDATE), None),
            };
            assert_eq!(
                verify_now(&parsed, &request),
                expected,
                "expires at now{offset:+} seconds"
            );
        }
    }

    #[test]
    fn parses_quoted_parameters_in_any_order_with_optional_spaces() {
        let value = format!(
            r#"Signature created="{NOW_SECONDS}" , vendor="ignored, with a comma" ,, expires = {} , headers="(request-target) host date" , algorithm="hs2019",key_id="{KEY_ID}",signature="{HI_B64}""#,
            NOW_SECONDS + 3600,
        );
        let parsed = parse_signature_header(&value).unwrap();
        assert_eq!(
            parsed.key_id, KEY_ID,
            "the `key_id` spelling reads as `keyId`"
        );
        assert_eq!(
            parsed.algorithm.as_deref(),
            Some("hs2019"),
            "a padded algorithm parameter"
        );
        assert_eq!(
            parsed.headers,
            ["(request-target)", "host", "date"].map(str::to_owned),
            "the header list keeps its order"
        );
        assert_eq!(
            parsed.signature,
            b"hi".as_slice(),
            "the quoted signature decodes"
        );
        assert_eq!(parsed.created, Some(NOW_SECONDS), "the quoted timestamp");
        assert_eq!(
            parsed.expires,
            Some(NOW_SECONDS + 3600),
            "the padded bare timestamp"
        );
    }

    #[test]
    fn bare_parameters_are_read_too() {
        let parsed =
            parse_signature_header(&format!("keyId={KEY_ID},headers=date,signature={HI_B64}"))
                .unwrap();
        assert_eq!(parsed.key_id, KEY_ID, "an unquoted key id");
        assert_eq!(parsed.algorithm, None, "no algorithm is the draft default");
        assert_eq!(
            parsed.headers,
            ["date"].map(str::to_owned),
            "an unquoted single header"
        );
        assert_eq!(parsed.signature, b"hi".as_slice(), "an unquoted signature");
    }

    #[test]
    fn the_authorization_spelling_parses_like_the_bare_signature_header() {
        let signed = sign_request(
            "GET",
            "https://peer.example/users/alice",
            None,
            &fixture_key(),
            now(),
        )
        .unwrap();
        let bare = parse_signature_header(&signed.signature).unwrap();
        let prefixed = parse_signature_header(&format!("Signature {}", signed.signature)).unwrap();
        assert_eq!(
            prefixed, bare,
            "the scheme prefix is dropped, nothing else changes"
        );
    }

    #[test]
    fn malformed_signature_headers_are_rejected() {
        for (header, expected) in [
            (String::new(), SignatureError::Missing),
            ("garbage".to_owned(), MALFORMED_PAIR),
            (
                format!(r#"keyId="a",signature="{HI_B64}""#),
                SignatureError::Missing,
            ),
            (r#"keyId="unclosed"#.to_owned(), MALFORMED_QUOTE),
            (
                format!(r#"keyId="a",bad name="x",headers="b",signature="{HI_B64}""#),
                MALFORMED_NAME,
            ),
            (
                r#"keyId="a",headers="b",signature="not base64!""#.to_owned(),
                MALFORMED_BASE64,
            ),
            (
                format!(r#"keyId="a",headers="b",signature="{HI_B64}",created="soon""#),
                MALFORMED_TIMESTAMP,
            ),
        ] {
            assert_eq!(
                parse_signature_header(&header).err(),
                Some(expected),
                "`{header}` must not parse"
            );
        }
    }

    #[test]
    fn other_signature_algorithms_are_unsupported() {
        let lines = get_lines("get /x");
        for algorithm in ["hmac-sha256", "rsa-sha512", "ecdsa-p256-sha256"] {
            let header = crafted_signature(
                "(request-target) host date",
                &lines,
                &format!(r#",algorithm="{algorithm}""#),
            );
            let parsed = parse_signature_header(&header).unwrap();
            let request = SignedRequestParts {
                method: "GET",
                path_and_query: "/x",
                headers: received_headers(&header, Some(NOW_FIXDATE), None),
            };
            assert_eq!(
                verify_now(&parsed, &request),
                Err(SignatureError::UnsupportedAlgorithm),
                "`{algorithm}` is neither rsa-sha256 nor hs2019"
            );
        }
    }

    #[test]
    fn hs2019_mixed_case_and_the_absent_algorithm_verify_as_rsa_sha256() {
        let lines = get_lines("get /x");
        for params in [r#",algorithm="hs2019""#, r#",algorithm="RSA-SHA256""#, ""] {
            let header = crafted_signature("(request-target) host date", &lines, params);
            let parsed = parse_signature_header(&header).unwrap();
            let request = SignedRequestParts {
                method: "GET",
                path_and_query: "/x",
                headers: received_headers(&header, Some(NOW_FIXDATE), None),
            };
            assert_eq!(
                verify_now(&parsed, &request),
                Ok(()),
                "algorithm parameter `{params:?}` accepts the RSA-SHA256 text"
            );
        }
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
