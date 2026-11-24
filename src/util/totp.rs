//! Time-based one-time passwords (TOTP) and two-factor backup codes.
//!
//! These are the primitives two-factor authentication is built on, pinned to
//! what authenticator apps and Pleroma's 2FA (`lib/pleroma/user/two_factor_auth.ex`,
//! which uses `NimbleTOTP` with its defaults) accept:
//!
//! - [`generate_secret`] mints the shared secret as an uppercase base32
//!   ([`RFC 4648`]) string without padding, backed by 20 random bytes from
//!   the OS CSPRNG — the length [`RFC 6238`] recommends.
//! - [`code_at`] computes the code for a point in time: [`RFC 6238`] TOTP
//!   over [`RFC 4226`] HOTP, so HMAC-SHA1, a 30-second step, 6 digits from
//!   the dynamic truncation, zero-padded.
//! - [`verify`] accepts the current step plus one step of drift on either
//!   side, comparing candidate codes in constant time
//!   ([`subtle::ConstantTimeEq`]).
//! - [`provisioning_uri`] renders the `otpauth://totp/` URI that setup QR
//!   codes carry.
//! - [`generate_backup_codes`] mints single-use recovery codes (8 lowercase
//!   base36 characters, the shape Pleroma issues), and [`hash_backup_code`]
//!   is the hex [SHA-256] stored in their place.
//!
//! Secrets and codes are secret material: nothing here formats them into
//! error messages or logs, and callers must not either.
//!
//! [`RFC 4226`]: https://datatracker.ietf.org/doc/html/rfc4226
//! [`RFC 4648`]: https://datatracker.ietf.org/doc/html/rfc4648
//! [`RFC 6238`]: https://datatracker.ietf.org/doc/html/rfc6238
//! [SHA-256]: https://datatracker.ietf.org/doc/html/rfc6234

use base32::Alphabet;
use hmac::{Hmac, KeyInit as _, Mac as _};
use rand::{Rng as _, RngCore as _, rngs::OsRng};
use sha1::Sha1;
use subtle::{Choice, ConstantTimeEq as _};

use crate::{
    error::{AppError, AppResult},
    util::token::sha256_hex,
};

/// Length of a generated secret in bytes: 160 bits, what `RFC 6238`
/// appendix B tests with and the size every authenticator app accepts.
const SECRET_BYTES: usize = 20;
/// Seconds one TOTP step lasts; `RFC 6238`'s default, which authenticator
/// apps assume unless the provisioning URI says otherwise.
const TIME_STEP_SECONDS: i64 = 30;
/// Steps [`verify`] accepts either side of the current one, to cover clock
/// drift between server and phone.
const DRIFT_STEPS: i64 = 1;
/// Digits in a TOTP code (the displayed code length).
const CODE_DIGITS: usize = 6;
/// 10 to the power of [`CODE_DIGITS`]: the modulo of `RFC 4226`'s
/// truncation, so the value fits in exactly that many decimal digits.
///
/// [`RFC 4226`]: https://datatracker.ietf.org/doc/html/rfc4226
const CODE_MODULUS: u32 = decimal_modulus(CODE_DIGITS);
/// The dynamic-truncation window of `RFC 4226` section 5.4: four bytes
/// picked by the low nibble of the HMAC's last byte.
const TRUNCATED_WINDOW_BYTES: usize = 4;
/// Mask for the low nibble of the HMAC's last byte (the truncation offset).
const NIBBLE_MASK: u8 = 0x0F;
/// Clears the sign bit of the truncated 32-bit window (`RFC 4226`).
const SIGN_MASK: u32 = 0x7FFF_FFFF;
/// Alphabet size of a backup code: `0-9` and `a-z`.
const BACKUP_ALPHABET_SIZE: u8 = 36;
/// Characters in one backup code.
const BACKUP_CODE_LEN: usize = 8;
/// Upper-case alphabet for hex digits, used by percent-encoding.
const HEX_UPPER_BASE: u8 = b'A';

/// Base32 ([`RFC 4648`]) without padding: the spelling authenticator apps
/// display and consume secrets in.
///
/// [`RFC 4648`]: https://datatracker.ietf.org/doc/html/rfc4648
const BASE32: Alphabet = Alphabet::Rfc4648 { padding: false };
/// Base32 ([`RFC 4648`]) with `=` padding tolerated on decode, so a secret
/// copied out of an app that pads still parses; the padding is stripped
/// from the decoded result by the crate.
///
/// [`RFC 4648`]: https://datatracker.ietf.org/doc/html/rfc4648
const BASE32_DECODE: Alphabet = Alphabet::Rfc4648 { padding: true };

/// HMAC-SHA1, the MAC [`RFC 6238`] fixes for TOTP.
///
/// [`RFC 6238`]: https://datatracker.ietf.org/doc/html/rfc6238
type HmacSha1 = Hmac<Sha1>;

/// A fresh 20-byte TOTP secret, base32 ([`RFC 4648`]) without padding and
/// uppercase: 32 characters from the OS CSPRNG.
///
/// [`RFC 4648`]: https://datatracker.ietf.org/doc/html/rfc4648
#[must_use]
pub fn generate_secret() -> String {
    let mut secret = [0_u8; SECRET_BYTES];
    // `fill_bytes` on the OS CSPRNG cannot fail; rand aborts internally if
    // the kernel ever refuses to deliver entropy (as in `util::token`).
    OsRng.fill_bytes(&mut secret);
    base32::encode(BASE32, &secret)
}

/// The [`RFC 6238`] code of `secret_b32` at `unix_time`: HMAC-SHA1 over the
/// 30-second step number with `RFC 4226` dynamic truncation, 6
/// zero-padded digits.
///
/// # Errors
///
/// [`AppError::BadRequest`] if `secret_b32` is not base32 (any padding and
/// either case tolerated); the message never quotes the secret.
///
/// [`RFC 6238`]: https://datatracker.ietf.org/doc/html/rfc6238
pub fn code_at(secret_b32: &str, unix_time: i64) -> AppResult<String> {
    let secret = decode_secret(secret_b32)?;
    hotp(&secret, unix_time.div_euclid(TIME_STEP_SECONDS))
}

/// Checks a 6-digit TOTP `code` against `secret_b32` at `now_unix`,
/// accepting one step of clock drift either side of the current one.
///
/// Every comparison is constant time ([`subtle::ConstantTimeEq`]) and all
/// accepted steps are checked regardless of early matches, so the answer
/// never leaks which step matched. Non-6-digit input — the empty string,
/// the wrong length, anything with a non-digit — is rejected without even
/// touching the secret.
#[must_use]
pub fn verify(secret_b32: &str, code: &str, now_unix: i64) -> bool {
    if code.len() != CODE_DIGITS || !code.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Ok(secret) = decode_secret(secret_b32) else {
        return false;
    };
    let step = now_unix.div_euclid(TIME_STEP_SECONDS);
    let candidate = code.as_bytes();
    let mut matched = Choice::from(0_u8);
    for drift in -DRIFT_STEPS..=DRIFT_STEPS {
        let Ok(expected) = hotp(&secret, step + drift) else {
            return false;
        };
        matched |= candidate.ct_eq(expected.as_bytes());
    }
    bool::from(matched)
}

/// The `otpauth://totp/` provisioning URI authenticator apps read from the
/// setup QR code.
///
/// Issuer and account are percent-encoded in the label (and the issuer
/// again in the query), with the fixed [`RFC 6238`] parameters the apps
/// default to anyway — SHA1, 6 digits, 30 seconds.
///
/// [`RFC 6238`]: https://datatracker.ietf.org/doc/html/rfc6238
#[must_use]
pub fn provisioning_uri(secret_b32: &str, account: &str, issuer: &str) -> String {
    let encoded_issuer = percent_encode(issuer);
    let label = format!("{encoded_issuer}:{}", percent_encode(account));
    format!(
        "otpauth://totp/{label}?secret={secret_b32}&issuer={encoded_issuer}&algorithm=SHA1&digits={CODE_DIGITS}&period={TIME_STEP_SECONDS}"
    )
}

/// `n` single-use backup codes, each 8 characters drawn uniformly from
/// `0-9` and `a-z` (the length and alphabet Pleroma's recovery codes use).
/// Store the hashes, show the codes once.
#[must_use]
pub fn generate_backup_codes(n: usize) -> Vec<String> {
    (0..n)
        .map(|_| {
            (0..BACKUP_CODE_LEN)
                .map(|_| char::from(random_backup_character()))
                .collect()
        })
        .collect()
}

/// The hex [SHA-256] of a backup `code`, lowercase: what gets stored, so a
/// database dump never reveals usable codes.
///
/// [SHA-256]: https://datatracker.ietf.org/doc/html/rfc6234
#[must_use]
pub fn hash_backup_code(code: &str) -> String {
    sha256_hex(code)
}

/// One character of a backup code: uniform over the 36-character alphabet
/// by rejection sampling inside `rand` (no modulo bias).
fn random_backup_character() -> u8 {
    let pick: u8 = OsRng.gen_range(0..BACKUP_ALPHABET_SIZE);
    if pick < 10 {
        b'0' + pick
    } else {
        b'a' + (pick - 10)
    }
}

/// Decodes a base32 secret for [`code_at`] and [`verify`]: surrounding
/// whitespace and either case tolerated, padding stripped by the crate;
/// an empty result is rejected like a parse failure, since no
/// authenticator app mints a zero-length secret.
fn decode_secret(secret_b32: &str) -> AppResult<Vec<u8>> {
    let normalized = secret_b32.trim().to_uppercase();
    let secret = base32::decode(BASE32_DECODE, &normalized).ok_or_else(|| {
        AppError::BadRequest("two-factor secret is not a valid base32 string".to_owned())
    })?;
    if secret.is_empty() {
        return Err(AppError::BadRequest(
            "two-factor secret decodes to zero bytes".to_owned(),
        ));
    }
    Ok(secret)
}

/// Decimal modulus 10<sup>`digits`</sup>, computed in `const` context so
/// [`CODE_MODULUS`] cannot drift from [`CODE_DIGITS`].
const fn decimal_modulus(digits: usize) -> u32 {
    let mut modulus: u32 = 1;
    let mut remaining = digits;
    while remaining > 0 {
        modulus *= 10;
        remaining -= 1;
    }
    modulus
}

/// The [`RFC 4226`] HOTP code for `key` and counter `step`: HMAC-SHA1 over
/// the big-endian counter, dynamic truncation to 6 zero-padded digits.
///
/// [`RFC 4226`]: https://datatracker.ietf.org/doc/html/rfc4226
fn hotp(key: &[u8], step: i64) -> AppResult<String> {
    let mut mac = HmacSha1::new_from_slice(key)
        .map_err(|err| AppError::Internal(format!("could not key HMAC-SHA1: {err}")))?;
    mac.update(&step.to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let nibble = hash
        .last()
        .ok_or_else(|| AppError::Internal("HMAC-SHA1 produced no output".to_owned()))?;
    let offset = usize::from(nibble & NIBBLE_MASK);
    let window = hash
        .get(offset..offset + TRUNCATED_WINDOW_BYTES)
        .ok_or_else(|| AppError::Internal("HMAC-SHA1 window out of range".to_owned()))?;
    let mut bytes = [0_u8; TRUNCATED_WINDOW_BYTES];
    bytes.copy_from_slice(window);
    let truncated = (u32::from_be_bytes(bytes) & SIGN_MASK) % CODE_MODULUS;
    Ok(format!("{truncated:0CODE_DIGITS$}"))
}

/// One uppercase hex digit for `nibble` (which must be below 16).
fn hex_digit(nibble: u8) -> char {
    if nibble < 10 {
        char::from(b'0' + nibble)
    } else {
        char::from(HEX_UPPER_BASE + (nibble - 10))
    }
}

/// `value` percent-encoded per [`RFC 3986`]: everything outside the
/// unreserved set becomes `%XX` with uppercase hex, byte by byte, so UTF-8
/// account names and issuer names survive the trip into the URI label and
/// query of an `otpauth://` URI.
///
/// [`RFC 3986`]: https://datatracker.ietf.org/doc/html/rfc3986
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            other => {
                encoded.push('%');
                encoded.push(hex_digit(other >> 4));
                encoded.push(hex_digit(other & NIBBLE_MASK));
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// The `RFC 6238` appendix B secret, ASCII `12345678901234567890`,
    /// spelled in base32 as [`code_at`] and [`verify`] take it.
    const RFC_SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    /// `RFC 6238` appendix B: the 8-digit codes for these times are
    /// `94287082`, `07081804` and `89005924`; the 6-digit codes this
    /// module emits are their low six digits.
    const RFC_VECTORS: [(i64, &str); 3] = [
        (59, "287082"),
        (1_111_111_109, "081804"),
        (1_234_567_890, "005924"),
    ];

    /// A code from a step far enough away that only drift ±1 could accept
    /// it.
    fn code_at_step(secret: &str, step: i64) -> String {
        code_at(secret, step * TIME_STEP_SECONDS).unwrap()
    }

    #[test]
    fn rfc6238_test_vectors_match() {
        for (unix_time, expected) in RFC_VECTORS {
            assert_eq!(
                code_at(RFC_SECRET, unix_time).unwrap(),
                expected,
                "code of the RFC 6238 secret at T={unix_time}"
            );
        }
    }

    #[test]
    fn codes_are_six_zero_padded_digits() {
        for t in 0..12 {
            let code = code_at(RFC_SECRET, t * 30 + 7).unwrap();
            assert_eq!(code.len(), 6, "code at T={t} has six digits");
            assert!(
                code.bytes().all(|b| b.is_ascii_digit()),
                "code at T={t} is all digits"
            );
        }
    }

    #[test]
    fn a_step_shares_its_code_and_steps_differ() {
        assert_eq!(
            code_at(RFC_SECRET, 0).unwrap(),
            code_at(RFC_SECRET, 29).unwrap(),
            "the same step (0) yields the same code"
        );
        assert_eq!(
            code_at(RFC_SECRET, 59).unwrap(),
            code_at(RFC_SECRET, 30).unwrap(),
            "T=59 and T=30 are both step 1"
        );
        assert_ne!(
            code_at(RFC_SECRET, 29).unwrap(),
            code_at(RFC_SECRET, 30).unwrap(),
            "consecutive steps differ"
        );
    }

    #[test]
    fn verify_accepts_current_step_and_drift_window() {
        let code = code_at(RFC_SECRET, 59).unwrap();
        assert!(verify(RFC_SECRET, &code, 59), "the current step verifies");
        assert!(
            verify(RFC_SECRET, &code, 59 - TIME_STEP_SECONDS),
            "one step back verifies"
        );
        assert!(
            verify(RFC_SECRET, &code, 59 + TIME_STEP_SECONDS),
            "one step forward verifies"
        );
    }

    #[test]
    fn verify_rejects_outside_the_drift_window() {
        let code = code_at(RFC_SECRET, 59).unwrap();
        assert!(
            !verify(RFC_SECRET, &code, 59 - 2 * TIME_STEP_SECONDS),
            "two steps back fails"
        );
        assert!(
            !verify(RFC_SECRET, &code, 59 + 2 * TIME_STEP_SECONDS),
            "two steps forward fails"
        );
    }

    #[test]
    fn verify_rejects_wrong_and_malformed_codes() {
        assert!(
            !verify(RFC_SECRET, &code_at_step(RFC_SECRET, 42), 59),
            "a far-away step's code must not pass"
        );
        for junk in ["", "12345", "1234567", "abcdef", "1234 6", "1234a6"] {
            assert!(!verify(RFC_SECRET, junk, 59), "`{junk}` is not a code");
        }
    }

    #[test]
    fn verify_rejects_garbage_secrets() {
        let code = code_at(RFC_SECRET, 59).unwrap();
        for junk in ["not base32!!", "", "====", "0!"] {
            assert!(!verify(junk, &code, 59), "a broken secret never verifies");
        }
    }

    #[test]
    fn garbage_secrets_are_bad_requests() {
        for junk in ["not base32!!", "====", "0!", "1-2 3", "*"] {
            let outcome = code_at(junk, 59);
            let Err(AppError::BadRequest(detail)) = outcome else {
                panic!("`{junk}` must fail as a 400, got an ok or another variant");
            };
            assert!(
                !detail.contains(junk.trim()),
                "the error must not quote the secret: `{detail}`"
            );
        }
    }

    #[test]
    fn empty_secrets_are_bad_requests() {
        for blank in ["", "   ", "\t\n"] {
            assert!(
                matches!(code_at(blank, 59), Err(AppError::BadRequest(_))),
                "a blank secret must fail as a 400"
            );
        }
    }

    #[test]
    fn secrets_parse_in_either_case_and_tolerate_padding() {
        assert_eq!(
            code_at(&RFC_SECRET.to_lowercase(), 59).unwrap(),
            "287082",
            "lowercase base32 is the same secret"
        );
        // 16 characters are two full base32 groups; the padded spelling of a
        // 10-byte prefix decodes through the tolerant alphabet.
        assert!(
            code_at("GEZDGNBVGY3TQOJQ=", 59).is_ok(),
            "a stray pad is tolerated"
        );
    }

    #[test]
    fn generated_secrets_are_20_random_base32_bytes() {
        let mut seen = HashSet::new();
        for _ in 0..64 {
            let secret = generate_secret();
            assert_eq!(secret.len(), 32, "20 bytes are 32 base32 characters");
            assert!(!secret.contains('='), "no padding");
            assert!(
                secret
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()),
                "`{secret}` is uppercase base32"
            );
            assert_eq!(
                base32::decode(BASE32, &secret).unwrap().len(),
                SECRET_BYTES,
                "the secret decodes to 20 bytes"
            );
            assert!(
                seen.insert(secret.clone()),
                "the CSPRNG must not repeat a secret"
            );
            assert!(
                code_at(&secret, 59).is_ok(),
                "a generated secret always parses back"
            );
        }
    }

    #[test]
    fn provisioning_uri_matches_the_otpauth_shape() {
        let uri = provisioning_uri(RFC_SECRET, "alice@example.com", "Wired");
        assert_eq!(
            uri,
            format!(
                "otpauth://totp/Wired:alice%40example.com?secret={RFC_SECRET}&issuer=Wired&algorithm=SHA1&digits=6&period=30"
            ),
            "the URI is the fixed otpauth shape"
        );
    }

    #[test]
    fn provisioning_uri_percent_encodes_label_parts() {
        let uri = provisioning_uri(RFC_SECRET, "rémi:wired/be", "ACME Corp");
        assert!(
            uri.starts_with("otpauth://totp/ACME%20Corp:r%C3%A9mi%3Awired%2Fbe?secret="),
            "issuer and account are percent-encoded per byte, got `{uri}`"
        );
        assert!(
            uri.contains("&issuer=ACME%20Corp"),
            "the issuer query value is encoded too, got `{uri}`"
        );
        assert!(
            !uri.contains(' ') && !uri.contains("rémi"),
            "raw spaces and non-ASCII bytes never appear, got `{uri}`"
        );
    }

    #[test]
    fn backup_codes_have_the_expected_shape() {
        let codes = generate_backup_codes(10);
        assert_eq!(codes.len(), 10, "n in, n out");
        for code in &codes {
            assert_eq!(code.len(), BACKUP_CODE_LEN, "codes are 8 characters");
            assert!(!code.contains('-'), "`{code}` is plain, not `xxxx-xxxx`");
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "`{code}` is [a-z0-9] only"
            );
        }
    }

    #[test]
    fn backup_codes_are_unique_and_zero_makes_none() {
        assert!(generate_backup_codes(0).is_empty(), "zero codes");
        let codes = generate_backup_codes(1000);
        let unique: HashSet<&String> = codes.iter().collect();
        assert_eq!(unique.len(), 1000, "1000 CSPRNG draws never collide");
    }

    #[test]
    fn backup_code_hashes_are_lowercase_hex_sha256() {
        // The published SHA-256 of the ASCII letter "a".
        assert_eq!(
            hash_backup_code("a"),
            "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb",
            "hash matches the reference digest"
        );
        let hashes: HashSet<String> = ["a", "", "0123456789", "abcdef01", "résumé-code"]
            .into_iter()
            .map(hash_backup_code)
            .collect();
        assert_eq!(hashes.len(), 5, "different codes, different digests");
        for hash in hashes {
            assert_eq!(hash.len(), 64, "a SHA-256 is 32 hex bytes");
            assert!(
                hash.bytes()
                    .all(|b| (b'a'..=b'f').contains(&b) || b.is_ascii_digit()),
                "`{hash}` is lowercase hex"
            );
        }
    }

    #[test]
    fn backup_codes_round_trip_their_verification() {
        // The contract storage code relies on: the code hashes to what the
        // stored hash holds, and a mistyped code does not.
        let codes = generate_backup_codes(3);
        let stored: HashSet<String> = codes
            .iter()
            .map(String::as_str)
            .map(hash_backup_code)
            .collect();
        for code in &codes {
            assert!(
                stored.contains(&hash_backup_code(code)),
                "an issued code matches its stored hash"
            );
        }
        let head = codes[0].chars().next().unwrap();
        let replacement = if head == 'q' { 'r' } else { 'q' };
        let rest: String = codes[0].chars().skip(1).collect();
        let typo = format!("{replacement}{rest}");
        assert!(
            !stored.contains(&hash_backup_code(&typo)),
            "a one-character typo does not match"
        );
    }

    #[test]
    fn percent_encoding_keeps_only_unreserved_characters() {
        assert_eq!(
            percent_encode("aZ09-._~"),
            "aZ09-._~",
            "unreserved pass through"
        );
        assert_eq!(
            percent_encode("a b/c?d=e&f"),
            "a%20b%2Fc%3Fd%3De%26f",
            "reserved bytes are %XX"
        );
        assert_eq!(percent_encode("é"), "%C3%A9", "UTF-8 is encoded byte-wise");
    }
}
