//! HMAC-SHA256, the one place the crate keys and finalizes it.
//!
//! The media proxy's link signatures, the webhook delivery signatures and the
//! mailer unsubscribe tokens all sign with HMAC-SHA256; each spells its
//! output (base64url, `sha256=` plus hex) at its own call site and takes the
//! raw digest from [`hmac_sha256`]. [`verify_hmac_sha256`] is the
//! constant-time check of a presented tag.

use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2_011::Sha256;
use subtle::ConstantTimeEq as _;

/// The HMAC flavor this module computes: HMAC-SHA256.
type HmacSha256 = Hmac<Sha256>;

/// The HMAC-SHA256 of `message` keyed by `key`, as the raw 32-byte digest.
///
/// `None` only for key material the HMAC constructor refuses, which HMAC
/// cannot do (it hashes keys of any length); callers still handle it
/// because non-test code may not `expect`.
#[must_use]
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> Option<[u8; 32]> {
    let mut mac = HmacSha256::new_from_slice(key).ok()?;
    mac.update(message);
    let digest = mac.finalize().into_bytes();
    <[u8; 32]>::try_from(digest.as_slice()).ok()
}

/// Whether `tag` is the HMAC-SHA256 of `message` keyed by `key`, compared in
/// constant time so a caller leaks nothing about how far a guess was.
///
/// A `tag` of the wrong length never verifies, and neither does any tag when
/// the digest cannot be computed.
#[must_use]
pub fn verify_hmac_sha256(key: &[u8], message: &[u8], tag: &[u8]) -> bool {
    hmac_sha256(key, message).is_some_and(|want| bool::from(want.as_slice().ct_eq(tag)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::token::to_hex;

    /// RFC 4231 test case 1: key of twenty `0x0b` bytes, data `"Hi There"`.
    const CASE_1_DIGEST: &str = "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7";
    /// RFC 4231 test case 2: key `"Jefe"`, data `"what do ya want for nothing?"`.
    const CASE_2_DIGEST: &str = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";

    #[test]
    fn rfc_4231_case_1_matches() {
        let digest = hmac_sha256(&[0x0b; 20], b"Hi There").expect("any key length keys HMAC");
        assert_eq!(to_hex(&digest), CASE_1_DIGEST, "the published digest");
    }

    #[test]
    fn rfc_4231_case_2_matches() {
        let digest = hmac_sha256(b"Jefe", b"what do ya want for nothing?")
            .expect("any key length keys HMAC");
        assert_eq!(to_hex(&digest), CASE_2_DIGEST, "the published digest");
    }

    #[test]
    fn an_empty_key_and_message_still_sign() {
        let digest = hmac_sha256(b"", b"").expect("an empty key keys HMAC");
        assert_eq!(
            to_hex(&digest),
            "b613679a0814d9ec772f95d778c35fc5ff1697c493715653c6c712144292c5ad",
            "HMAC-SHA256 of nothing under no key"
        );
    }

    #[test]
    fn a_published_digest_verifies() {
        let tag = hmac_sha256(b"Jefe", b"what do ya want for nothing?").expect("signs");
        assert!(
            verify_hmac_sha256(b"Jefe", b"what do ya want for nothing?", &tag),
            "the digest of the same key and message verifies"
        );
    }

    #[test]
    fn a_wrong_key_message_or_tag_does_not_verify() {
        let tag = hmac_sha256(b"Jefe", b"message").expect("signs");
        assert!(!verify_hmac_sha256(b"Jeff", b"message", &tag), "wrong key");
        assert!(
            !verify_hmac_sha256(b"Jefe", b"massage", &tag),
            "wrong message"
        );
        let mut flipped = tag;
        flipped[0] ^= 1;
        assert!(
            !verify_hmac_sha256(b"Jefe", b"message", &flipped),
            "a flipped bit"
        );
    }

    #[test]
    fn a_tag_of_the_wrong_length_does_not_verify() {
        let tag = hmac_sha256(b"Jefe", b"message").expect("signs");
        assert!(
            !verify_hmac_sha256(b"Jefe", b"message", &tag[..31]),
            "a truncated tag"
        );
        assert!(
            !verify_hmac_sha256(b"Jefe", b"message", &[]),
            "an empty tag"
        );
        let mut longer = tag.to_vec();
        longer.push(0);
        assert!(
            !verify_hmac_sha256(b"Jefe", b"message", &longer),
            "an over-long tag"
        );
    }
}
