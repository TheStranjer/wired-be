//! The four shared primitives every other reader in this module is built
//! from: the required-variable check and the number, boolean and
//! comma-list parses.

#[cfg(doc)]
use super::Config;

use super::error::ConfigError;

/// Reads a variable the instance cannot start without: its value, refusing
/// [`ConfigError::Missing`] when it is unset or empty. Both required settings
/// read through here, which is also what keeps [`Config::from_lookup`] inside
/// the size budget AGENTS.md gives it.
///
/// # Errors
///
/// Returns [`ConfigError::Missing`] naming `var`.
pub(crate) fn required_var<F>(lookup: &F, var: &'static str) -> Result<String, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(var)
        .filter(|value| !value.is_empty())
        .ok_or(ConfigError::Missing(var))
}

/// Reads a non-negative integer setting, applying `default` when the variable
/// is unset or empty.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] if the value is not a whole number.
pub(crate) fn number<F>(lookup: &F, var: &'static str, default: usize) -> Result<usize, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(var).filter(|value| !value.is_empty()) else {
        return Ok(default);
    };
    raw.parse().ok().ok_or(ConfigError::InvalidNumber(var))
}

/// Reads a boolean setting, applying `default` when the variable is unset or
/// empty. Accepted spellings are `true`/`false` (case-insensitive) and `1`/`0`.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] if the value is neither spelling.
pub(crate) fn boolean<F>(lookup: &F, var: &'static str, default: bool) -> Result<bool, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(var).filter(|value| !value.is_empty()) else {
        return Ok(default);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(ConfigError::InvalidBoolean(var)),
    }
}

/// Splits a comma-separated list into trimmed, non-empty owned values.
pub(crate) fn comma_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    use crate::config::{
        test_support::try_config_with,
        vars::{
            AVATAR_LIMIT_BYTES_VAR, BANNER_LIMIT_BYTES_VAR, INBOX_MAX_BYTES_VAR,
            JOB_CONCURRENCY_VAR, MAX_BIO_CHARS_VAR, MAX_CHAT_MESSAGE_CHARS_VAR,
            MAX_DISPLAY_NAME_CHARS_VAR, MAX_FETCHES_PER_ACTIVITY_VAR, MAX_MEDIA_ATTACHMENTS_VAR,
            MAX_MEDIA_DESCRIPTION_VAR, MAX_PINNED_STATUSES_VAR, MAX_POLL_EXPIRATION_VAR,
            MAX_POLL_OPTION_CHARS_VAR, MAX_POLL_OPTIONS_VAR, MAX_PROFILE_FIELDS_VAR,
            MAX_REMOTE_MENTION_LOOKUPS_VAR, MAX_REPORT_COMMENT_VAR,
            MAX_SCHEDULED_PER_USER_PER_DAY_VAR, MAX_SCHEDULED_PER_USER_VAR, MAX_STATUS_CHARS_VAR,
            MEDIA_PROXY_MAX_BYTES_VAR, MIN_EXPIRATION_SECS_VAR, MIN_PASSWORD_LENGTH_VAR,
            MIN_POLL_EXPIRATION_VAR, MIN_SCHEDULED_SECS_VAR, OAUTH_TOKEN_TTL_SECS_VAR,
            PASSWORD_RESET_TTL_SECS_VAR, PURGE_DELETED_STATUSES_DAYS_VAR,
            STREAMING_PING_INTERVAL_SECS_VAR, THREAD_FETCH_DEPTH_VAR, UPLOAD_LIMIT_BYTES_VAR,
            UPLOAD_MAX_FILES_VAR,
        },
    };

    #[test]
    fn invalid_numbers_are_errors() {
        for var in [
            MAX_STATUS_CHARS_VAR,
            MAX_MEDIA_ATTACHMENTS_VAR,
            MAX_POLL_OPTIONS_VAR,
            MAX_POLL_OPTION_CHARS_VAR,
            MIN_POLL_EXPIRATION_VAR,
            MAX_POLL_EXPIRATION_VAR,
            MAX_CHAT_MESSAGE_CHARS_VAR,
            MAX_MEDIA_DESCRIPTION_VAR,
            MAX_DISPLAY_NAME_CHARS_VAR,
            MAX_BIO_CHARS_VAR,
            MAX_PROFILE_FIELDS_VAR,
            MAX_REPORT_COMMENT_VAR,
            MAX_PINNED_STATUSES_VAR,
            MIN_EXPIRATION_SECS_VAR,
            MIN_SCHEDULED_SECS_VAR,
            MAX_SCHEDULED_PER_USER_VAR,
            MAX_SCHEDULED_PER_USER_PER_DAY_VAR,
            UPLOAD_LIMIT_BYTES_VAR,
            UPLOAD_MAX_FILES_VAR,
            AVATAR_LIMIT_BYTES_VAR,
            BANNER_LIMIT_BYTES_VAR,
            INBOX_MAX_BYTES_VAR,
            MAX_REMOTE_MENTION_LOOKUPS_VAR,
            MIN_PASSWORD_LENGTH_VAR,
            OAUTH_TOKEN_TTL_SECS_VAR,
            PASSWORD_RESET_TTL_SECS_VAR,
            JOB_CONCURRENCY_VAR,
            PURGE_DELETED_STATUSES_DAYS_VAR,
            STREAMING_PING_INTERVAL_SECS_VAR,
            MEDIA_PROXY_MAX_BYTES_VAR,
            THREAD_FETCH_DEPTH_VAR,
            MAX_FETCHES_PER_ACTIVITY_VAR,
        ] {
            let err = try_config_with(&[(var, "many")]).unwrap_err();
            assert_eq!(err, ConfigError::InvalidNumber(var), "error for {var}");
            assert!(err.source().is_none(), "no source for {var}");
            assert!(
                err.to_string().starts_with(&format!("invalid {var}: ")),
                "message names {var}"
            );
        }
    }
}
