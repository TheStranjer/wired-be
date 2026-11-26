//! Inbound RFC 9421 HTTP Message Signatures.
//!
//! The successor of the draft-cavage scheme in [`super`]: the signature
//! arrives as a `Signature-Input` dictionary (what is covered, plus
//! `created`, `keyid` and `alg`) and a `Signature` dictionary holding the
//! bytes, both keyed by a label. Mastodon sends it as the second knock of its
//! double-knock; Fedify-based servers send it first.
//!
//! The rules follow the cavage ones so a peer is judged the same way under
//! either scheme:
//!
//! - `alg` is `rsa-v1_5-sha256` (or absent: the key decides, and fediverse
//!   keys are RSA). `ed25519` and the other registered algorithms are
//!   refused: actor documents publish no key this verifier could use them
//!   with.
//! - the covered components include `@method`, and `@target-uri` or
//!   `@authority` with `@path`; a `POST` also covers `content-digest`.
//!   [`verify_content_digest`] then checks that header (RFC 9530,
//!   `sha-256`) against the body actually received.
//! - `created` is required and must lie within the cavage `Date` window; a
//!   past `expires` rejects the signature.
//! - the `keyid` names the key exactly as a cavage `keyId` does, so the
//!   shared key lookup resolves it.

use axum::http::HeaderMap;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Utc};
use sha2::{Digest as _, Sha256};

mod base;
mod parse;
#[cfg(test)]
mod tests;

use self::{
    base::signature_base,
    parse::{Input, LabeledSignature, parse_signature_input, parse_signatures},
};
use super::{
    MALFORMED_BASE64, MALFORMED_DIGEST, MALFORMED_TIMESTAMP, POST_METHOD, SignatureError,
    SignedRequestParts,
    checks::header_value,
    freshness::{check_expiry, check_window},
};
use crate::util::keys::verify_sha256;

/// The request header carrying the covered components and parameters.
const SIGNATURE_INPUT_HEADER: &str = "signature-input";
/// The request header carrying the signature bytes (RFC 9421 form).
const SIGNATURE_HEADER_NAME: &str = "signature";
/// The request header carrying the body digest (RFC 9530).
const CONTENT_DIGEST_HEADER: &str = "content-digest";
/// The one `alg` this verifier checks: RSASSA-PKCS1-v1_5 over SHA-256.
const RSA_V1_5_SHA256: &str = "rsa-v1_5-sha256";
/// The derived component naming the request's method.
const METHOD: &str = "@method";
/// The derived component naming the whole request URL.
const TARGET_URI: &str = "@target-uri";
/// The derived component naming the request's authority.
const AUTHORITY: &str = "@authority";
/// The derived component naming the request's path.
const PATH: &str = "@path";
/// The digest algorithm name RFC 9530 gives SHA-256.
const SHA_256: &str = "sha-256";

/// One RFC 9421 signature a request presents: a `Signature-Input` member
/// joined with the `Signature` member of the same label.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rfc9421Signature {
    /// The member as parsed, which also rebuilds the signature base.
    input: Input,
    /// The `keyid` parameter: which key claims to have signed.
    key_id: String,
    /// The `alg` parameter, `None` when absent.
    algorithm: Option<String>,
    /// The `created` parameter as unix seconds.
    created: Option<i64>,
    /// The `expires` parameter as unix seconds.
    expires: Option<i64>,
    /// The decoded signature bytes.
    signature: Vec<u8>,
}

impl Rfc9421Signature {
    /// The `keyid` parameter, which names the signing key like a cavage
    /// `keyId`.
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }
}

/// Whether the request presents RFC 9421 signature material at all: a
/// `Signature-Input` header.
#[must_use]
pub fn rfc9421_offered(headers: &HeaderMap) -> bool {
    headers.contains_key(SIGNATURE_INPUT_HEADER)
}

/// Reads the RFC 9421 signatures a request presents: every `Signature-Input`
/// member that has a `Signature` member under the same label and a `keyid`.
///
/// A request without a `Signature-Input` header presents none (`Ok` of an
/// empty list).
///
/// # Errors
///
/// [`SignatureError::Missing`] when `Signature-Input` is present without a
/// `Signature`, [`SignatureError::Malformed`] when either header is not a
/// dictionary of the expected shape (including a draft-cavage `Signature`).
pub fn parse_rfc9421(headers: &HeaderMap) -> Result<Vec<Rfc9421Signature>, SignatureError> {
    let Some(input_text) = header_value(headers, SIGNATURE_INPUT_HEADER) else {
        return Ok(Vec::new());
    };
    let signature_text =
        header_value(headers, SIGNATURE_HEADER_NAME).ok_or(SignatureError::Missing)?;
    let inputs = parse_signature_input(&input_text)?;
    let signatures = parse_signatures(&signature_text)?;
    Ok(inputs
        .into_iter()
        .filter_map(|input| join_member(input, &signatures).ok())
        .collect())
}

/// Joins a `Signature-Input` member with the `Signature` bytes of its label.
fn join_member(
    input: Input,
    signatures: &[LabeledSignature],
) -> Result<Rfc9421Signature, SignatureError> {
    let signature = signatures
        .iter()
        .find(|entry| entry.0 == input.label)
        .map(|entry| entry.1.clone())
        .ok_or(SignatureError::Missing)?;
    let key_id = input.text_param("keyid").ok_or(SignatureError::Missing)?;
    Ok(Rfc9421Signature {
        algorithm: input.text_param("alg"),
        created: input.int_param("created")?,
        expires: input.int_param("expires")?,
        key_id,
        signature,
        input,
    })
}

/// Verifies one RFC 9421 signature against the received request.
///
/// Checks the algorithm, the covered components and the time rules (the
/// same window as the cavage `Date`), rebuilds the signature base from `request` and
/// verifies the RSASSA-PKCS1-v1_5 SHA-256 signature over it with
/// `public_pem` at `now`. `scheme` is the scheme of the public URL
/// (`https`), which `@target-uri` needs and a received request does not
/// carry. The body is not read here; see [`verify_content_digest`].
///
/// # Errors
///
/// [`SignatureError::UnsupportedAlgorithm`], [`SignatureError::Missing`],
/// [`SignatureError::Malformed`] or [`SignatureError::Expired`] when the
/// signature breaks a rule; [`SignatureError::BadSignature`] when the base
/// does not verify against `public_pem` — including when that key does not
/// parse.
pub fn verify_rfc9421(
    signature: &Rfc9421Signature,
    request: &SignedRequestParts<'_>,
    scheme: &str,
    public_pem: &str,
    now: DateTime<Utc>,
) -> Result<(), SignatureError> {
    if signature
        .algorithm
        .as_deref()
        .is_some_and(|name| name != RSA_V1_5_SHA256)
    {
        return Err(SignatureError::UnsupportedAlgorithm);
    }
    check_covered(signature, request.method)?;
    check_expiry(signature.expires, now)?;
    let created = signature.created.ok_or(SignatureError::Missing)?;
    check_window(
        DateTime::from_timestamp(created, 0).ok_or(MALFORMED_TIMESTAMP)?,
        now,
    )?;
    let base = signature_base(&signature.input, request, scheme)?;
    if verify_sha256(public_pem, base.as_bytes(), &signature.signature) {
        return Ok(());
    }
    Err(SignatureError::BadSignature)
}

/// The method and the target must be covered, and for a `POST` the body's
/// `content-digest` too.
fn check_covered(signature: &Rfc9421Signature, method: &str) -> Result<(), SignatureError> {
    let covers = |name: &str| signature.input.components.iter().any(|item| item == name);
    let target = covers(TARGET_URI) || (covers(AUTHORITY) && covers(PATH));
    let digest = !method.eq_ignore_ascii_case(POST_METHOD) || covers(CONTENT_DIGEST_HEADER);
    if covers(METHOD) && target && digest {
        return Ok(());
    }
    Err(SignatureError::Missing)
}

/// Checks the request's `Content-Digest` header against the received
/// `body` (RFC 9530): the `sha-256` member must be there and describe it.
///
/// Other digest algorithms in the header are ignored. A missing header is
/// an error exactly for requests that carry a body.
///
/// # Errors
///
/// [`SignatureError::Missing`] for a headerless request with a body,
/// [`SignatureError::Malformed`] for an unparsable header,
/// [`SignatureError::UnsupportedAlgorithm`] when no `sha-256` member is
/// present, [`SignatureError::BadDigest`] when it does not describe `body`.
pub fn verify_content_digest(headers: &HeaderMap, body: &[u8]) -> Result<(), SignatureError> {
    let Some(text) = header_value(headers, CONTENT_DIGEST_HEADER) else {
        return if body.is_empty() {
            Ok(())
        } else {
            Err(SignatureError::Missing)
        };
    };
    let actual: [u8; 32] = Sha256::digest(body).into();
    let mut checked = false;
    for member in text.split(',') {
        let (name, value) = member.trim().split_once('=').ok_or(MALFORMED_DIGEST)?;
        if !name.eq_ignore_ascii_case(SHA_256) {
            continue;
        }
        let (encoded, _params) = value
            .strip_prefix(':')
            .and_then(|rest| rest.split_once(':'))
            .ok_or(MALFORMED_DIGEST)?;
        let claimed = BASE64.decode(encoded).map_err(|_err| MALFORMED_BASE64)?;
        if claimed != actual {
            return Err(SignatureError::BadDigest);
        }
        checked = true;
    }
    if checked {
        Ok(())
    } else {
        Err(SignatureError::UnsupportedAlgorithm)
    }
}
