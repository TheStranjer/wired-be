//! Password hashing: Argon2id for new hashes, legacy formats still verified.
//!
//! Per `docs/ARCHITECTURE.md` the accounts table keeps a PHC string. New
//! passwords hash with Argon2id at the crate defaults and a fresh 16-byte salt
//! from the OS CSPRNG (`OsRng`, via the `getrandom` crate, as [`hash_password`]
//! wraps [`argon2::password_hash::try_generate_salt`]). Imported Pleroma users
//! must still be able to log in, so [`verify_password`] also accepts the two
//! legacy formats Pleroma stored — Comeonin-style
//! `$pbkdf2-sha512$<iterations>$<salt b64>$<hash b64>`
//! (`lib/pleroma/password/pbkdf2.ex`) and bcrypt `$2a$`/`$2b$`
//! (`bcrypt_elixir`) — and [`needs_rehash`] flags any hash that is not Argon2id
//! so the login path can transparently upgrade it.
//!
//! Nothing here logs or `Debug`-prints its inputs: passwords and hash strings
//! are consumed as plain data and never formatted into messages or traces.

use argon2::{
    ARGON2ID_IDENT, Argon2,
    password_hash::{
        PasswordHasher as _, PasswordVerifier as _, phc::PasswordHash, try_generate_salt,
    },
};
use base64::{
    Engine as _,
    alphabet::Alphabet,
    engine::general_purpose::{GeneralPurpose, NO_PAD},
};
use pbkdf2::{pbkdf2_hmac, sha2::Sha512};

use crate::error::{AppError, AppResult};

/// Prefix of the legacy Pleroma/Comeonin PBKDF2-SHA512 hashes.
const PBKDF2_SHA512_PREFIX: &str = "$pbkdf2-sha512$";
/// Prefix of bcrypt hashes with the `$2a$` minor version marker.
const BCRYPT_2A_PREFIX: &str = "$2a$";
/// Prefix of bcrypt hashes with the `$2b$` minor version marker.
const BCRYPT_2B_PREFIX: &str = "$2b$";
/// Key length Pleroma derived (64 bytes, `length: 64` in
/// `lib/pleroma/password/pbkdf2.ex`); stored hashes must match it.
const PBKDF2_KEY_LEN: usize = 64;
/// The adapted base64 alphabet Comeonin uses: standard base64 with `+`
/// spelled `.` and no padding (`decode64`/`encode64` in
/// `lib/pleroma/password/pbkdf2.ex`).
const COMEONIN_B64_ALPHABET: &str =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789./";

/// Hash `plain` into a PHC string: Argon2id, crate-default parameters, and a
/// fresh random salt, so the same password never hashes twice the same way.
///
/// # Errors
///
/// [`AppError::Internal`] if the OS RNG fails or hashing cannot run. The
/// detail names only the stage, never the password.
pub fn hash_password(plain: &str) -> AppResult<String> {
    let salt = try_generate_salt()
        .map_err(|err| AppError::Internal(format!("could not generate a password salt: {err}")))?;
    Argon2::default()
        .hash_password_with_salt(plain.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| AppError::Internal(format!("could not hash the password: {err}")))
}

/// Check `plain` against a stored `hash` of any supported format.
///
/// Argon2id PHC strings are verified by `argon2`, `$pbkdf2-sha512$` strings as
/// Pleroma stored them, and `$2a$`/`$2b$` strings as bcrypt. A malformed or
/// foreign hash never verifies and never panics: it is simply `false`, so
/// callers can hand it untrusted input directly.
#[must_use]
pub fn verify_password(plain: &str, hash: &str) -> bool {
    if hash.starts_with(PBKDF2_SHA512_PREFIX) {
        return verify_pbkdf2_sha512(plain, hash);
    }
    if hash.starts_with(BCRYPT_2A_PREFIX) || hash.starts_with(BCRYPT_2B_PREFIX) {
        return verify_bcrypt(plain, hash);
    }
    verify_argon2(plain, hash)
}

/// Whether `stored_hash` is anything other than Argon2id, i.e. worth
/// replacing with a fresh [`hash_password`] output after a successful login.
/// Unparseable strings are `true` too: they cannot be Argon2id.
#[must_use]
pub fn needs_rehash(stored_hash: &str) -> bool {
    !PasswordHash::new(stored_hash).is_ok_and(|parsed| parsed.algorithm == ARGON2ID_IDENT)
}

/// Verify an Argon2 PHC string; anything that does not parse (bcrypt, junk)
/// or does not match is `false`.
#[must_use]
fn verify_argon2(plain: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(plain.as_bytes(), &parsed)
            .is_ok()
    })
}

/// Verify a bcrypt hash; the crate reports malformed input as an error, which
/// is `false` here.
#[must_use]
fn verify_bcrypt(plain: &str, hash: &str) -> bool {
    bcrypt::verify(plain, hash).unwrap_or(false)
}

/// Verify a legacy Pleroma hash: `$pbkdf2-sha512$<iterations>$<salt>$<hash>`
/// with Comeonin's adapted base64 (`+` as `.`, no padding) and a 64-byte
/// PBKDF2-HMAC-SHA512 key, exactly as `Pleroma.Password.Pbkdf2.verify_pass/2`
/// computes it. Any other digest tag (e.g. `$pbkdf2-sha256$`) is out of scope
/// for this ticket and never verifies.
#[must_use]
fn verify_pbkdf2_sha512(plain: &str, hash: &str) -> bool {
    // Elixir's `String.split(hash, "$", trim: true)` drops empty fields, so
    // after the prefix exactly three remain: iterations, salt and hash.
    let Some(rest) = hash.strip_prefix(PBKDF2_SHA512_PREFIX) else {
        return false;
    };
    let fields: Vec<&str> = rest.split('$').filter(|field| !field.is_empty()).collect();
    let &[iterations, salt, expected] = fields.as_slice() else {
        return false;
    };
    let Ok(iterations) = iterations.parse::<u32>() else {
        return false;
    };
    let Some(salt) = decode_comeonin_b64(salt) else {
        return false;
    };
    let Some(expected) = decode_comeonin_b64(expected) else {
        return false;
    };
    if salt.is_empty() || expected.len() != PBKDF2_KEY_LEN {
        return false;
    }
    let mut derived = [0_u8; PBKDF2_KEY_LEN];
    pbkdf2_hmac::<Sha512>(plain.as_bytes(), &salt, iterations, &mut derived);
    constant_time_eq(&derived, &expected)
}

/// Decode Comeonin's adapted base64; `None` for any invalid spelling.
#[must_use]
fn decode_comeonin_b64(text: &str) -> Option<Vec<u8>> {
    let alphabet = Alphabet::new(COMEONIN_B64_ALPHABET).ok()?;
    GeneralPurpose::new(&alphabet, NO_PAD).decode(text).ok()
}

/// Length-sensitive, byte-content-independent equality of two digests.
#[must_use]
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut difference = a.len() ^ b.len();
    for (x, y) in a.iter().zip(b) {
        difference |= usize::from(x ^ y);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The password every known-answer vector was made for.
    const PASSWORD: &str = "correct horse battery staple";
    /// A password that must never verify against `PASSWORD`'s hashes.
    const WRONG_PASSWORD: &str = "wrong horse battery staple";
    /// Published bcrypt `$2a$` test vector (Openwall/jBCrypt suite) for "a".
    const BCRYPT_2A_FOR_A: &str = "$2a$06$m0CrhHm10qJ3lXRY.5zDGO3rS2KdeeWLuGmsfGlMfOxih58VYVfxe";
    /// bcrypt `$2b$` hash of `PASSWORD` (bcrypt 5.0.0, cost 12).
    const BCRYPT_2B: &str = "$2b$12$28E9Mpz84hFJrMMxMmid6.2qqJZdjG.YOLhO5OjHkMJ2Lmm4AaGr2";
    /// Pleroma-format hash of `PASSWORD`: 160 000 iterations, 16 salt bytes.
    const PBKDF2_160K: &str = "$pbkdf2-sha512$160000$AQEBAQEBAQEBAQEBAQEBAQ$i2QYLb2eUNSgxTfR/I0U1dVvN6yQtyTKZ3v3vimmN11ASGoQqEqzPBO7clczl1VuH3KNfjrLgjZ8Zoet6kVKPA";
    /// Same password at 1 000 iterations; the digest carries a `.` (a `+` in
    /// standard base64), exercising the Comeonin alphabet.
    const PBKDF2_DOT: &str = "$pbkdf2-sha512$1000$AQIDBAUGBwgJCgsMDQ4PEA$6ns5yb20qFEOAZtQC5I5NTYSjJdRm0Q4pr88XAZigT2g733NANqOfPZr8uEnB.zEYNt52uGV2XybZQAOdoT.OQ";
    /// And one whose digest carries both `.` and `/`.
    const PBKDF2_DOT_SLASH: &str = "$pbkdf2-sha512$1000$AAAAAAAAAAAAAAAAAAAAAA$2lN61Fb7c5RSif0yV8mMqQZS5N89hQhcgNHNtre.H/KBueh6Jl8GcuHO5YTz3OGfkDIalyt2EwQc2aMRSfsNZQ";
    /// Inputs no format should accept.
    const GARBAGE: [&str; 6] = [
        "",
        "not a hash",
        "$",
        "$argon2id$v=19$m=garbage",
        "$2b$12$tooshort",
        "$pbkdf2-sha512$notanumber$not!b64$nor!this",
    ];

    #[test]
    fn hashes_then_verifies() {
        let hash = hash_password(PASSWORD).unwrap();
        assert!(hash.starts_with("$argon2id$v=19$"), "PHC argon2id string");
        assert!(verify_password(PASSWORD, &hash), "the hash accepts itself");
    }

    #[test]
    fn wrong_password_fails() {
        let hash = hash_password(PASSWORD).unwrap();
        assert!(
            !verify_password(WRONG_PASSWORD, &hash),
            "a near miss is refused"
        );
    }

    #[test]
    fn same_password_hashes_differently() {
        let first = hash_password(PASSWORD).unwrap();
        let second = hash_password(PASSWORD).unwrap();
        assert_ne!(first, second, "the random salt makes each hash unique");
        assert!(verify_password(PASSWORD, &first), "both still verify");
        assert!(verify_password(PASSWORD, &second), "both still verify");
    }

    #[test]
    fn garbage_hashes_never_verify() {
        for hash in GARBAGE {
            assert!(!verify_password(PASSWORD, hash), "{hash} must not verify");
        }
    }

    #[test]
    fn bcrypt_vectors_verify() {
        assert!(
            verify_password("a", BCRYPT_2A_FOR_A),
            "published $2a$ vector"
        );
        assert!(
            !verify_password(PASSWORD, BCRYPT_2A_FOR_A),
            "wrong password"
        );
        assert!(verify_password(PASSWORD, BCRYPT_2B), "$2b$ vector");
        assert!(
            !verify_password(WRONG_PASSWORD, BCRYPT_2B),
            "wrong password on $2b$"
        );
    }

    #[test]
    fn pbkdf2_vectors_verify() {
        for hash in [PBKDF2_160K, PBKDF2_DOT, PBKDF2_DOT_SLASH] {
            assert!(verify_password(PASSWORD, hash), "Pleroma hash verifies");
            assert!(!verify_password(WRONG_PASSWORD, hash), "wrong password");
        }
    }

    #[test]
    fn malformed_pbkdf2_never_verifies() {
        for hash in [
            "$pbkdf2-sha512$1000$AQID",
            "$pbkdf2-sha512$1000$AQID$hash$extra",
            "$pbkdf2-sha512$1000$$digest",
            "$pbkdf2-sha512$1000$salt$",
            "$pbkdf2-sha512$-1$AQID$digest",
            "$pbkdf2-sha512$1000$not!b64$digest",
            "$pbkdf2-sha256$1000$AQID$digest",
        ] {
            assert!(!verify_password(PASSWORD, hash), "{hash} must not verify");
        }
    }

    #[test]
    fn needs_rehash_flags_everything_but_argon2id() {
        let fresh = hash_password(PASSWORD).unwrap();
        assert!(!needs_rehash(&fresh), "a fresh argon2id hash is current");
        // Same valid PHC string under the Argon2i ident: still not Argon2id.
        let argon2i = fresh.replacen("argon2id", "argon2i", 1);
        assert!(needs_rehash(&argon2i), "argon2i is not argon2id");
        assert!(needs_rehash(PBKDF2_160K), "legacy pbkdf2");
        assert!(needs_rehash(PBKDF2_DOT), "legacy pbkdf2, again");
        assert!(needs_rehash(BCRYPT_2A_FOR_A), "legacy $2a$ bcrypt");
        assert!(needs_rehash(BCRYPT_2B), "legacy $2b$ bcrypt");
        for hash in [
            "",
            "not a hash",
            "$",
            "$2b$12$tooshort",
            "$pbkdf2-sha512$notanumber$not!b64$nor!this",
        ] {
            assert!(needs_rehash(hash), "{hash} cannot be argon2id");
        }
    }

    #[test]
    fn comeonin_base64_rounds_through_the_adapted_alphabet() {
        assert_eq!(
            decode_comeonin_b64("AQEBAQEBAQEBAQEBAQEBAQ"),
            Some(vec![1_u8; 16])
        );
        assert_eq!(decode_comeonin_b64(""), Some(vec![]));
        assert_eq!(
            decode_comeonin_b64("+abc"),
            None,
            "'+' is not Comeonin text"
        );
        assert_eq!(
            decode_comeonin_b64("QQ=="),
            None,
            "padding is not Comeonin text"
        );
    }

    #[test]
    fn constant_time_eq_matches_equality() {
        assert!(constant_time_eq(b"abc", b"abc"), "equal");
        assert!(!constant_time_eq(b"abc", b"abd"), "one byte off");
        assert!(!constant_time_eq(b"abc", b"ab"), "length differs");
        assert!(constant_time_eq(b"", b""), "both empty");
    }
}
