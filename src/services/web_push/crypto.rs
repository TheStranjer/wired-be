//! The RFC 8291 `aes128gcm` content coding: the header constants, the
//! key derivation ([`derive_context`]) and the one-record seal
//! ([`seal_record`]) that [`encrypt_payload`] assembles.

use aes_gcm::{
    Aes128Gcm, KeyInit as _,
    aead::{Aead as _, Nonce},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hkdf::Hkdf;
use p256::{
    SecretKey,
    elliptic_curve::{Generate as _, sec1::ToSec1Point as _},
};
use rand::RngCore as _;
use sha2_011::Sha256;

use crate::error::{AppError, AppResult};

/// RFC 8291 §3.4: the `aes128gcm` header's random salt length, in octets.
pub(super) const SALT_LEN: usize = 16;
/// RFC 8291 §3.2: the authentication secret's length, in octets.
pub(super) const AUTH_SECRET_LEN: usize = 16;
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
pub(super) const PADDING_DELIMITER: u8 = 0x02;
/// RFC 8291 §3.3: the label that opens the key-combining `info`.
const KEY_INFO_LABEL: &[u8] = b"WebPush: info";
/// RFC 8291 §3.4: the content encryption key's `info`.
const CEK_INFO: &[u8] = b"Content-Encoding: aes128gcm\0";
/// RFC 8291 §3.4: the nonce's `info`.
const NONCE_INFO: &[u8] = b"Content-Encoding: nonce\0";

/// The AES key and nonce one `aes128gcm` record is sealed with, derived per
/// RFC 8291 §3.4 from the ECDH secret, the auth secret and the header salt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CryptoContext {
    /// The 16-octet content encryption key.
    pub(super) cek: [u8; CEK_LEN],
    /// The 12-octet AES-GCM nonce.
    pub(super) nonce: [u8; NONCE_LEN],
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
pub(super) fn derive_context(
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
pub(super) fn decode_auth_secret(auth_b64url: &str) -> AppResult<[u8; AUTH_SECRET_LEN]> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::web_push::{decrypt_payload, test_support::*};

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
}
