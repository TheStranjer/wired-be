//! Random tokens: the OAuth credentials and the webhook signing secrets.
//!
//! Pleroma mints an app's `client_id` and `client_secret` with
//! `:crypto.strong_rand_bytes(32) |> Base.url_encode64(padding: false)`
//! (`lib/pleroma/web/o_auth/app.ex`): 32 bytes from the OS CSPRNG,
//! base64url-encoded without padding, which is exactly 43 URL-safe
//! characters. [`random_token`] reproduces that recipe so clients that
//! assume the shape keep working. [`random_hex`] is the same CSPRNG drawn
//! into the lowercase-hex spelling webhook secrets take.
//!
//! Tokens are secret material: callers pass them straight to storage or a
//! response body and never format them into logs or messages.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore as _;
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;

/// Bytes behind the 43-character credentials Pleroma and Mastodon issue
/// (`crypto.strong_rand_bytes(32)`; 32 bytes are 43 base64url characters).
pub const CREDENTIAL_BYTES: usize = 32;

/// `bytes` random bytes from the OS CSPRNG, rendered as lowercase hex:
/// two characters per byte.
///
/// This is the webhook signing secret's recipe —
/// [`crate::models::webhook`] mints 32 bytes this way (Pleroma's
/// `generate_secret` in `lib/pleroma/webhook.ex` hex-encodes random bytes
/// too; the ticket fixes the width).
///
/// # Panics
///
/// If the OS CSPRNG fails to deliver `bytes` random bytes, which on a
/// supported platform it does not.
#[must_use]
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0_u8; bytes];
    // `fill_bytes` on the OS CSPRNG of this platform cannot fail; rand
    // aborts internally if the kernel ever refuses to deliver entropy.
    rand::rngs::OsRng.fill_bytes(&mut buf);
    to_hex(&buf)
}

/// `bytes` as lowercase hex, two characters per byte: the spelling random
/// secrets are stored in and the one an HMAC digest is announced in (see
/// [`crate::services::webhooks`]'s signed deliveries).
#[must_use]
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(
        String::with_capacity(bytes.len().saturating_mul(2)),
        |mut hex, byte| {
            let written: std::fmt::Result = write!(hex, "{byte:02x}");
            debug_assert!(written.is_ok(), "a String buffer never fails to grow");
            hex
        },
    )
}

/// The lowercase hex `SHA-256` of `text`: 64 characters.
///
/// Mastodon's domain-block digest (Pleroma writes it as
/// `:crypto.hash(:sha256, host)` with `Base.encode16(case: :lower)`), the
/// stored form of two-factor backup codes, and what email-confirmation
/// tokens derive from.
#[must_use]
pub fn sha256_hex(text: &str) -> String {
    sha256_hex_bytes(text.as_bytes())
}

/// The lowercase hex `SHA-256` of `bytes`: [`sha256_hex`] for data that
/// is not text, such as a downloaded emoji pack archive.
#[must_use]
pub fn sha256_hex_bytes(bytes: &[u8]) -> String {
    to_hex(&Sha256::digest(bytes))
}

/// The at-rest form of a high-entropy random credential: the lowercase
/// hex `SHA-256` of `token`, 64 characters.
///
/// Password-reset and MFA challenge tokens are 32 CSPRNG bytes
/// ([`random_token`] over [`CREDENTIAL_BYTES`]), so there is nothing to
/// brute-force and no need for a salt or a slow hash: the digest only has
/// to make a leaked row useless as a credential. Storage keeps this
/// digest and every lookup binds the digest of the submitted text.
#[must_use]
pub fn token_digest(token: &str) -> String {
    sha256_hex(token)
}

/// `bytes` random bytes from the OS CSPRNG, base64url-encoded with no
/// padding.
///
/// Only `A–Z`, `a–z`, `0–9`, `-` and `_` appear in the output, so it is
/// safe in URLs and headers verbatim. `random_token(CREDENTIAL_BYTES)`
/// yields the 43-character form OAuth credentials are stored in.
///
/// # Panics
///
/// If the OS CSPRNG fails to deliver `bytes` random bytes, which on a
/// supported platform it does not.
#[must_use]
pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0_u8; bytes];
    // `fill_bytes` on the OS CSPRNG of this platform cannot fail; rand
    // aborts internally if the kernel ever refuses to deliver entropy.
    rand::rngs::OsRng.fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether every character is in the URL-safe base64 alphabet.
    fn is_url_safe(token: &str) -> bool {
        token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }

    #[test]
    fn credential_tokens_are_43_url_safe_characters() {
        let token = random_token(CREDENTIAL_BYTES);
        assert_eq!(token.len(), 43, "32 bytes encode to 43 characters");
        assert!(is_url_safe(&token), "`{token}` uses only URL-safe bytes");
        assert!(!token.contains('='), "no padding");
    }

    #[test]
    fn tokens_are_unique_across_calls() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(
                seen.insert(random_token(CREDENTIAL_BYTES)),
                "the CSPRNG must not repeat a credential"
            );
        }
    }

    #[test]
    fn length_scales_with_the_byte_count() {
        // 3 bytes are 4 characters, so other sizes round up to the next
        // base64 group without padding.
        assert_eq!(random_token(0).len(), 0, "nothing in, nothing out");
        assert_eq!(random_token(3).len(), 4, "3 bytes are 4 characters");
        assert_eq!(random_token(16).len(), 22, "16 bytes round up to 22");
        assert_eq!(random_token(20).len(), 27, "20 bytes round up to 27");
    }

    #[test]
    fn hex_secrets_are_two_lowercase_characters_per_byte() {
        let secret = random_hex(32);
        assert_eq!(secret.len(), 64, "32 bytes are 64 hex characters");
        assert!(
            secret
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "`{secret}` is lowercase hex"
        );
        assert_eq!(random_hex(0), "", "nothing in, nothing out");
    }

    #[test]
    fn the_sha256_hex_digest_is_lowercase_hex() {
        // The published SHA-256 of the ASCII string "abc".
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "the digest matches the SHA-256 test vector"
        );
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "even the empty string hashes"
        );
        let digest = sha256_hex("Wired");
        assert_eq!(digest.len(), 64, "SHA-256 is 32 bytes, 64 hex characters");
        assert!(
            digest
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "`{digest}` is lowercase hex"
        );
    }

    #[test]
    fn token_digest_is_64_lowercase_hex_and_stable() {
        let digest = token_digest("abc");
        assert_eq!(
            digest, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "the digest of \"abc\" is the SHA-256 test vector"
        );
        assert_eq!(token_digest("abc"), digest, "the digest is stable");
        assert_eq!(digest.len(), 64, "SHA-256 is 64 hex characters");
        assert!(
            digest
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "`{digest}` is lowercase hex"
        );
        assert_ne!(token_digest("abd"), digest, "another token, another digest");
    }

    #[test]
    fn hex_secrets_are_unique_across_calls() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(
                seen.insert(random_hex(32)),
                "the CSPRNG must not repeat a secret"
            );
        }
    }
}
