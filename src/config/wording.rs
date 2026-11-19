//! Reads the per-field profile text limits into `TextLengthLimits` and
//! bundles the instance-wording trio `profile_and_limits` answers.

#[cfg(doc)]
use super::Config;

use super::{
    defaults::{
        DEFAULT_MAX_BIO_CHARS, DEFAULT_MAX_DISPLAY_NAME_CHARS, DEFAULT_MAX_MEDIA_DESCRIPTION,
        DEFAULT_MAX_PROFILE_FIELDS,
    },
    error::ConfigError,
    instance::{InstanceProfile, instance_profile},
    limits::{ContentLimits, content_limits},
    parse::number,
    vars::{
        MAX_BIO_CHARS_VAR, MAX_DISPLAY_NAME_CHARS_VAR, MAX_MEDIA_DESCRIPTION_VAR,
        MAX_PROFILE_FIELDS_VAR,
    },
};

/// What [`text_length_limits`] reads: the four default-or-parse profile
/// limits, named by what they cap and hung on the [`Config`] fields in
/// [`Config::from_lookup`] (`MAX_MEDIA_DESCRIPTION`,
/// `MAX_DISPLAY_NAME_CHARS`, `MAX_BIO_CHARS`, `MAX_PROFILE_FIELDS`).
#[derive(Debug)]
pub(crate) struct TextLengthLimits {
    /// The media attachment description length (`MAX_MEDIA_DESCRIPTION`).
    pub(crate) media_description: usize,
    /// The display name length (`MAX_DISPLAY_NAME_CHARS`).
    pub(crate) display_name_chars: usize,
    /// The profile bio length (`MAX_BIO_CHARS`).
    pub(crate) bio_chars: usize,
    /// The profile metadata field count (`MAX_PROFILE_FIELDS`).
    pub(crate) profile_fields: usize,
}

/// Reads the four per-field profile limits that live beside the status
/// ones: the media attachment description (`MAX_MEDIA_DESCRIPTION`), the
/// display name (`MAX_DISPLAY_NAME_CHARS`), the profile bio
/// (`MAX_BIO_CHARS`) and the profile metadata field count
/// (`MAX_PROFILE_FIELDS`). Grouped because each is a plain default-or-parse
/// and [`Config::from_lookup`] has a size budget (AGENTS.md).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when any of the four variables
/// holds a value that is not a whole number.
pub(crate) fn text_length_limits<F>(lookup: &F) -> Result<TextLengthLimits, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(TextLengthLimits {
        media_description: number(
            lookup,
            MAX_MEDIA_DESCRIPTION_VAR,
            DEFAULT_MAX_MEDIA_DESCRIPTION,
        )?,
        display_name_chars: number(
            lookup,
            MAX_DISPLAY_NAME_CHARS_VAR,
            DEFAULT_MAX_DISPLAY_NAME_CHARS,
        )?,
        bio_chars: number(lookup, MAX_BIO_CHARS_VAR, DEFAULT_MAX_BIO_CHARS)?,
        profile_fields: number(lookup, MAX_PROFILE_FIELDS_VAR, DEFAULT_MAX_PROFILE_FIELDS)?,
    })
}

/// The shape [`profile_and_limits`] answers: the instance's profile, its
/// numeric content limits and its per-field text limits, in the order the
/// three groups feed the [`Config`] fields; factored out because a
/// three-value tuple of structs is a very complex type to spell inline.
pub(crate) type Wording = (InstanceProfile, ContentLimits, TextLengthLimits);

/// Reads the three groups that shape what the instance says about itself
/// and how much of it a client may send: the [`instance_profile`], the
/// [`content_limits`] and the [`text_length_limits`] beside it. Grouped
/// because [`Config::from_lookup`] has a size budget (AGENTS.md); see
/// [`Wording`].
///
/// # Errors
///
/// Those of [`content_limits`] and [`text_length_limits`].
pub(crate) fn profile_and_limits<F>(lookup: &F) -> Result<Wording, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok((
        instance_profile(lookup),
        content_limits(lookup)?,
        text_length_limits(lookup)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::test_support::config_with;

    #[test]
    fn reads_custom_max_media_description() {
        let config = config_with(&[(MAX_MEDIA_DESCRIPTION_VAR, "100")]);
        assert_eq!(
            config.max_media_description, 100,
            "custom MAX_MEDIA_DESCRIPTION"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_media_description: 100"),
            "the description limit is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn reads_custom_profile_length_limits() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.max_display_name_chars, 50,
            "default MAX_DISPLAY_NAME_CHARS"
        );
        assert_eq!(defaults.max_bio_chars, 5_000, "default MAX_BIO_CHARS");

        let config = config_with(&[
            (MAX_DISPLAY_NAME_CHARS_VAR, "30"),
            (MAX_BIO_CHARS_VAR, "200"),
        ]);
        assert_eq!(
            config.max_display_name_chars, 30,
            "custom MAX_DISPLAY_NAME_CHARS"
        );
        assert_eq!(config.max_bio_chars, 200, "custom MAX_BIO_CHARS");
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_display_name_chars: 30"),
            "the display-name limit is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("max_bio_chars: 200"),
            "the bio limit is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn reads_custom_max_profile_fields() {
        let defaults = config_with(&[]);
        assert_eq!(defaults.max_profile_fields, 4, "default MAX_PROFILE_FIELDS");

        let config = config_with(&[(MAX_PROFILE_FIELDS_VAR, "2")]);
        assert_eq!(config.max_profile_fields, 2, "custom MAX_PROFILE_FIELDS");
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_profile_fields: 2"),
            "the field-count limit is visible in Debug output: {debug}"
        );
    }
}
