//! The receiver's half of the `aes128gcm` content coding: [`decrypt_payload`]
//! reads a push body back to its plaintext with the client's private key.
//! These exist for tests and verification only, compiled under the same
//! `cfg(any(test, feature = "integration-tests"))` gate as the functions
//! themselves.

use aes_gcm::{
    Aes128Gcm, KeyInit as _,
    aead::{Aead as _, Nonce},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use p256::{SecretKey, elliptic_curve::sec1::ToSec1Point as _};

use super::crypto::{PADDING_DELIMITER, SALT_LEN, decode_auth_secret, derive_context};
use crate::error::{AppError, AppResult};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::web_push::test_support::*;

    #[test]
    fn the_rfc_8291_vector_decrypts_back_to_the_plaintext() {
        let body = b64(RFC_BODY_B64);
        let plaintext =
            decrypt_payload(&body, RFC_UA_PRIVATE_B64, RFC_AUTH_B64).expect("the vector decrypts");
        assert_eq!(plaintext, RFC_PLAINTEXT, "the client reads the §5 example");
    }
}
