//! PKCE (RFC 7636) for the authorization-code flow.
//!
//! It holds the `S256` challenge a code is bound to at `/oauth/authorize`
//! and the check the `authorization_code` grant runs against the
//! `code_verifier` it is redeemed with. Only `S256` is spoken; `plain` is
//! refused, as Mastodon's `pkce_code_challenge_methods ['S256']` does.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

use crate::error::{AppError, AppResult};

/// The one `code_challenge_method` this server accepts.
pub const METHOD_S256: &str = "S256";
/// Shortest `code_challenge` / `code_verifier` (RFC 7636 §4.1, §4.2).
const MIN_LEN: usize = 43;
/// Longest `code_challenge` / `code_verifier` (RFC 7636 §4.1, §4.2).
const MAX_LEN: usize = 128;
/// 400 body for a `code_challenge` that is not 43–128 base64url characters.
pub const INVALID_CODE_CHALLENGE: &str = "Invalid code_challenge";
/// 400 body for a `code_challenge_method` other than [`METHOD_S256`], or a
/// challenge sent without one.
pub const UNSUPPORTED_CODE_CHALLENGE_METHOD: &str = "Unsupported code_challenge_method";
/// 400 body for a `code_challenge_method` sent without a challenge.
pub const MISSING_CODE_CHALLENGE: &str = "Missing parameter: code_challenge";

/// `BASE64URL-ENCODE(SHA256(ASCII(verifier)))` without padding
/// (RFC 7636 §4.2).
#[must_use]
pub fn s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Whether `text` is 43–128 characters of `allowed`.
fn well_formed(text: &str, allowed: fn(char) -> bool) -> bool {
    (MIN_LEN..=MAX_LEN).contains(&text.len()) && text.chars().all(allowed)
}

/// The base64url alphabet of a challenge.
const fn is_base64url(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// The unreserved characters of a verifier (RFC 7636 §4.1).
const fn is_unreserved(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')
}

/// Validates the PKCE parameters of an authorize request and returns the
/// challenge to bind the code to, `None` when the request carries none.
///
/// # Errors
///
/// 400 [`UNSUPPORTED_CODE_CHALLENGE_METHOD`] for a method other than
/// `S256` (or a challenge without one), [`MISSING_CODE_CHALLENGE`] for a
/// method without a challenge, and [`INVALID_CODE_CHALLENGE`] for a
/// challenge that is not 43–128 base64url characters.
pub fn parse_challenge(challenge: Option<&str>, method: Option<&str>) -> AppResult<Option<String>> {
    if method.is_some_and(|given| given != METHOD_S256) {
        return Err(AppError::BadRequest(
            UNSUPPORTED_CODE_CHALLENGE_METHOD.to_owned(),
        ));
    }
    match (challenge, method) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(AppError::BadRequest(MISSING_CODE_CHALLENGE.to_owned())),
        (Some(_), None) => Err(AppError::BadRequest(
            UNSUPPORTED_CODE_CHALLENGE_METHOD.to_owned(),
        )),
        (Some(text), Some(_)) if well_formed(text, is_base64url) => Ok(Some(text.to_owned())),
        (Some(_), Some(_)) => Err(AppError::BadRequest(INVALID_CODE_CHALLENGE.to_owned())),
    }
}

/// Whether a code bound to `stored` may be redeemed with `verifier`.
///
/// Always when it is bound to no challenge, otherwise only when the
/// verifier is well formed and its [`s256`] equals the challenge
/// (compared in constant time).
#[must_use]
pub fn verifies(stored: Option<&str>, verifier: Option<&str>) -> bool {
    let Some(challenge) = stored else {
        return true;
    };
    let Some(presented) = verifier.filter(|text| well_formed(text, is_unreserved)) else {
        return false;
    };
    s256(presented)
        .as_bytes()
        .ct_eq(challenge.as_bytes())
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RFC 7636 Appendix B verifier.
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    /// Its S256 challenge.
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    /// A `BadRequest` message, or a panic.
    fn message(result: AppResult<Option<String>>) -> String {
        result.expect_err("a refusal").to_string()
    }

    #[test]
    fn s256_matches_the_rfc_7636_appendix_b_vector() {
        assert_eq!(s256(VERIFIER), CHALLENGE, "Appendix B");
    }

    #[test]
    fn a_request_without_pkce_binds_nothing() {
        assert_eq!(parse_challenge(None, None).unwrap(), None, "no PKCE");
    }

    #[test]
    fn an_s256_challenge_is_accepted() {
        assert_eq!(
            parse_challenge(Some(CHALLENGE), Some("S256")).unwrap(),
            Some(CHALLENGE.to_owned()),
            "the Appendix B challenge"
        );
        let longest = "a".repeat(128);
        assert!(parse_challenge(Some(&longest), Some("S256")).is_ok(), "128");
    }

    #[test]
    fn other_methods_and_a_missing_method_are_refused() {
        for method in [Some("plain"), Some("s256"), Some(""), None] {
            assert_eq!(
                message(parse_challenge(Some(CHALLENGE), method)),
                UNSUPPORTED_CODE_CHALLENGE_METHOD,
                "a challenge with any method but S256 is refused"
            );
        }
        assert_eq!(
            message(parse_challenge(None, Some("plain"))),
            UNSUPPORTED_CODE_CHALLENGE_METHOD,
            "plain is refused with or without a challenge"
        );
    }

    #[test]
    fn a_method_without_a_challenge_is_refused() {
        assert_eq!(
            message(parse_challenge(None, Some("S256"))),
            MISSING_CODE_CHALLENGE,
            "nothing to bind"
        );
    }

    #[test]
    fn malformed_challenges_are_refused() {
        let too_short = "a".repeat(42);
        let too_long = "a".repeat(129);
        let padded = format!("{}=", "a".repeat(43));
        let plus = format!("{}+", "a".repeat(42));
        for bad in ["", too_short.as_str(), &too_long, &padded, &plus] {
            assert_eq!(
                message(parse_challenge(Some(bad), Some("S256"))),
                INVALID_CODE_CHALLENGE,
                "`{bad}` is not 43-128 base64url characters"
            );
        }
    }

    #[test]
    fn an_unbound_code_accepts_any_verifier_or_none() {
        assert!(verifies(None, None), "no verifier");
        assert!(verifies(None, Some("whatever")), "an ignored verifier");
    }

    #[test]
    fn a_bound_code_needs_the_matching_verifier() {
        assert!(verifies(Some(CHALLENGE), Some(VERIFIER)), "the right one");
        assert!(!verifies(Some(CHALLENGE), None), "missing");
        assert!(!verifies(Some(CHALLENGE), Some("")), "empty");
        assert!(!verifies(Some(CHALLENGE), Some(CHALLENGE)), "the challenge");
        let other = "b".repeat(43);
        assert!(!verifies(Some(CHALLENGE), Some(&other)), "another verifier");
        let spaced = format!("{VERIFIER} ");
        assert!(!verifies(Some(CHALLENGE), Some(&spaced)), "malformed");
    }
}
