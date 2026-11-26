//! A hand-rolled RFC 9421 signer for tests.
//!
//! Deliberately independent of the verifier's own base builder: it writes the
//! signature base as literal text, so a bug shared by both halves cannot
//! hide. Test-only; outbound federation stays cavage.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::util::keys::sign_sha256;

/// What an RFC 9421 test request is signed with and as.
#[derive(Clone, Copy, Debug)]
pub struct Rfc9421Key<'a> {
    /// The `keyid` parameter.
    pub key_id: &'a str,
    /// The signing key as PEM.
    pub private_pem: &'a str,
    /// The `created` parameter, unix seconds.
    pub created: i64,
}

/// The `Content-Digest` value of `body`: `sha-256=:<base64>:`.
#[must_use]
pub fn content_digest(body: &[u8]) -> String {
    format!("sha-256=:{}:", BASE64.encode(Sha256::digest(body)))
}

/// The `Signature-Input` member value for `components`.
#[must_use]
pub fn params_for(components: &str, key: &Rfc9421Key<'_>) -> String {
    format!(
        r#"({components});created={};keyid="{}";alg="rsa-v1_5-sha256""#,
        key.created, key.key_id
    )
}

/// The `Signature` header value for label `label` over the literal
/// signature `base`.
///
/// # Panics
///
/// When `key.private_pem` is not a usable private key.
#[must_use]
pub fn signature_over(label: &str, base: &str, key: &Rfc9421Key<'_>) -> String {
    let raw = sign_sha256(key.private_pem, base.as_bytes()).unwrap();
    format!("{label}=:{}:", BASE64.encode(raw))
}

/// The headers a peer sends for `method` `url`, signed over `@method`,
/// `@target-uri` and — when there is a `body` — `content-digest`, as
/// `(name, value)` pairs: `host`, the digest, `signature-input`, `signature`.
///
/// # Panics
///
/// When `url` is not an absolute URL with a host or `key` cannot sign.
#[must_use]
pub fn sign(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    key: &Rfc9421Key<'_>,
) -> Vec<(&'static str, String)> {
    let parsed = Url::parse(url).unwrap();
    let name = parsed.host_str().unwrap();
    let host = parsed
        .port()
        .map_or_else(|| name.to_owned(), |port| format!("{name}:{port}"));
    let digest = body.map(content_digest);
    let (components, digest_line) = digest.as_ref().map_or_else(
        || (r#""@method" "@target-uri""#, String::new()),
        |value| {
            (
                r#""@method" "@target-uri" "content-digest""#,
                format!("\n\"content-digest\": {value}"),
            )
        },
    );
    let params = params_for(components, key);
    let base = format!(
        "\"@method\": {method}\n\"@target-uri\": {url}{digest_line}\n\"@signature-params\": {params}"
    );
    let mut headers = vec![("host", host)];
    if let Some(value) = digest {
        headers.push(("content-digest", value));
    }
    headers.push(("signature-input", format!("sig1={params}")));
    headers.push(("signature", signature_over("sig1", &base, key)));
    headers
}
