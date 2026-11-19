//! Reads the settings that bound uploads and the three feature toggles
//! that gate the endpoints of [`crate::api::misc`].

#[cfg(doc)]
use super::Config;

use crate::extract::multipart::UploadLimits;

use super::{
    defaults::{
        DEFAULT_AVATAR_LIMIT_BYTES, DEFAULT_BANNER_LIMIT_BYTES, DEFAULT_CAPTCHA_ENABLED,
        DEFAULT_HEALTHCHECK_ENABLED, DEFAULT_MONGOOSEIM_ENABLED, DEFAULT_UPLOAD_LIMIT_BYTES,
        DEFAULT_UPLOAD_MAX_FILES,
    },
    error::ConfigError,
    federation_types::Toggle,
    parse::{boolean, number},
    vars::{
        AVATAR_LIMIT_BYTES_VAR, BANNER_LIMIT_BYTES_VAR, CAPTCHA_ENABLED_VAR,
        HEALTHCHECK_ENABLED_VAR, MONGOOSEIM_ENABLED_VAR, UPLOAD_LIMIT_BYTES_VAR,
        UPLOAD_MAX_FILES_VAR,
    },
};

/// The shape [`upload_settings`] answers: the per-request multipart limits
/// and the two profile-image byte caps, in [`Config`] field order; factored
/// out because a mixed tuple of a struct and two numbers reads better by
/// name.
pub(crate) type UploadSettings = (UploadLimits, usize, usize);

/// Reads the settings that bound uploads: the per-request multipart limits
/// (`UPLOAD_LIMIT_BYTES` and `UPLOAD_MAX_FILES`) and the two profile-image
/// byte caps (`AVATAR_LIMIT_BYTES` for the avatar, `BANNER_LIMIT_BYTES` for
/// the banner and the background image alike). Grouped because each is a
/// plain default-or-parse and [`Config::from_lookup`] has a size budget
/// (AGENTS.md); the tuple comes back in field order.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when any of the four variables
/// holds a value that is not a whole number.
pub(crate) fn upload_settings<F>(lookup: &F) -> Result<UploadSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok((
        UploadLimits {
            max_file_bytes: number(lookup, UPLOAD_LIMIT_BYTES_VAR, DEFAULT_UPLOAD_LIMIT_BYTES)?,
            max_files: number(lookup, UPLOAD_MAX_FILES_VAR, DEFAULT_UPLOAD_MAX_FILES)?,
        },
        number(lookup, AVATAR_LIMIT_BYTES_VAR, DEFAULT_AVATAR_LIMIT_BYTES)?,
        number(lookup, BANNER_LIMIT_BYTES_VAR, DEFAULT_BANNER_LIMIT_BYTES)?,
    ))
}

/// Reads the three feature toggles that gate the endpoints of
/// [`crate::api::misc`]: the `MongooseIM` auth endpoints, the captcha
/// provider and the healthcheck. Grouped because each is a plain
/// default-or-parse and [`Config::from_lookup`] has a size budget
/// (AGENTS.md); the tuple comes back in field order.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] for whichever variable holds a
/// value that is neither spelling.
pub(crate) fn misc_toggles<F>(lookup: &F) -> Result<(Toggle, Toggle, Toggle), ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok((
        boolean(lookup, MONGOOSEIM_ENABLED_VAR, DEFAULT_MONGOOSEIM_ENABLED)?.into(),
        boolean(lookup, CAPTCHA_ENABLED_VAR, DEFAULT_CAPTCHA_ENABLED)?.into(),
        boolean(lookup, HEALTHCHECK_ENABLED_VAR, DEFAULT_HEALTHCHECK_ENABLED)?.into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::test_support::{config_with, try_config_with};

    /// The two profile-image byte caps: Pleroma's avatar/banner defaults,
    /// the environment overriding each, and `Debug` showing both.
    #[test]
    fn reads_custom_profile_image_limits() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.avatar_limit_bytes, 2_000_000,
            "default AVATAR_LIMIT_BYTES"
        );
        assert_eq!(
            defaults.banner_limit_bytes, 4_000_000,
            "default BANNER_LIMIT_BYTES"
        );

        let config = config_with(&[
            (AVATAR_LIMIT_BYTES_VAR, "1024"),
            (BANNER_LIMIT_BYTES_VAR, "2048"),
        ]);
        assert_eq!(
            config.avatar_limit_bytes, 1_024,
            "custom AVATAR_LIMIT_BYTES"
        );
        assert_eq!(
            config.banner_limit_bytes, 2_048,
            "custom BANNER_LIMIT_BYTES"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("avatar_limit_bytes: 1024"),
            "the avatar cap is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("banner_limit_bytes: 2048"),
            "the banner cap is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn misc_toggles_parse_booleans() {
        for var in [
            MONGOOSEIM_ENABLED_VAR,
            CAPTCHA_ENABLED_VAR,
            HEALTHCHECK_ENABLED_VAR,
        ] {
            for (value, want) in [
                ("true", true),
                ("TRUE", true),
                (" 1 ", true),
                ("false", false),
                ("FaLsE", false),
                ("0", false),
                ("", false),
            ] {
                let config = config_with(&[(var, value)]);
                let got = match var {
                    MONGOOSEIM_ENABLED_VAR => config.mongooseim_enabled,
                    CAPTCHA_ENABLED_VAR => config.captcha_enabled,
                    _ => config.healthcheck_enabled,
                };
                assert_eq!(got.get(), want, "{var}={value:?} means {want}");
            }
        }
    }

    #[test]
    fn invalid_misc_toggles_are_errors() {
        for var in [
            MONGOOSEIM_ENABLED_VAR,
            CAPTCHA_ENABLED_VAR,
            HEALTHCHECK_ENABLED_VAR,
        ] {
            let err = try_config_with(&[(var, "maybe")]).unwrap_err();
            assert_eq!(err, ConfigError::InvalidBoolean(var), "error for {var}");
            assert!(
                err.to_string().starts_with(&format!("invalid {var}: ")),
                "message names {var}"
            );
        }
    }

    #[test]
    fn debug_shows_the_misc_toggles() {
        let config = config_with(&[
            (MONGOOSEIM_ENABLED_VAR, "true"),
            (CAPTCHA_ENABLED_VAR, "false"),
            (HEALTHCHECK_ENABLED_VAR, "true"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("mongooseim_enabled: Toggle(true)"),
            "the `MongooseIM` toggle is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("captcha_enabled: Toggle(false)"),
            "the captcha toggle is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("healthcheck_enabled: Toggle(true)"),
            "the healthcheck toggle is visible in Debug output: {debug}"
        );
    }
}
