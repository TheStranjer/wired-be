//! Reads the plain numeric content limits into `ContentLimits`.
//!
//! These are the sixteen `number` reads that bound status length,
//! attachments, polls, chat, uploads' pixels, expirations, inbox bodies,
//! mention lookups and scheduled statuses.

#[cfg(doc)]
use super::Config;

use super::{
    defaults::{
        DEFAULT_INBOX_MAX_BYTES, DEFAULT_MAX_CHAT_MESSAGE_CHARS, DEFAULT_MAX_IMAGE_PIXELS,
        DEFAULT_MAX_MEDIA_ATTACHMENTS, DEFAULT_MAX_PINNED_STATUSES, DEFAULT_MAX_POLL_EXPIRATION,
        DEFAULT_MAX_POLL_OPTION_CHARS, DEFAULT_MAX_POLL_OPTIONS,
        DEFAULT_MAX_REMOTE_MENTION_LOOKUPS, DEFAULT_MAX_REPORT_COMMENT,
        DEFAULT_MAX_SCHEDULED_PER_USER, DEFAULT_MAX_SCHEDULED_PER_USER_PER_DAY,
        DEFAULT_MAX_STATUS_CHARS, DEFAULT_MIN_EXPIRATION_SECS, DEFAULT_MIN_POLL_EXPIRATION,
        DEFAULT_MIN_SCHEDULED_SECS,
    },
    error::ConfigError,
    parse::number,
    vars::{
        INBOX_MAX_BYTES_VAR, MAX_CHAT_MESSAGE_CHARS_VAR, MAX_IMAGE_PIXELS_VAR,
        MAX_MEDIA_ATTACHMENTS_VAR, MAX_PINNED_STATUSES_VAR, MAX_POLL_EXPIRATION_VAR,
        MAX_POLL_OPTION_CHARS_VAR, MAX_POLL_OPTIONS_VAR, MAX_REMOTE_MENTION_LOOKUPS_VAR,
        MAX_REPORT_COMMENT_VAR, MAX_SCHEDULED_PER_USER_PER_DAY_VAR, MAX_SCHEDULED_PER_USER_VAR,
        MAX_STATUS_CHARS_VAR, MIN_EXPIRATION_SECS_VAR, MIN_POLL_EXPIRATION_VAR,
        MIN_SCHEDULED_SECS_VAR,
    },
};

/// What [`content_limits`] reads: the eleven plain numeric content limits,
/// named by what they cap and hung on the [`Config`] fields in
/// [`Config::from_lookup`] (`MAX_STATUS_CHARS`, `MAX_MEDIA_ATTACHMENTS`,
/// `MAX_POLL_OPTIONS`, `MAX_POLL_OPTION_CHARS`, `MIN_POLL_EXPIRATION`,
/// `MAX_POLL_EXPIRATION`, `MAX_CHAT_MESSAGE_CHARS`, `MAX_IMAGE_PIXELS`,
/// `MAX_REPORT_COMMENT`, `MAX_PINNED_STATUSES`, `MIN_EXPIRATION_SECS`).
#[derive(Debug)]
pub(crate) struct ContentLimits {
    /// The maximum status length (`MAX_STATUS_CHARS`).
    pub(crate) status_chars: usize,
    /// The media attachment count (`MAX_MEDIA_ATTACHMENTS`).
    pub(crate) media_attachments: usize,
    /// The poll option count (`MAX_POLL_OPTIONS`).
    pub(crate) poll_options: usize,
    /// The poll option length (`MAX_POLL_OPTION_CHARS`).
    pub(crate) poll_option_chars: usize,
    /// The shortest poll expiration, in seconds (`MIN_POLL_EXPIRATION`).
    pub(crate) min_poll_expiration_secs: usize,
    /// The longest poll expiration, in seconds (`MAX_POLL_EXPIRATION`).
    pub(crate) max_poll_expiration_secs: usize,
    /// The chat message length (`MAX_CHAT_MESSAGE_CHARS`).
    pub(crate) chat_message_chars: usize,
    /// The pixel count an uploaded image may decode to (`MAX_IMAGE_PIXELS`).
    pub(crate) image_pixels: usize,
    /// The length of a report's `comment` (`MAX_REPORT_COMMENT`).
    pub(crate) report_comment: usize,
    /// The number of statuses one account may pin (`MAX_PINNED_STATUSES`).
    pub(crate) pinned_statuses: usize,
    /// The shortest status expiration, in seconds (`MIN_EXPIRATION_SECS`).
    pub(crate) min_expiration_secs: usize,
    /// The largest inbound `ActivityPub` inbox body, in bytes
    /// (`INBOX_MAX_BYTES`).
    pub(crate) inbox_max_bytes: usize,
    /// The unknown-remote mentions one status may look up
    /// (`MAX_REMOTE_MENTION_LOOKUPS`).
    pub(crate) max_remote_mention_lookups: usize,
    /// How far in the future a `scheduled_at` create must be, in seconds
    /// (`MIN_SCHEDULED_SECS`).
    pub(crate) min_scheduled_secs: usize,
    /// How many pending scheduled rows one account may hold
    /// (`MAX_SCHEDULED_PER_USER`).
    pub(crate) max_scheduled_per_user: usize,
    /// How many pending scheduled rows one account may hold for one UTC
    /// day (`MAX_SCHEDULED_PER_USER_PER_DAY`).
    pub(crate) max_scheduled_per_user_per_day: usize,
}

/// Reads the sixteen plain numeric content limits: the maximum status length
/// (`MAX_STATUS_CHARS`), the media attachment count
/// (`MAX_MEDIA_ATTACHMENTS`), the poll option count (`MAX_POLL_OPTIONS`),
/// the poll option length (`MAX_POLL_OPTION_CHARS`), the shortest
/// (`MIN_POLL_EXPIRATION`) and longest (`MAX_POLL_EXPIRATION`) poll
/// expiration, the chat message length (`MAX_CHAT_MESSAGE_CHARS`), the
/// pixel count an uploaded image may decode to (`MAX_IMAGE_PIXELS`), the
/// length of a report's `comment` (`MAX_REPORT_COMMENT`), the number of
/// statuses one account may pin to its profile (`MAX_PINNED_STATUSES`),
/// the shortest `expires_in` a status create may ask for
/// (`MIN_EXPIRATION_SECS`), the largest inbound inbox body
/// (`INBOX_MAX_BYTES`), the unknown-remote mentions one status may look
/// up (`MAX_REMOTE_MENTION_LOOKUPS`) and the three settings that bound
/// scheduled statuses: the `scheduled_at` window (`MIN_SCHEDULED_SECS`),
/// the pending rows one account may hold (`MAX_SCHEDULED_PER_USER`) and
/// the pending rows it may hold for one UTC day
/// (`MAX_SCHEDULED_PER_USER_PER_DAY`). Grouped because each is the same
/// `number` read and [`Config::from_lookup`] has a size budget (AGENTS.md).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] for whichever variable holds a
/// value that is not a whole number.
pub(crate) fn content_limits<F>(lookup: &F) -> Result<ContentLimits, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(ContentLimits {
        status_chars: number(lookup, MAX_STATUS_CHARS_VAR, DEFAULT_MAX_STATUS_CHARS)?,
        media_attachments: number(
            lookup,
            MAX_MEDIA_ATTACHMENTS_VAR,
            DEFAULT_MAX_MEDIA_ATTACHMENTS,
        )?,
        poll_options: number(lookup, MAX_POLL_OPTIONS_VAR, DEFAULT_MAX_POLL_OPTIONS)?,
        poll_option_chars: number(
            lookup,
            MAX_POLL_OPTION_CHARS_VAR,
            DEFAULT_MAX_POLL_OPTION_CHARS,
        )?,
        min_poll_expiration_secs: number(
            lookup,
            MIN_POLL_EXPIRATION_VAR,
            DEFAULT_MIN_POLL_EXPIRATION,
        )?,
        max_poll_expiration_secs: number(
            lookup,
            MAX_POLL_EXPIRATION_VAR,
            DEFAULT_MAX_POLL_EXPIRATION,
        )?,
        chat_message_chars: number(
            lookup,
            MAX_CHAT_MESSAGE_CHARS_VAR,
            DEFAULT_MAX_CHAT_MESSAGE_CHARS,
        )?,
        image_pixels: number(lookup, MAX_IMAGE_PIXELS_VAR, DEFAULT_MAX_IMAGE_PIXELS)?,
        report_comment: number(lookup, MAX_REPORT_COMMENT_VAR, DEFAULT_MAX_REPORT_COMMENT)?,
        pinned_statuses: number(lookup, MAX_PINNED_STATUSES_VAR, DEFAULT_MAX_PINNED_STATUSES)?,
        min_expiration_secs: number(lookup, MIN_EXPIRATION_SECS_VAR, DEFAULT_MIN_EXPIRATION_SECS)?,
        inbox_max_bytes: number(lookup, INBOX_MAX_BYTES_VAR, DEFAULT_INBOX_MAX_BYTES)?,
        max_remote_mention_lookups: number(
            lookup,
            MAX_REMOTE_MENTION_LOOKUPS_VAR,
            DEFAULT_MAX_REMOTE_MENTION_LOOKUPS,
        )?,
        min_scheduled_secs: number(lookup, MIN_SCHEDULED_SECS_VAR, DEFAULT_MIN_SCHEDULED_SECS)?,
        max_scheduled_per_user: number(
            lookup,
            MAX_SCHEDULED_PER_USER_VAR,
            DEFAULT_MAX_SCHEDULED_PER_USER,
        )?,
        max_scheduled_per_user_per_day: number(
            lookup,
            MAX_SCHEDULED_PER_USER_PER_DAY_VAR,
            DEFAULT_MAX_SCHEDULED_PER_USER_PER_DAY,
        )?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::{
        Config,
        test_support::{config_with, try_config_with},
    };

    #[test]
    fn reads_custom_max_chat_message_chars() {
        let config = config_with(&[(MAX_CHAT_MESSAGE_CHARS_VAR, "140")]);
        assert_eq!(
            config.max_chat_message_chars, 140,
            "custom MAX_CHAT_MESSAGE_CHARS"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_chat_message_chars: 140"),
            "the chat length is visible in Debug output: {debug}"
        );
    }

    /// The report comment cap: Mastodon's 1000 by default, the environment
    /// overriding it, and `Debug` showing the value.
    #[test]
    fn reads_custom_max_report_comment() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.max_report_comment, 1_000,
            "default MAX_REPORT_COMMENT"
        );

        let config = config_with(&[(MAX_REPORT_COMMENT_VAR, "140")]);
        assert_eq!(config.max_report_comment, 140, "custom MAX_REPORT_COMMENT");
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_report_comment: 140"),
            "the report comment limit is visible in Debug output: {debug}"
        );
    }

    /// The pinned-status count: Pleroma's one by default, the environment
    /// overriding it, and `Debug` showing the value.
    #[test]
    fn reads_custom_max_pinned_statuses() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.max_pinned_statuses, 1,
            "default MAX_PINNED_STATUSES"
        );

        let config = config_with(&[(MAX_PINNED_STATUSES_VAR, "5")]);
        assert_eq!(config.max_pinned_statuses, 5, "custom MAX_PINNED_STATUSES");
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_pinned_statuses: 5"),
            "the pinned-status limit is visible in Debug output: {debug}"
        );
    }

    /// The shortest status expiration: Pleroma's 600 seconds by default,
    /// the environment overriding it, and `Debug` showing the value.
    #[test]
    fn reads_custom_min_expiration_secs() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.min_expiration_secs, 600,
            "default MIN_EXPIRATION_SECS"
        );

        let config = config_with(&[(MIN_EXPIRATION_SECS_VAR, "120")]);
        assert_eq!(
            config.min_expiration_secs, 120,
            "custom MIN_EXPIRATION_SECS"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("min_expiration_secs: 120"),
            "the expiration floor is visible in Debug output: {debug}"
        );
    }

    /// The poll option length cap: Pleroma's 200 characters by default, the
    /// environment overriding it, and `Debug` showing the value.
    #[test]
    fn reads_custom_max_poll_option_chars() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.max_poll_option_chars, 200,
            "default MAX_POLL_OPTION_CHARS"
        );

        let config = config_with(&[(MAX_POLL_OPTION_CHARS_VAR, "50")]);
        assert_eq!(
            config.max_poll_option_chars, 50,
            "custom MAX_POLL_OPTION_CHARS"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_poll_option_chars: 50"),
            "the poll option length cap is visible in Debug output: {debug}"
        );
    }

    /// The two poll expiration bounds: the 300-second floor and the
    /// 31_536_000-second (one-year) ceiling by default, the environment
    /// overriding each, and `Debug` showing both.
    #[test]
    fn reads_custom_poll_expiration_bounds() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.min_poll_expiration_secs, 300,
            "default MIN_POLL_EXPIRATION"
        );
        assert_eq!(
            defaults.max_poll_expiration_secs, 31_536_000,
            "default MAX_POLL_EXPIRATION"
        );

        let config = config_with(&[
            (MIN_POLL_EXPIRATION_VAR, "60"),
            (MAX_POLL_EXPIRATION_VAR, "86400"),
        ]);
        assert_eq!(
            config.min_poll_expiration_secs, 60,
            "custom MIN_POLL_EXPIRATION"
        );
        assert_eq!(
            config.max_poll_expiration_secs, 86_400,
            "custom MAX_POLL_EXPIRATION"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("min_poll_expiration_secs: 60"),
            "the poll floor is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("max_poll_expiration_secs: 86400"),
            "the poll ceiling is visible in Debug output: {debug}"
        );
    }

    /// The scheduled-statuses window and the two per-user limits: Pleroma's
    /// 300 seconds, 300 rows and 25 rows per day by default, the environment
    /// overriding each.
    #[test]
    fn scheduled_status_limits_default_and_read_their_variables() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.min_scheduled_secs, 300,
            "default MIN_SCHEDULED_SECS"
        );
        assert_eq!(
            defaults.max_scheduled_per_user, 300,
            "default MAX_SCHEDULED_PER_USER"
        );
        assert_eq!(
            defaults.max_scheduled_per_user_per_day, 25,
            "default MAX_SCHEDULED_PER_USER_PER_DAY"
        );

        let config = config_with(&[
            (MIN_SCHEDULED_SECS_VAR, "60"),
            (MAX_SCHEDULED_PER_USER_VAR, "10"),
            (MAX_SCHEDULED_PER_USER_PER_DAY_VAR, "5"),
        ]);
        assert_eq!(config.min_scheduled_secs, 60, "custom MIN_SCHEDULED_SECS");
        assert_eq!(
            config.max_scheduled_per_user, 10,
            "custom MAX_SCHEDULED_PER_USER"
        );
        assert_eq!(
            config.max_scheduled_per_user_per_day, 5,
            "custom MAX_SCHEDULED_PER_USER_PER_DAY"
        );
    }

    /// The three scheduled-status settings are visible in `Debug` output:
    /// they name limits, not secrets.
    #[test]
    fn debug_shows_the_scheduled_status_limits() {
        let config = config_with(&[
            (MIN_SCHEDULED_SECS_VAR, "60"),
            (MAX_SCHEDULED_PER_USER_VAR, "10"),
            (MAX_SCHEDULED_PER_USER_PER_DAY_VAR, "5"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("min_scheduled_secs: 60"),
            "the scheduled-statuses window is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("max_scheduled_per_user: 10"),
            "the per-user pending cap is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("max_scheduled_per_user_per_day: 5"),
            "the per-user per-day cap is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn max_image_pixels_defaults_to_forty_megapixels() {
        let config = config_with(&[]);
        assert_eq!(
            config.max_image_pixels, 40_000_000,
            "default max_image_pixels"
        );
    }

    #[test]
    fn reads_custom_max_image_pixels() {
        let config = config_with(&[(MAX_IMAGE_PIXELS_VAR, "1000")]);
        assert_eq!(config.max_image_pixels, 1_000, "custom MAX_IMAGE_PIXELS");
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_image_pixels: 1000"),
            "the pixel limit is visible in Debug output: {debug}"
        );
        let err = try_config_with(&[(MAX_IMAGE_PIXELS_VAR, "many")]).unwrap_err();
        assert_eq!(err, ConfigError::InvalidNumber(MAX_IMAGE_PIXELS_VAR));
    }

    #[test]
    fn inbox_max_bytes_defaults_to_one_mebibyte() {
        let config = config_with(&[]);
        assert_eq!(
            config.inbox_max_bytes, 1_048_576,
            "default INBOX_MAX_BYTES is 1 MiB"
        );
    }

    #[test]
    fn reads_custom_inbox_max_bytes() {
        let config = config_with(&[(INBOX_MAX_BYTES_VAR, "1024")]);
        assert_eq!(config.inbox_max_bytes, 1_024, "custom INBOX_MAX_BYTES");
        let debug = format!("{config:?}");
        assert!(
            debug.contains("inbox_max_bytes: 1024"),
            "the inbox body cap is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn max_remote_mention_lookups_defaults_to_ten() {
        let config = config_with(&[]);
        assert_eq!(
            config.max_remote_mention_lookups, 10,
            "default MAX_REMOTE_MENTION_LOOKUPS is ten"
        );
        assert_eq!(
            Config::for_tests().max_remote_mention_lookups,
            10,
            "the test config keeps the default, so mention tests see it too"
        );
    }

    #[test]
    fn reads_custom_max_remote_mention_lookups() {
        let config = config_with(&[(MAX_REMOTE_MENTION_LOOKUPS_VAR, "3")]);
        assert_eq!(
            config.max_remote_mention_lookups, 3,
            "custom MAX_REMOTE_MENTION_LOOKUPS"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_remote_mention_lookups: 3"),
            "the remote mention cap is visible in Debug output: {debug}"
        );
        let err = try_config_with(&[(MAX_REMOTE_MENTION_LOOKUPS_VAR, "plenty")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidNumber(MAX_REMOTE_MENTION_LOOKUPS_VAR),
            "a non-number cap is refused"
        );
    }
}
