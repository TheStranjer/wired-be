//! Random tokens for OAuth credentials.
//!
//! Pleroma mints an app's `client_id` and `client_secret` with
//! `:crypto.strong_rand_bytes(32) |> Base.url_encode64(padding: false)`
//! (`lib/pleroma/web/o_auth/app.ex`): 32 bytes from the OS CSPRNG,
//! base64url-encoded without padding, which is exactly 43 URL-safe
//! characters. [`random_token`] reproduces that recipe so clients that
//! assume the shape keep working.
//!
//! Tokens are secret material: callers pass them straight to storage or a
//! response body and never format them into logs or messages.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore as _;

/// Bytes behind the 43-character credentials Pleroma and Mastodon issue
/// (`crypto.strong_rand_bytes(32)`; 32 bytes are 43 base64url characters).
pub const CREDENTIAL_BYTES: usize = 32;

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
}
