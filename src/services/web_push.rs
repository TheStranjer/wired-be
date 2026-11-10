//! Web Push sender: the RFC 8291 `aes128gcm` payload encryption, the VAPID
//! (RFC 8292) request signing and the one POST that delivers a payload to a
//! browser's push service (WBE-149).
//!
//! The reference is Pleroma's `Pleroma.Web.Push.Impl`
//! (`lib/pleroma/web/push/impl.ex`), which delegates the wire work to the
//! `web_push_encryption` Hex package; this module *is* that package's
//! sender, split the way the two RFCs are:
//!
//! - [`encrypt_payload`] implements the RFC 8291 content coding: a random
//!   16-octet salt, a fresh ephemeral P-256 key pair, ECDH against the
//!   subscription's `p256dh` key, the two HKDF-SHA-256 steps the RFC spells
//!   out (the `WebPush: info\0` + client key + server key combine with the
//!   auth secret, then the `Content-Encoding:` derives), one record of
//!   `plaintext || 0x02` sealed with AES-128-GCM, and the
//!   `salt || rs || idlen || server public key` header in front.
//! - [`vapid_headers`] implements the RFC 8292 authorization: an ES256 JWT
//!   whose `aud` is the endpoint's origin, `exp` twelve hours out and `sub`
//!   the instance's [`vapid_subject`], sent as `Authorization: vapid t=…, k=…`
//!   beside the instance public key ([`crate::services::vapid`]).
//! - [`send`] puts the two together and POSTs one subscription's payload
//!   with the headers a push service expects, classifying the answer into
//!   [`PushError::Gone`] (the subscription is dead; the caller deletes it)
//!   or [`PushError::Retryable`] (5xx, 429, transport: the queue tries
//!   again).
//!
//! The delivery decisions that surround the send — which alert wakes a
//! subscription ([`alert_enabled`], whose `policy` mate the
//! [`push.deliver` job](crate::jobs::web_push) resolves against
//! [`crate::models::follow`]), and what the JSON payload says
//! ([`payload_json`]) — are pure here, so the job is thin glue. Like
//! Pleroma the payload is the Mastodon-shaped JSON document
//! (`access_token`, `notification_id`, `title`, `body`, …); the ticket
//! fixes its content: the title is `<actor display name> <verb>` and the
//! body the status's plain text, empty when the recipient set
//! `hide_notification_contents`.

use std::{error::Error, fmt};

use aes_gcm::{
    Aes128Gcm, KeyInit as _,
    aead::{Aead as _, Nonce},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, TimeDelta, Utc};
use hkdf::Hkdf;
use p256::{
    SecretKey,
    ecdsa::{Signature, SigningKey, signature::Signer as _},
    elliptic_curve::{Generate as _, sec1::ToSec1Point as _},
    pkcs8::DecodePrivateKey as _,
};
use rand::RngCore as _;
use serde_json::{Value, json};
use sha2_011::Sha256;
use sqlx::PgPool;
use tracing::debug;
use url::Url;
use uuid::Uuid;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    http_client::HttpClient,
    models::{
        notification,
        web_push_subscription::{self, WebPushSubscription},
    },
};

/// RFC 8291 §3.4: the `aes128gcm` header's random salt length, in octets.
const SALT_LEN: usize = 16;
/// RFC 8291 §3.2: the authentication secret's length, in octets.
const AUTH_SECRET_LEN: usize = 16;
/// RFC 8291 §3.4: the combined input keying material's length.
const IKM_LEN: usize = 32;
/// RFC 8291 §3.4: `L_cek`, the content encryption key's length.
const CEK_LEN: usize = 16;
/// RFC 8291 §3.4: `L_nonce`, the AES-GCM nonce's length.
const NONCE_LEN: usize = 12;
/// The `rs` (record size) the header carries, big-endian in four octets:
/// the ticket's single-record size and what a push service is required to
/// accept (RFC 8291 §4).
const RECORD_SIZE: u32 = 4096;
/// [`RECORD_SIZE`] as the four header octets.
const RECORD_SIZE_BE: [u8; 4] = RECORD_SIZE.to_be_bytes();
/// RFC 8291 §4: the longest payload one record carries — `rs` minus the
/// one-octet padding delimiter and the 16-octet AEAD tag.
const MAX_PLAINTEXT_LEN: usize = 4096 - 1 - 16;
/// RFC 8188 §2.1: the `keyid` length octet — the uncompressed P-256 point
/// it announces is always [`KEY_ID_BYTES`] octets (the `0x04` tag plus the
/// two 32-byte coordinates).
const KEY_ID_LEN: u8 = 65;
/// The byte length of the point [`KEY_ID_LEN`] announces.
const KEY_ID_BYTES: usize = 65;
/// RFC 8291 §4: the padding delimiter that closes the single record.
const PADDING_DELIMITER: u8 = 0x02;
/// RFC 8291 §3.3: the label that opens the key-combining `info`.
const KEY_INFO_LABEL: &[u8] = b"WebPush: info";
/// RFC 8291 §3.4: the content encryption key's `info`.
const CEK_INFO: &[u8] = b"Content-Encoding: aes128gcm\0";
/// RFC 8291 §3.4: the nonce's `info`.
const NONCE_INFO: &[u8] = b"Content-Encoding: nonce\0";

/// The `TTL` header every push message carries: four weeks, Pleroma's
/// `web_push_encryption` default and Mastodon's push TTL.
pub const TTL_HEADER: &str = "2419200";
/// The `Urgency` header: the middle of RFC 8030 §5.3's three levels, which
/// a notification delivery is.
pub const URGENCY_HEADER: &str = "normal";
/// RFC 8292 §2.1 caps a VAPID JWT's life at 24 hours; the ticket signs
/// half of that.
const VAPID_JWT_LIFETIME: TimeDelta = TimeDelta::hours(12);
/// The payload's `preferred_locale`: this instance has no localized
/// notification strings yet, so every wake reads English — the same
/// constant [`crate::mailer`] letters in.
const PREFERRED_LOCALE: &str = "en";

/// The verbs [`payload_json`] pairs with the actor's display name, by
/// notification type (Mastodon's push wording; Pleroma's `format_body`
/// clauses in spirit). [`DEFAULT_VERB`] covers a type not listed.
const VERBS: &[(&str, &str)] = &[
    (notification::TYPE_FOLLOW, "followed you"),
    (notification::TYPE_FOLLOW_REQUEST, "requested to follow you"),
    (notification::TYPE_FAVOURITE, "favourited your post"),
    (notification::TYPE_REBLOG, "boosted your post"),
    (notification::TYPE_MENTION, "mentioned you"),
    (notification::TYPE_EMOJI_REACTION, "reacted to your post"),
    ("status", "posted"),
    ("update", "edited a post"),
    ("poll", "ended a poll"),
    ("pleroma:chat_mention", "sent you a chat message"),
    ("admin.sign_up", "signed up"),
    ("admin.report", "filed a report"),
];
/// The title verb for a notification type [`VERBS`] does not name.
const DEFAULT_VERB: &str = "sent you a notification";

/// Why a push delivery did not happen, as [`send`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushError {
    /// The subscription is dead: the push service answered 404 or 410, or
    /// the stored keys can encrypt nothing. The caller deletes the
    /// subscription — sending to it will never work again.
    Gone,
    /// The delivery may succeed later: a 5xx, a 429, a transport failure,
    /// or any other refusal the ticket leaves on the retryable side. The
    /// string states what happened, for the job's `last_error`.
    Retryable(String),
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gone => f.write_str("the push subscription is gone"),
            Self::Retryable(detail) => write!(f, "retryable push failure: {detail}"),
        }
    }
}

impl Error for PushError {}

/// The AES key and nonce one `aes128gcm` record is sealed with, derived per
/// RFC 8291 §3.4 from the ECDH secret, the auth secret and the header salt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CryptoContext {
    /// The 16-octet content encryption key.
    cek: [u8; CEK_LEN],
    /// The 12-octet AES-GCM nonce.
    nonce: [u8; NONCE_LEN],
}

/// Encrypts `plaintext` as one RFC 8291 `aes128gcm` push body.
///
/// The subscription's keys are `p256dh_b64url` (the client's uncompressed
/// P-256 public point) and `auth_b64url` (its 16-octet auth secret), both
/// URL-safe base64. Fresh random salt and ephemeral P-256 key, the derived
/// key and nonce sealing `plaintext || 0x02` into a single record, and the
/// `salt || rs || idlen || server public key` header in front.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for keys that are not URL-safe base64, not a
/// point on P-256, or not 16 octets of auth secret;
/// [`AppError::PayloadTooLarge`] when `plaintext` exceeds
/// `MAX_PLAINTEXT_LEN`; [`AppError::Internal`] for a CSPRNG, HKDF or
/// AES-GCM failure.
pub fn encrypt_payload(
    plaintext: &[u8],
    p256dh_b64url: &str,
    auth_b64url: &str,
) -> AppResult<Vec<u8>> {
    let mut salt = [0_u8; SALT_LEN];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    let server_secret = SecretKey::try_generate().map_err(|err| {
        AppError::Internal(format!("could not generate the push ECDH key: {err}"))
    })?;
    encrypt_record(plaintext, p256dh_b64url, auth_b64url, &server_secret, &salt)
}

/// The deterministic core of [`encrypt_payload`]: the same coding with the
/// salt and ephemeral key handed in, so the RFC 8291 Appendix A vector test
/// pins every intermediate value.
///
/// # Errors
///
/// As [`encrypt_payload`].
fn encrypt_record(
    plaintext: &[u8],
    p256dh_b64url: &str,
    auth_b64url: &str,
    server_secret: &SecretKey,
    salt: &[u8; SALT_LEN],
) -> AppResult<Vec<u8>> {
    if plaintext.len() > MAX_PLAINTEXT_LEN {
        return Err(AppError::PayloadTooLarge(format!(
            "a web push payload is at most {MAX_PLAINTEXT_LEN} plaintext bytes \
             (record size {RECORD_SIZE})"
        )));
    }
    let client_public = decode_public_key(p256dh_b64url)?;
    let auth_secret = decode_auth_secret(auth_b64url)?;
    let ua_point = client_public.to_sec1_point(false);
    let as_point = server_secret.public_key().to_sec1_point(false);
    let shared = p256::ecdh::diffie_hellman(
        &server_secret.to_nonzero_scalar(),
        client_public.as_affine(),
    );
    let context = derive_context(
        shared.raw_secret_bytes(),
        &auth_secret,
        ua_point.as_bytes(),
        as_point.as_bytes(),
        salt,
    )?;
    let ciphertext = seal_record(&context, plaintext)?;
    let mut message = Vec::with_capacity(SALT_LEN + 4 + 1 + KEY_ID_BYTES + ciphertext.len());
    message.extend_from_slice(salt);
    message.extend_from_slice(&RECORD_SIZE_BE);
    message.push(KEY_ID_LEN);
    message.extend_from_slice(as_point.as_bytes());
    message.extend_from_slice(&ciphertext);
    Ok(message)
}

/// RFC 8291 §3.3 then §3.4: the ECDH secret combined with the auth secret
/// into the input keying material, then that expanded against the header
/// salt into the record's content key and nonce.
///
/// # Errors
///
/// [`AppError::Internal`] if HKDF refuses a length (it cannot for these
/// fixed sizes; the failure is carried, never swallowed).
fn derive_context(
    ecdh_secret: &[u8],
    auth_secret: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
    salt: &[u8],
) -> AppResult<CryptoContext> {
    let key_info = combined_key_info(ua_public, as_public);
    let mut ikm = [0_u8; IKM_LEN];
    hkdf_sha256(auth_secret, ecdh_secret, &key_info, &mut ikm)?;
    let mut cek = [0_u8; CEK_LEN];
    hkdf_sha256(salt, &ikm, CEK_INFO, &mut cek)?;
    let mut nonce = [0_u8; NONCE_LEN];
    hkdf_sha256(salt, &ikm, NONCE_INFO, &mut nonce)?;
    Ok(CryptoContext { cek, nonce })
}

/// The RFC 8291 §3.3 `key_info`:
/// `"WebPush: info" || 0x00 || ua_public || as_public`.
#[must_use]
fn combined_key_info(ua_public: &[u8], as_public: &[u8]) -> Vec<u8> {
    let mut info = Vec::with_capacity(KEY_INFO_LABEL.len() + 1 + ua_public.len() + as_public.len());
    info.extend_from_slice(KEY_INFO_LABEL);
    info.push(0x00);
    info.extend_from_slice(ua_public);
    info.extend_from_slice(as_public);
    info
}

/// One HKDF-SHA-256 (RFC 5869) extract-and-expand into `out`.
///
/// # Errors
///
/// [`AppError::Internal`] if the expand fails (it cannot for an output of
/// at most 8 160 octets; the failure is kept visible).
fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8], out: &mut [u8]) -> AppResult<()> {
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info, out)
        .map_err(|err| AppError::Internal(format!("web push HKDF failed: {err}")))
}

/// Seals the single record: `plaintext || 0x02` through AES-128-GCM,
/// ciphertext with its 16-octet tag appended, as the content coding puts
/// them on the wire.
///
/// # Errors
///
/// [`AppError::Internal`] if the cipher refuses its (fixed-length) inputs.
fn seal_record(context: &CryptoContext, plaintext: &[u8]) -> AppResult<Vec<u8>> {
    let cipher = Aes128Gcm::new_from_slice(&context.cek)
        .map_err(|err| AppError::Internal(format!("could not build the push cipher: {err}")))?;
    let nonce = Nonce::<Aes128Gcm>::from(context.nonce);
    let mut record = Vec::with_capacity(plaintext.len() + 1);
    record.extend_from_slice(plaintext);
    record.push(PADDING_DELIMITER);
    cipher
        .encrypt(&nonce, record.as_slice())
        .map_err(|err| AppError::Internal(format!("AES-GCM encryption failed: {err}")))
}

/// The subscription's `key_p256dh` as a parsed client public key.
///
/// # Errors
///
/// [`AppError::Unprocessable`] when the stored text is not URL-safe base64
/// of a point on P-256 (an off-curve key is exactly the attack RFC 8291 §7
/// warns about, so it is refused, not used).
fn decode_public_key(p256dh_b64url: &str) -> AppResult<p256::PublicKey> {
    let bytes = decode_urlsafe(p256dh_b64url, "p256dh")?;
    p256::PublicKey::from_sec1_bytes(&bytes).map_err(|err| {
        AppError::Unprocessable(format!(
            "the subscription p256dh key is not a P-256 public point: {err}"
        ))
    })
}

/// The subscription's `key_auth` as its raw 16-octet secret.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for non-base64 text or a length other than
/// the 16 octets RFC 8291 §3.2 fixes.
fn decode_auth_secret(auth_b64url: &str) -> AppResult<[u8; AUTH_SECRET_LEN]> {
    let bytes = decode_urlsafe(auth_b64url, "auth")?;
    bytes.try_into().map_err(|_wrong: Vec<u8>| {
        AppError::Unprocessable(format!(
            "the subscription auth secret is not {AUTH_SECRET_LEN} octets"
        ))
    })
}

/// URL-safe base64 without padding — padding tolerated: a trailing run of
/// `=` is trimmed first, so keys any client spelled still decode.
///
/// # Errors
///
/// [`AppError::Unprocessable`] when the alphabet does not decode.
fn decode_urlsafe(value: &str, which: &str) -> AppResult<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(value.trim_end_matches('='))
        .map_err(|err| {
            AppError::Unprocessable(format!("the {which} key is not URL-safe base64: {err}"))
        })
}

/// Test support: decrypts an `aes128gcm` push body back to its plaintext.
///
/// The client's half of RFC 8291: its private key (URL-safe base64 of the
/// 32-octet scalar) and auth secret read the header key and seal the tests
/// deliver. The record size is parsed and ignored (a receiver MAY ignore
/// it, RFC 8291 §4); a record not ending in the `0x02` delimiter is
/// discarded, and zero padding after the delimiter is trimmed.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for a truncated body, undecodable keys or a
/// missing padding delimiter; [`AppError::Internal`] when the tag fails to
/// verify (a corrupted or forged body) or a primitive fails.
#[cfg(any(test, feature = "integration-tests"))]
pub fn decrypt_payload(
    message: &[u8],
    client_private_b64: &str,
    auth_b64url: &str,
) -> AppResult<Vec<u8>> {
    let (salt, rest) = split_fixed::<SALT_LEN>(message, "salt")?;
    let (_record_size, rest) = split_fixed::<4>(rest, "record size")?;
    let (key_id_len, rest) = rest
        .split_first()
        .ok_or_else(|| AppError::Unprocessable("push body has no keyid length".to_owned()))?;
    let key_id_len = usize::from(*key_id_len);
    if rest.len() < key_id_len {
        return Err(AppError::Unprocessable(
            "push body ends inside its keyid".to_owned(),
        ));
    }
    let (server_public, ciphertext) = rest.split_at(key_id_len);
    let client_secret = decode_client_private(client_private_b64)?;
    let server_public_key = p256::PublicKey::from_sec1_bytes(server_public).map_err(|err| {
        AppError::Unprocessable(format!("the push body carries no P-256 point: {err}"))
    })?;
    let auth_secret = decode_auth_secret(auth_b64url)?;
    let ua_point = client_secret.public_key().to_sec1_point(false);
    let shared = p256::ecdh::diffie_hellman(
        &client_secret.to_nonzero_scalar(),
        server_public_key.as_affine(),
    );
    let context = derive_context(
        shared.raw_secret_bytes(),
        &auth_secret,
        ua_point.as_bytes(),
        server_public,
        salt,
    )?;
    let cipher = Aes128Gcm::new_from_slice(&context.cek)
        .map_err(|err| AppError::Internal(format!("could not build the push cipher: {err}")))?;
    let nonce = Nonce::<Aes128Gcm>::from(context.nonce);
    let record = cipher.decrypt(&nonce, ciphertext).map_err(|err| {
        AppError::Internal(format!("the push record failed authentication: {err}"))
    })?;
    unpad_record(&record)
}

/// One fixed-size content-coding header field peeled off the front of a
/// body, beside the rest of it: the answer of [`split_fixed`].
type HeaderField<'a, const N: usize> = (&'a [u8; N], &'a [u8]);

/// Peels the content coding's leading 16-octet salt or 4-octet `rs` off the
/// front of a slice by the same name it reports under.
///
/// # Errors
///
/// [`AppError::Unprocessable`] when the body is shorter than the field.
#[cfg(any(test, feature = "integration-tests"))]
fn split_fixed<'msg, const N: usize>(
    message: &'msg [u8],
    field: &str,
) -> AppResult<HeaderField<'msg, N>> {
    message
        .split_first_chunk::<N>()
        .ok_or_else(|| AppError::Unprocessable(format!("the push body ends inside its {field}")))
}

/// The client's private scalar out of its URL-safe base64 text.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for undecodable or non-scalar bytes.
#[cfg(any(test, feature = "integration-tests"))]
fn decode_client_private(client_private_b64: &str) -> AppResult<SecretKey> {
    let bytes = URL_SAFE_NO_PAD
        .decode(client_private_b64.trim_end_matches('='))
        .map_err(|err| {
            AppError::Unprocessable(format!("the client private key does not decode: {err}"))
        })?;
    SecretKey::from_slice(&bytes).map_err(|err| {
        AppError::Unprocessable(format!(
            "the client private key is not a P-256 scalar: {err}"
        ))
    })
}

/// Trims the padding delimiter and the zero padding behind it (RFC 8188 §3)
/// off a decrypted record.
///
/// # Errors
///
/// [`AppError::Unprocessable`] when the record does not end in the `0x02`
/// delimiter — RFC 8291 §4 requires a receiver to discard such a message.
#[cfg(any(test, feature = "integration-tests"))]
fn unpad_record(record: &[u8]) -> AppResult<Vec<u8>> {
    let (payload, tail) = record.split_at(record.len().saturating_sub(1));
    if tail.first() != Some(&PADDING_DELIMITER) {
        return Err(AppError::Unprocessable(
            "the push record has no padding delimiter".to_owned(),
        ));
    }
    let kept = payload
        .iter()
        .rposition(|&byte| byte != 0x00)
        .map_or(0, |last| last + 1);
    let (plaintext, _padding) = payload.split_at(kept);
    Ok(plaintext.to_vec())
}

/// The VAPID authorization for one push `POST`: an ES256 JWT.
///
/// The token is signed over the endpoint's origin, issued now and expiring
/// twelve hours later, with `subject` as its `sub`, rendered as
/// the header value `vapid t=<jwt>, k=<public key>`.
///
/// Returns the header value and the JWT's expiry, so a caller with a long
/// retry chain can tell when to re-sign.
///
/// # Errors
///
/// [`AppError::Unprocessable`] when `endpoint` is not an `http(s)` URL with
/// a host (there would be no origin to sign); [`AppError::Internal`] when
/// `private_pem` is not a PKCS#8 P-256 key or the ECDSA operation fails —
/// the messages name the failure, never key material.
pub fn vapid_headers(
    endpoint: &str,
    private_pem: &str,
    public_key_b64: &str,
    subject: &str,
) -> AppResult<(String, DateTime<Utc>)> {
    let audience = endpoint_origin(endpoint)?;
    let expires_at = Utc::now() + VAPID_JWT_LIFETIME;
    let header = URL_SAFE_NO_PAD.encode(json!({ "typ": "JWT", "alg": "ES256" }).to_string());
    let claims = URL_SAFE_NO_PAD.encode(
        json!({
            "aud": audience,
            "exp": expires_at.timestamp(),
            "sub": subject,
        })
        .to_string(),
    );
    let signing_input = format!("{header}.{claims}");
    let signing_key = SigningKey::from_pkcs8_pem(private_pem).map_err(|err| {
        AppError::Internal(format!(
            "the stored VAPID private key does not parse: {err}"
        ))
    })?;
    let signature: Signature = signing_key.sign(signing_input.as_bytes());
    let token = format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    );
    Ok((format!("vapid t={token}, k={public_key_b64}"), expires_at))
}

/// The JWT `aud`: the endpoint URL's origin — `scheme://host`, the port
/// appended only when it is not the scheme's default, the normalization a
/// push service compares against.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for an unparsable URL, a non-`http(s)`
/// scheme or a missing host.
fn endpoint_origin(endpoint: &str) -> AppResult<String> {
    let parsed = Url::parse(endpoint)
        .map_err(|err| AppError::Unprocessable(format!("the push endpoint is not a URL: {err}")))?;
    let host = parsed
        .host_str()
        .filter(|_| matches!(parsed.scheme(), "http" | "https"))
        .ok_or_else(|| {
            AppError::Unprocessable(
                "the push endpoint is not an http(s) URL with a host".to_owned(),
            )
        })?;
    let port = match parsed.port() {
        Some(port) if !is_default_port(parsed.scheme(), port) => format!(":{port}"),
        _ => String::new(),
    };
    Ok(format!("{}://{host}{port}", parsed.scheme()))
}

/// Whether `port` is `scheme`'s default — the port an origin spells without
/// it. `Url::port_or_known_default` cannot answer this (it returns the
/// *explicit* port whenever one is present), so the two schemes
/// [`endpoint_origin`] accepts carry the comparison directly.
#[must_use]
fn is_default_port(scheme: &str, port: u16) -> bool {
    matches!((scheme, port), ("http", 80) | ("https", 443))
}

/// The VAPID `sub` this instance signs with (RFC 8292 §2.1: a `mailto:` or
/// `https:` URI): the configured admin contact when there is one, the
/// instance's own origin otherwise.
#[must_use]
pub fn vapid_subject(config: &Config) -> String {
    config.contact_email.as_deref().map_or_else(
        || config.base_url.clone(),
        |email| format!("mailto:{email}"),
    )
}

/// Whether `notification_type` wakes this subscription.
///
/// The stored `alerts[type]` boolean decides, defaulting to on for a type
/// the object does not name (Pleroma's `Subscription.enabled?` treats an
/// absent flag as enabled, and the API's rows carry every known type
/// anyway).
#[must_use]
pub fn alert_enabled(alerts: &Value, notification_type: &str) -> bool {
    alerts
        .get(notification_type)
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// The title verb [`payload_json`] pairs with the actor's display name for
/// `notification_type` — `DEFAULT_VERB` for a type `VERBS` does not name.
#[must_use]
pub fn verb_for(notification_type: &str) -> &'static str {
    VERBS
        .iter()
        .find(|(kind, _verb)| *kind == notification_type)
        .map_or(DEFAULT_VERB, |(_kind, verb)| *verb)
}

/// Everything [`payload_json`] stitches into the delivered document, read
/// from the database by the [`push.deliver` job](crate::jobs::web_push).
#[derive(Clone, Debug)]
pub struct PushPayload<'a> {
    /// The subscription's access token string (`oauth_tokens.token`): what
    /// the client wakes with to fetch what changed.
    pub access_token: &'a str,
    /// The notification row's id.
    pub notification_id: Uuid,
    /// The notification's type, as stored.
    pub notification_type: &'a str,
    /// The actor's avatar URL.
    pub icon: &'a str,
    /// The actor's display name (its nickname when unnamed): the title's
    /// first half.
    pub actor_display_name: &'a str,
    /// The status's plain text, or empty — hidden contents and status-less
    /// types both send `""`.
    pub body: &'a str,
}

/// The Mastodon-shaped push payload as compact JSON.
///
/// The document: `access_token`, `notification_id` (hyphenated text,
/// decision 4), `notification_type`, `icon`, the fixed
/// `PREFERRED_LOCALE` `preferred_locale`, the `"<actor display name> <verb>"` title (the verb
/// from [`verb_for`]) and the plain-text `body`.
#[must_use]
pub fn payload_json(parts: &PushPayload<'_>) -> String {
    json!({
        "access_token": parts.access_token,
        "notification_id": parts.notification_id.to_string(),
        "notification_type": parts.notification_type,
        "icon": parts.icon,
        "preferred_locale": PREFERRED_LOCALE,
        "title": format!(
            "{} {}",
            parts.actor_display_name,
            verb_for(parts.notification_type)
        ),
        "body": parts.body,
    })
    .to_string()
}

/// Delivers one payload to one subscription: [`encrypt_payload`] the JSON,
/// then the `POST`.
///
/// The request carries `Content-Encoding: aes128gcm`,
/// `Content-Type: application/octet-stream`, `TTL: ` [`TTL_HEADER`],
/// `Urgency: ` [`URGENCY_HEADER`] and the pre-signed `Authorization` header
/// ([`vapid_headers`] signs per delivery; [`send`] just carries the text).
/// A 2xx answer is `Ok`. A 404 or 410 — or stored keys that cannot encrypt
/// at all — is [`PushError::Gone`]: the subscription will never receive
/// anything again and its caller deletes it. A 429, a 5xx or a transport
/// failure is [`PushError::Retryable`]; so is any other status, which the
/// ticket leaves on the safe side of a retry rather than a deletion.
///
/// # Errors
///
/// [`PushError`] per the mapping above: [`Gone`](PushError::Gone) for
/// 404/410 and unusable subscription keys, [`Retryable`](PushError::Retryable)
/// for every other failure.
pub async fn send(
    http: &HttpClient,
    subscription: &WebPushSubscription,
    payload_json: &str,
    authorization: &str,
) -> Result<(), PushError> {
    let encrypted = encrypt_payload(
        payload_json.as_bytes(),
        &subscription.key_p256dh,
        &subscription.key_auth,
    );
    let body = match encrypted {
        Ok(body) => body,
        Err(AppError::Unprocessable(detail)) => {
            debug!(subscription = %subscription.id, detail, "subscription keys are unusable");
            return Err(PushError::Gone);
        }
        Err(other) => return Err(PushError::Retryable(other.to_string())),
    };
    let response = http
        .post_bytes(
            &subscription.endpoint,
            &[
                ("authorization", authorization),
                ("content-encoding", "aes128gcm"),
                ("content-type", "application/octet-stream"),
                ("ttl", TTL_HEADER),
                ("urgency", URGENCY_HEADER),
            ],
            body,
        )
        .await
        .map_err(|err| PushError::Retryable(err.to_string()))?;
    classify_status(response.status)
}

/// Sorts one push-service answer into delivered, gone or retryable.
///
/// # Errors
///
/// [`PushError::Gone`] for 404/410, [`PushError::Retryable`] for anything
/// else outside the `2xx` range.
fn classify_status(status: u16) -> Result<(), PushError> {
    if (200..300).contains(&status) {
        Ok(())
    } else if matches!(status, 404 | 410) {
        Err(PushError::Gone)
    } else {
        Err(PushError::Retryable(format!(
            "the push service answered {status}"
        )))
    }
}

/// Queues one `push.deliver` job ([`crate::jobs::web_push::KIND_DELIVER`])
/// per subscription of `user_id`, for the freshly created notification
/// `notification_id`.
///
/// The [`crate::services::notify`] hook's muscle: a local recipient that
/// just gained a notification gains one delivery job per registered
/// browser, and a recipient with no subscriptions gains none. Returns how
/// many jobs went onto the queue.
///
/// # Errors
///
/// Any database failure (reading subscriptions, writing jobs) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn enqueue_deliveries(
    pool: &PgPool,
    notification_id: Uuid,
    user_id: Uuid,
) -> AppResult<usize> {
    use crate::{
        jobs::{QUEUE_DEFAULT, web_push},
        models::job::{NewJob, enqueue},
    };
    let subscriptions = web_push_subscription::for_user(pool, user_id).await?;
    for subscription in &subscriptions {
        let _job = enqueue(
            pool,
            NewJob {
                queue: QUEUE_DEFAULT,
                kind: web_push::KIND_DELIVER,
                args: web_push::job_args(notification_id, subscription.id),
                run_at: None,
                max_attempts: Some(web_push::MAX_ATTEMPTS),
                unique_key: None,
            },
        )
        .await?;
    }
    Ok(subscriptions.len())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Router,
        body::Bytes,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use tokio::net::TcpListener;

    use super::*;
    use crate::{
        config::{
            BASE_URL_VAR, CONTACT_EMAIL_VAR, DATABASE_URL_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
        },
        models::web_push_subscription::DEFAULT_POLICY,
        services::vapid,
    };

    /// RFC 8291 Appendix A: the header salt.
    const RFC_SALT_B64: &str = "DGv6ra1nlYgDCS1FRnbzlw";
    /// RFC 8291 Appendix A: the application server's private scalar.
    const RFC_AS_PRIVATE_B64: &str = "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw";
    /// RFC 8291 Appendix A: the application server's public point.
    const RFC_AS_PUBLIC_B64: &str = "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml\
        mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
    /// RFC 8291 Appendix A: the user agent's public point.
    const RFC_UA_PUBLIC_B64: &str = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx\
        aOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    /// RFC 8291 Appendix A: the user agent's private scalar.
    const RFC_UA_PRIVATE_B64: &str = "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94";
    /// RFC 8291 Appendix A: the authentication secret.
    const RFC_AUTH_B64: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    /// RFC 8291 Appendix A: the ECDH shared secret.
    const RFC_ECDH_SECRET_B64: &str = "kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs";
    /// RFC 8291 Appendix A: the combined input keying material.
    const RFC_IKM_B64: &str = "S4lYMb_L0FxCeq0WhDx813KgSYqU26kOyzWUdsXYyrg";
    /// RFC 8291 Appendix A: the content encryption key.
    const RFC_CEK_B64: &str = "oIhVW04MRdy2XN9CiKLxTg";
    /// RFC 8291 Appendix A: the record nonce.
    const RFC_NONCE_B64: &str = "4h_95klXJ5E_qnoN";
    /// RFC 8291 §5: the example plaintext.
    const RFC_PLAINTEXT: &[u8] = b"When I grow up, I want to be a watermelon";
    /// RFC 8291 §5: the whole encrypted body (header and record) as sent.
    const RFC_BODY_B64: &str = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7\
        VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95b\
        Qpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";

    /// Decodes a URL-safe base64 literal, test shorthand.
    fn b64(text: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD
            .decode(text)
            .expect("the vector text is base64url")
    }

    /// A fresh client key pair as the subscription stores it plus the
    /// private scalar the test decrypts with: `(p256dh, auth, private)`,
    /// all URL-safe base64.
    fn client_keys() -> (String, String, String) {
        let secret = SecretKey::try_generate().expect("a fresh P-256 client key");
        let mut auth = [0_u8; AUTH_SECRET_LEN];
        rand::rngs::OsRng.fill_bytes(&mut auth);
        (
            URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_point(false).as_bytes()),
            URL_SAFE_NO_PAD.encode(auth),
            URL_SAFE_NO_PAD.encode(secret.to_bytes()),
        )
    }

    /// A stored subscription row aimed at `endpoint`, keys as given.
    fn subscription(endpoint: &str, p256dh: &str, auth: &str) -> WebPushSubscription {
        WebPushSubscription {
            id: Uuid::now_v7(),
            user_id: Some(Uuid::now_v7()),
            token_id: Uuid::now_v7(),
            endpoint: endpoint.to_owned(),
            key_p256dh: p256dh.to_owned(),
            key_auth: auth.to_owned(),
            alerts: json!({}),
            policy: DEFAULT_POLICY.to_owned(),
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// A local mini push service answering every `POST /push` with
    /// `status`, on a fresh loopback port. Returns its URL and the handle
    /// task to abort when done.
    async fn mock_push_service(status: u16) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port to serve on");
        let url = format!(
            "http://{}/push",
            listener.local_addr().expect("the bound address")
        );
        let app = Router::new().route(
            "/push",
            post(move || async move {
                StatusCode::from_u16(status).expect("the test statuses are representable")
            }),
        );
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("test push service stopped: {err}");
            }
        });
        (url, server)
    }

    /// [`send`]s one fixed payload to `subscription` over the test client.
    async fn send_once(subscription: &WebPushSubscription) -> Result<(), PushError> {
        send_once_body(subscription, r#"{"title":"Alice followed you"}"#).await
    }

    #[test]
    fn the_rfc_8291_vector_encrypts_key_for_key_into_the_body() {
        let salt: [u8; SALT_LEN] = b64(RFC_SALT_B64)
            .try_into()
            .expect("the vector salt is 16 octets");
        let secret = SecretKey::from_slice(&b64(RFC_AS_PRIVATE_B64)).expect("the scalar parses");
        let ua_public = b64(RFC_UA_PUBLIC_B64);
        let auth = b64(RFC_AUTH_B64);
        let client_public =
            p256::PublicKey::from_sec1_bytes(&ua_public).expect("the vector point is on the curve");
        let as_point = secret.public_key().to_sec1_point(false);
        assert_eq!(
            URL_SAFE_NO_PAD.encode(as_point.as_bytes()),
            RFC_AS_PUBLIC_B64,
            "the vector's private key carries its public point"
        );
        let shared =
            p256::ecdh::diffie_hellman(&secret.to_nonzero_scalar(), client_public.as_affine());
        assert_eq!(
            URL_SAFE_NO_PAD.encode(shared.raw_secret_bytes()),
            RFC_ECDH_SECRET_B64,
            "ECDH matches the RFC's ecdh_secret"
        );
        let key_info = combined_key_info(&ua_public, as_point.as_bytes());
        let mut ikm = [0_u8; IKM_LEN];
        hkdf_sha256(&auth, shared.raw_secret_bytes(), &key_info, &mut ikm)
            .expect("the vector combine runs");
        assert_eq!(
            URL_SAFE_NO_PAD.encode(ikm),
            RFC_IKM_B64,
            "the input keying material matches the RFC's IKM"
        );
        let mut cek = [0_u8; CEK_LEN];
        hkdf_sha256(&salt, &ikm, CEK_INFO, &mut cek).expect("the CEK derive runs");
        assert_eq!(
            URL_SAFE_NO_PAD.encode(cek),
            RFC_CEK_B64,
            "the content key matches the RFC's CEK"
        );
        let mut nonce = [0_u8; NONCE_LEN];
        hkdf_sha256(&salt, &ikm, NONCE_INFO, &mut nonce).expect("the nonce derive runs");
        assert_eq!(
            URL_SAFE_NO_PAD.encode(nonce),
            RFC_NONCE_B64,
            "the nonce matches the RFC's NONCE"
        );
        let body = encrypt_record(
            RFC_PLAINTEXT,
            RFC_UA_PUBLIC_B64,
            RFC_AUTH_B64,
            &secret,
            &salt,
        )
        .expect("the vector encrypts");
        assert_eq!(
            URL_SAFE_NO_PAD.encode(&body),
            RFC_BODY_B64,
            "the whole body matches the RFC's §5 example message"
        );
    }

    #[test]
    fn the_rfc_8291_vector_decrypts_back_to_the_plaintext() {
        let body = b64(RFC_BODY_B64);
        let plaintext =
            decrypt_payload(&body, RFC_UA_PRIVATE_B64, RFC_AUTH_B64).expect("the vector decrypts");
        assert_eq!(plaintext, RFC_PLAINTEXT, "the client reads the §5 example");
    }

    #[test]
    fn encryption_round_trips_random_keys_and_builds_the_header() {
        let (p256dh, auth, private) = client_keys();
        let plaintext = br#"{"title":"Alice followed you","body":""}"#;
        let body = encrypt_payload(plaintext, &p256dh, &auth).expect("a fresh key pair encrypts");
        assert_eq!(
            body.get(SALT_LEN..SALT_LEN + 4),
            Some(RECORD_SIZE_BE.as_slice()),
            "the header carries rs 4096 big-endian"
        );
        assert_eq!(
            body.get(SALT_LEN + 4),
            Some(&KEY_ID_LEN),
            "the keyid length octet announces the 65-byte point"
        );
        assert_eq!(
            decrypt_payload(&body, &private, &auth).expect("the client reads it back"),
            plaintext,
            "encrypt-then-decrypt is the identity"
        );
    }

    #[test]
    fn two_encryptions_of_one_payload_never_share_a_body() {
        let (p256dh, auth, _private) = client_keys();
        let first = encrypt_payload(b"same", &p256dh, &auth).expect("the first encryption runs");
        let second = encrypt_payload(b"same", &p256dh, &auth).expect("the second encryption runs");
        assert_ne!(
            first, second,
            "the salt and ephemeral key are fresh per send"
        );
    }

    #[test]
    fn keys_that_are_not_usable_are_unprocessable() {
        for bad_p256dh in ["not base64!!", "AAAAAAAA"] {
            let err = encrypt_payload(b"x", bad_p256dh, RFC_AUTH_B64)
                .expect_err("a broken client key must not encrypt");
            assert!(
                matches!(err, AppError::Unprocessable(_)),
                "a broken p256dh is the subscription's fault, got {err}"
            );
        }
        let err = encrypt_payload(b"x", RFC_UA_PUBLIC_B64, "c2hvcnQ=")
            .expect_err("a 5-octet auth secret must not encrypt");
        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "the auth secret length is fixed by the RFC, got {err}"
        );
    }

    #[test]
    fn a_payload_over_the_record_cap_is_too_large() {
        let (p256dh, auth, _private) = client_keys();
        let ok = vec![b'x'; MAX_PLAINTEXT_LEN];
        assert!(
            encrypt_payload(&ok, &p256dh, &auth).is_ok(),
            "rs minus delimiter and tag still fits"
        );
        let too_long = vec![b'x'; MAX_PLAINTEXT_LEN + 1];
        let err = encrypt_payload(&too_long, &p256dh, &auth)
            .expect_err("one byte past the cap cannot ride a single record");
        assert!(
            matches!(err, AppError::PayloadTooLarge(_)),
            "over-long payloads name their cap, got {err}"
        );
    }

    #[test]
    fn vapid_signs_an_es256_jwt_for_the_endpoint_origin() {
        use p256::ecdsa::{VerifyingKey, signature::Verifier as _};

        let pair = vapid::generate_pair_for_tests().expect("a test VAPID pair");
        let (authorization, expires_at) = vapid_headers(
            "https://push.example.net/push/JzLQ3raZJf?token=1",
            pair.private_pem(),
            pair.public_key(),
            "mailto:admin@example.org",
        )
        .expect("the header signs");
        let (token, key) = authorization
            .strip_prefix("vapid t=")
            .and_then(|rest| rest.split_once(", k="))
            .expect("the header is the vapid scheme with both parameters");
        assert_eq!(key, pair.public_key(), "k names the instance public key");
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "header.claims.signature");
        let jwt_header: Value =
            serde_json::from_slice(&b64(parts[0])).expect("the JWT header is base64url JSON");
        assert_eq!(jwt_header["alg"], "ES256", "the VAPID algorithm");
        assert_eq!(jwt_header["typ"], "JWT", "the token type");
        let claims: Value =
            serde_json::from_slice(&b64(parts[1])).expect("the JWT claims are base64url JSON");
        assert_eq!(
            claims["aud"], "https://push.example.net",
            "aud is the origin, without path or query"
        );
        assert_eq!(
            claims["sub"], "mailto:admin@example.org",
            "sub passes through"
        );
        let expected = (Utc::now() + TimeDelta::hours(12)).timestamp();
        let exp = claims["exp"].as_i64().expect("exp is a number");
        assert!(
            (exp - expected).abs() <= 60,
            "exp is twelve hours out: {exp} vs {expected}"
        );
        assert_eq!(
            exp,
            expires_at.timestamp(),
            "the reported expiry is the signed `exp`"
        );
        let signature_bytes = b64(parts[2]);
        assert_eq!(
            signature_bytes.len(),
            64,
            "the JWT signature is the fixed r||s form"
        );
        let verifying =
            VerifyingKey::from_sec1_bytes(&b64(pair.public_key())).expect("the public key parses");
        let signature = Signature::from_slice(&signature_bytes).expect("the signature parses");
        verifying
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("the signature verifies against the instance public key");
        assert!(
            verifying.verify(b"tampered", &signature).is_err(),
            "a JWT over other bytes must not verify with this key"
        );
    }

    #[test]
    fn a_vapid_endpoint_keeps_a_non_default_port_and_refuses_junk() {
        let pair = vapid::generate_pair_for_tests().expect("a test VAPID pair");
        let (authorization, _expires) = vapid_headers(
            "https://push.example:8443/p",
            pair.private_pem(),
            pair.public_key(),
            "https://wired.example",
        )
        .expect("a non-default port signs fine");
        let claims_text = authorization
            .strip_prefix("vapid t=")
            .and_then(|rest| rest.split_once(','))
            .map(|(token, _rest)| token.split('.').nth(1).unwrap_or_default().to_owned())
            .expect("the header carries claims");
        let claims: Value = serde_json::from_slice(&b64(&claims_text)).expect("the claims decode");
        assert_eq!(
            claims["aud"], "https://push.example:8443",
            "the port is signed"
        );
        assert_eq!(
            claims["sub"], "https://wired.example",
            "the https subject form is fine too"
        );
        assert!(
            endpoint_origin("ftp://push.example/p").is_err(),
            "a non-http scheme has no signable origin"
        );
        assert!(
            endpoint_origin("not a url").is_err(),
            "junk is refused before anything is signed"
        );
    }

    #[test]
    fn an_unparseable_private_key_fails_the_signing_without_leaking() {
        let err = vapid_headers(
            "https://push.example/p",
            "-----BEGIN PRIVATE KEY-----\nQUJD\n-----END PRIVATE KEY-----",
            "public",
            "mailto:a@b.c",
        )
        .expect_err("junk key material cannot sign");
        assert!(
            matches!(err, AppError::Internal(_)),
            "broken stored secrets are a server failure, got {err}"
        );
    }

    #[test]
    fn the_subject_prefers_the_contact_email() {
        let with_email = Config::from_lookup(|key| match key {
            DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
            BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
            CONTACT_EMAIL_VAR => Some("admin@wired.example".to_owned()),
            _ => None,
        })
        .expect("the fixed config parses");
        assert_eq!(
            vapid_subject(&with_email),
            "mailto:admin@wired.example",
            "CONTACT_EMAIL is the mailto subject"
        );
        let without = Config::for_tests();
        assert_eq!(
            vapid_subject(&without),
            TEST_BASE_URL,
            "with no contact configured the instance origin stands in"
        );
    }

    #[test]
    fn alert_flags_read_the_stored_object_with_a_default_on() {
        let alerts = json!({"follow": true, "favourite": false, "mention": "yes"});
        assert!(alert_enabled(&alerts, "follow"), "explicitly on");
        assert!(!alert_enabled(&alerts, "favourite"), "explicitly off");
        assert!(
            alert_enabled(&alerts, "reblog"),
            "a type the object does not name is on"
        );
        assert!(
            alert_enabled(&alerts, "mention"),
            "a non-boolean flag is not an off switch"
        );
        assert!(
            alert_enabled(&Value::Null, "follow"),
            "a null object alerts all"
        );
    }

    #[test]
    fn every_wakeable_type_gets_its_verb_and_the_fallback_covers_the_rest() {
        assert_eq!(verb_for(notification::TYPE_FOLLOW), "followed you");
        assert_eq!(
            verb_for(notification::TYPE_FOLLOW_REQUEST),
            "requested to follow you"
        );
        assert_eq!(
            verb_for(notification::TYPE_FAVOURITE),
            "favourited your post"
        );
        assert_eq!(verb_for(notification::TYPE_REBLOG), "boosted your post");
        assert_eq!(verb_for(notification::TYPE_MENTION), "mentioned you");
        assert_eq!(
            verb_for(notification::TYPE_EMOJI_REACTION),
            "reacted to your post"
        );
        assert_eq!(verb_for("status"), "posted", "a followed account posted");
        assert_eq!(verb_for("update"), "edited a post");
        assert_eq!(verb_for("poll"), "ended a poll");
        assert_eq!(verb_for("pleroma:chat_mention"), "sent you a chat message");
        assert_eq!(verb_for("admin.sign_up"), "signed up");
        assert_eq!(verb_for("admin.report"), "filed a report");
        assert_eq!(verb_for("mystery.type"), DEFAULT_VERB, "the fallback");
    }

    #[test]
    fn the_payload_document_carries_the_mastodon_fields() {
        let parts = PushPayload {
            access_token: "tok3n",
            notification_id: Uuid::now_v7(),
            notification_type: notification::TYPE_FAVOURITE,
            icon: "https://wired.example/avatar.png",
            actor_display_name: "Alice",
            body: "the liked post's plain text",
        };
        let text = payload_json(&parts);
        let value: Value = serde_json::from_str(&text).expect("the payload is JSON");
        assert_eq!(
            value["access_token"], "tok3n",
            "the client wakes with its token"
        );
        assert_eq!(
            value["notification_id"],
            parts.notification_id.to_string(),
            "the id round-trips as decision 4's hyphenated text"
        );
        assert_eq!(value["notification_type"], "favourite");
        assert_eq!(value["icon"], "https://wired.example/avatar.png");
        assert_eq!(value["preferred_locale"], "en");
        assert_eq!(value["title"], "Alice favourited your post", "name + verb");
        assert_eq!(value["body"], "the liked post's plain text");
        let hidden = payload_json(&PushPayload {
            body: "",
            ..parts.clone()
        });
        let hidden: Value = serde_json::from_str(&hidden).expect("the hidden payload is JSON");
        assert_eq!(hidden["body"], "", "a hidden-content wake carries no body");
    }

    #[tokio::test]
    async fn a_2xx_answer_delivers() {
        for status in [200_u16, 201, 204, 299] {
            let (url, server) = mock_push_service(status).await;
            let (p256dh, auth, _private) = client_keys();
            assert_eq!(
                send_once(&subscription(&url, &p256dh, &auth)).await,
                Ok(()),
                "status {status} is a success"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn a_404_or_410_reports_the_subscription_gone() {
        for status in [404_u16, 410] {
            let (url, server) = mock_push_service(status).await;
            let (p256dh, auth, _private) = client_keys();
            assert_eq!(
                send_once(&subscription(&url, &p256dh, &auth)).await,
                Err(PushError::Gone),
                "status {status} retires the subscription"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn throttling_server_errors_and_other_rejections_retry() {
        for status in [400_u16, 413, 429, 500, 502, 503] {
            let (url, server) = mock_push_service(status).await;
            let (p256dh, auth, _private) = client_keys();
            match send_once(&subscription(&url, &p256dh, &auth)).await {
                Err(PushError::Retryable(detail)) => assert!(
                    detail.contains(&status.to_string()),
                    "the detail names the status: {detail}"
                ),
                other => panic!("status {status} must be retryable, got {other:?}"),
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn an_unreachable_service_is_retryable_not_gone() {
        let (p256dh, auth, _private) = client_keys();
        match send_once(&subscription("http://127.0.0.1:1/push", &p256dh, &auth)).await {
            Err(PushError::Retryable(_)) => {}
            other => panic!("a dead peer must retry, got {other:?}"),
        }
    }

    /// The five headers a push POST must carry, as the push service sees
    /// them.
    const PUSH_HEADERS: [&str; 5] = [
        "authorization",
        "content-encoding",
        "content-type",
        "ttl",
        "urgency",
    ];

    /// The recording mock's state: the five headers and the body of the
    /// request it served.
    #[derive(Clone, Default)]
    struct Recorder {
        /// `name: value` for each [`PUSH_HEADERS`] entry.
        header_lines: Arc<Mutex<Vec<String>>>,
        /// The request body.
        body: Arc<Mutex<Vec<u8>>>,
    }

    /// The recording handler: store the five push headers and the body,
    /// answer `Created`.
    async fn record_push(
        axum::extract::State(rec): axum::extract::State<Recorder>,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        for name in PUSH_HEADERS {
            let value = headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("<missing>")
                .to_owned();
            rec.header_lines
                .lock()
                .expect("an unpoisoned lock")
                .push(format!("{name}: {value}"));
        }
        *rec.body.lock().expect("an unpoisoned lock") = body.to_vec();
        StatusCode::CREATED
    }

    /// Serves `app` on `listener` until the test aborts the task.
    async fn serve_mock(listener: TcpListener, app: Router) {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("test push service stopped: {err}");
        }
    }

    #[tokio::test]
    async fn the_request_carries_the_five_push_headers_and_a_reading_coding() {
        let (p256dh, auth, private) = client_keys();
        let rec = Recorder::default();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port to serve on");
        let url = format!(
            "http://{}/push",
            listener.local_addr().expect("the bound address")
        );
        let app = Router::new()
            .route("/push", post(record_push))
            .with_state(rec.clone());
        let server = tokio::spawn(serve_mock(listener, app));
        let payload = r#"{"title":"Alice followed you","body":""}"#;
        send_once_body(&subscription(&url, &p256dh, &auth), payload)
            .await
            .expect("the mock accepts");
        server.abort();

        let seen = rec.header_lines.lock().expect("an unpoisoned lock").clone();
        for expected in [
            "authorization: vapid t=jwt, k=key".to_owned(),
            "content-encoding: aes128gcm".to_owned(),
            "content-type: application/octet-stream".to_owned(),
            format!("ttl: {TTL_HEADER}"),
            format!("urgency: {URGENCY_HEADER}"),
        ] {
            assert!(
                seen.contains(&expected),
                "the header `{expected}` is sent, saw {seen:?}"
            );
        }
        let body = rec.body.lock().expect("an unpoisoned lock").clone();
        let decrypted =
            decrypt_payload(&body, &private, &auth).expect("the body is a valid record");
        assert_eq!(
            String::from_utf8(decrypted).expect("the payload is utf-8"),
            payload,
            "the service cannot read the body but the client can"
        );
    }

    /// [`send_once`] with a caller-chosen payload.
    async fn send_once_body(
        subscription: &WebPushSubscription,
        payload: &str,
    ) -> Result<(), PushError> {
        send(
            &HttpClient::new_for_tests("Wired (https://wired.test)"),
            subscription,
            payload,
            "vapid t=jwt, k=key",
        )
        .await
    }

    #[tokio::test]
    async fn unusable_stored_keys_retire_rather_than_retry() {
        let broken = subscription("https://push.invalid/x", "junk-not-base64!!", "also junk!!");
        assert_eq!(
            send_once(&broken).await,
            Err(PushError::Gone),
            "bad keys are a dead subscription, whatever the endpoint"
        );
    }
}
