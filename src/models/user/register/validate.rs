//! The shape rules a registration request is measured against: the
//! nickname's required, length, format and reserved-name checks, the
//! email's `x@y.z` test, the password's minimum length, and the
//! field-error messages — Ecto's own wording — they write into a
//! [`FieldErrors`] map.
//!
//! None of the three `validate_*` rules stops at its first failure: each
//! appends every message its field earned, so one submission is answered
//! with all of them at once (see [`super::create_local_user`], which calls
//! the three in a row and refuses the request when the map is not empty).
//! [`is_plausible_email`] is the parent's: the same shape rule the
//! email-changing endpoints reuse; [`is_valid_email_local`] and
//! [`is_valid_email_domain`] are its two halves.

use super::{FieldErrors, NICKNAME_FIELD, error::add_error, is_plausible_email};
use crate::config::Config;

/// Ecto's `validate_required` message: a value the account cannot be
/// created without is missing or blank.
pub(super) const CANNOT_BE_BLANK: &str = "can't be blank";
/// Ecto's `validate_format` message.
pub(super) const HAS_INVALID_FORMAT: &str = "has invalid format";
/// Ecto's `unique_constraint` message, from the index behind it.
pub(super) const HAS_ALREADY_BEEN_TAKEN: &str = "has already been taken";
/// Pleroma's `validate_not_restricted_nickname` message for a reserved
/// nickname (`lib/pleroma/user.ex`).
const INVALID_NICKNAME: &str = "Invalid nickname";
/// SQLSTATE of a Postgres unique violation.
pub(super) const SQLSTATE_UNIQUE_VIOLATION: &str = "23505";
/// Longest allowed nickname; the shortest is 1, which the blank rule
/// already enforces (Pleroma-era length bound 1..30).
const NICKNAME_MAX_LEN: usize = 30;

/// Nicknames registration never grants: routes this instance owns plus the
/// spirit of Pleroma's `restricted_nicknames` (`lib/pleroma/user.ex`); the
/// comparison in [`is_reserved`] is case-insensitive.
const RESERVED_NICKNAMES: [&str; 24] = [
    "admin",
    "root",
    "api",
    "users",
    "inbox",
    "outbox",
    "relay",
    "internal",
    "media",
    "proxy",
    "oauth",
    "objects",
    "activities",
    "tags",
    "notice",
    "main",
    "static",
    "system",
    ".well-known",
    "nodeinfo",
    "registration",
    "auth",
    "feed",
    "embed",
];

/// Characters the email local part may use: ASCII alphanumerics plus the
/// specials Pleroma's `@email_regex` allows (`lib/pleroma/user.ex`).
const EMAIL_LOCAL_SPECIALS: &str = ".!#$%&'*+/=?^_`{|}~-";

/// Ecto's interpolated `too_short` message for a minimum of `count`
/// characters.
pub(super) fn at_least(count: usize) -> String {
    format!("should be at least {count} character(s)")
}

/// Ecto's interpolated `too_long` message for a maximum of `count`
/// characters.
fn at_most(count: usize) -> String {
    format!("should be at most {count} character(s)")
}

/// Pleroma's shape rules on one (already trimmed) nickname: the blank rule
/// of `validate_required`, the 1..30 length bound, the
/// `~r/^[a-zA-Z0-9_-]+$/` format and `validate_not_restricted_nickname`.
pub(super) fn validate_nickname(nickname: &str, errors: &mut FieldErrors) {
    if nickname.is_empty() {
        add_error(errors, NICKNAME_FIELD, CANNOT_BE_BLANK);
        return;
    }
    if nickname.chars().count() > NICKNAME_MAX_LEN {
        add_error(errors, NICKNAME_FIELD, &at_most(NICKNAME_MAX_LEN));
    }
    if !is_local_nickname(nickname) {
        add_error(errors, NICKNAME_FIELD, HAS_INVALID_FORMAT);
    }
    if is_reserved(nickname) {
        add_error(errors, NICKNAME_FIELD, INVALID_NICKNAME);
    }
}

/// Whether `nickname` is what `^[a-zA-Z0-9_-]+$` matches: one or more ASCII
/// letters, digits, `_` or `-`. The character class is checked by hand (as
/// in [`crate::util::format`]) — that language needs no regex engine and
/// cannot backtrack.
fn is_local_nickname(nickname: &str) -> bool {
    !nickname.is_empty()
        && nickname
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Whether `nickname` is one of [`RESERVED_NICKNAMES`], case-insensitively
/// (Pleroma compares lower-cased spellings).
fn is_reserved(nickname: &str) -> bool {
    RESERVED_NICKNAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(nickname))
}

/// The submitted email trimmed, with an all-blank submission normalized to
/// `None` so it reads as "not given" (Ecto's `trim: true` cast).
pub(super) fn normalize_email(email: Option<&str>) -> Option<String> {
    email
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The email rules: required unless
/// [`accounts_email_optional`](Config::accounts_email_optional), and a
/// plausibly-formed `x@y.z` when given.
pub(super) fn validate_email(email: Option<&str>, config: &Config, errors: &mut FieldErrors) {
    match email {
        None if !config.accounts_email_optional => add_error(errors, "email", CANNOT_BE_BLANK),
        Some(value) if !is_plausible_email(value) => add_error(errors, "email", HAS_INVALID_FORMAT),
        None | Some(_) => {}
    }
}

/// The local part: at least one allowed character.
pub(super) fn is_valid_email_local(local: &str) -> bool {
    !local.is_empty()
        && local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || EMAIL_LOCAL_SPECIALS.contains(c))
}

/// The domain: at least two labels, each a valid hostname label.
pub(super) fn is_valid_email_domain(domain: &str) -> bool {
    domain.split('.').count() >= 2 && domain.split('.').all(is_valid_email_label)
}

/// A hostname label: alphanumerics and inner hyphens, starting and ending
/// alphanumeric.
fn is_valid_email_label(label: &str) -> bool {
    !label.is_empty()
        && label
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && label
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The password rules: required, and at least
/// [`min_password_length`](Config::min_password_length) characters
/// (counted in characters, not bytes).
pub(super) fn validate_password(password: &str, config: &Config, errors: &mut FieldErrors) {
    if password.is_empty() {
        add_error(errors, "password", CANNOT_BE_BLANK);
        return;
    }
    if password.chars().count() < config.min_password_length {
        add_error(errors, "password", &at_least(config.min_password_length));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::user::register::test_support::*;

    /// Every message the pure validation of `nickname` produced.
    fn nickname_errors(nickname: &str) -> Vec<String> {
        let mut errors = FieldErrors::new();
        validate_nickname(nickname, &mut errors);
        errors.remove("nickname").unwrap_or_default()
    }

    /// Every message the pure validation of `email` produced.
    fn email_errors(email: Option<&str>, config: &Config) -> Vec<String> {
        let normalized = normalize_email(email);
        let mut errors = FieldErrors::new();
        validate_email(normalized.as_deref(), config, &mut errors);
        errors.remove("email").unwrap_or_default()
    }

    /// Every message the pure validation of `password` produced.
    fn password_errors(password: &str, config: &Config) -> Vec<String> {
        let mut errors = FieldErrors::new();
        validate_password(password, config, &mut errors);
        errors.remove("password").unwrap_or_default()
    }

    #[test]
    fn nickname_rules() {
        assert_eq!(
            nickname_errors(""),
            vec![CANNOT_BE_BLANK],
            "a blank nickname is required"
        );
        assert!(
            nickname_errors("a").is_empty(),
            "one character is the shortest valid nickname"
        );
        assert!(
            nickname_errors(&"a".repeat(NICKNAME_MAX_LEN)).is_empty(),
            "thirty characters are still allowed"
        );
        assert_eq!(
            nickname_errors(&"a".repeat(NICKNAME_MAX_LEN + 1)),
            vec![at_most(NICKNAME_MAX_LEN)],
            "thirty-one characters are not"
        );
        for bad in ["a b", "a.b", "aé", "über", "a+b", "a@b"] {
            assert_eq!(
                nickname_errors(bad),
                vec![HAS_INVALID_FORMAT],
                "{bad:?} does not match ^[a-zA-Z0-9_-]+$"
            );
        }
        assert!(
            nickname_errors("Z-9_a").is_empty(),
            "letters, digits, _ and - are exactly the language"
        );
    }

    #[test]
    fn every_reserved_nickname_is_rejected_case_insensitively() {
        for reserved in RESERVED_NICKNAMES {
            let errors = nickname_errors(reserved);
            assert!(
                errors.contains(&INVALID_NICKNAME.to_owned()),
                "{reserved:?} is reserved, got {errors:?}"
            );
            let upper = reserved.to_uppercase();
            if upper != *reserved {
                assert!(
                    nickname_errors(&upper).contains(&INVALID_NICKNAME.to_owned()),
                    "{upper:?} is reserved too"
                );
            }
        }
        assert_eq!(
            nickname_errors("Admin"),
            vec![INVALID_NICKNAME],
            "a purely alphabetic reserved name yields exactly its one message"
        );
    }

    #[test]
    fn email_rules() {
        let required = Config::for_tests();
        assert_eq!(
            email_errors(None, &required),
            vec![CANNOT_BE_BLANK],
            "no email when it is required"
        );
        assert_eq!(
            email_errors(Some("   "), &required),
            vec![CANNOT_BE_BLANK],
            "a blank email reads as missing"
        );
        let optional = Config {
            accounts_email_optional: true,
            ..Config::for_tests()
        };
        assert!(
            email_errors(None, &optional).is_empty(),
            "no email passes when ACCOUNTS_EMAIL_OPTIONAL is on"
        );
        assert!(
            email_errors(Some("  "), &optional).is_empty(),
            "a blank email is not-given, not invalid"
        );
        for bad in [
            "plainaddress",
            "a@b",
            "@x.y",
            "a@",
            "a@.y",
            "a@y.",
            "a@y..z",
            "a b@x.y",
            "a@x y.z",
            "a@@x.y",
            "a@-x.y",
            "a@x-.y",
        ] {
            assert_eq!(
                email_errors(Some(bad), &required),
                vec![HAS_INVALID_FORMAT],
                "{bad:?} does not look like x@y.z"
            );
        }
        for good in [
            "x@y.z",
            "a@b.c",
            "A@B.C",
            "alice+tag@sub.example.org",
            "o'brien!#%&/=?^_`{|}~-@a-b.example.co.uk",
        ] {
            assert!(
                email_errors(Some(good), &required).is_empty(),
                "{good:?} looks like x@y.z"
            );
        }
    }

    #[test]
    fn password_rules() {
        let config = Config::for_tests();
        assert_eq!(
            password_errors("", &config),
            vec![CANNOT_BE_BLANK],
            "a blank password is required"
        );
        assert_eq!(
            password_errors(SHORT_PASSWORD, &config),
            vec![at_least(8)],
            "seven characters miss the default minimum of 8"
        );
        assert!(
            password_errors("exactly8", &config).is_empty(),
            "exactly eight characters pass"
        );
        assert_eq!(
            password_errors("ééééééé", &config),
            vec![at_least(8)],
            "the minimum counts characters, not bytes"
        );
        assert!(
            password_errors("éééééééé", &config).is_empty(),
            "eight non-ASCII characters are eight characters"
        );
        let strict = Config {
            min_password_length: 20,
            ..Config::for_tests()
        };
        assert_eq!(
            password_errors("twelve chars", &strict),
            vec![at_least(20)],
            "MIN_PASSWORD_LENGTH raises the bar"
        );
        let lenient = Config {
            min_password_length: SHORT_PASSWORD.chars().count(),
            ..Config::for_tests()
        };
        assert!(
            password_errors(SHORT_PASSWORD, &lenient).is_empty(),
            "and lowers it"
        );
    }
}
