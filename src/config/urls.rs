//! The shared readers for the location settings.
//!
//! One directory variable through `directory`, the media URL prefix, and
//! the frontend origin — each factored out of [`Config::from_lookup`],
//! which has a size budget (AGENTS.md).

use std::path::PathBuf;

#[cfg(doc)]
use super::Config;

use super::{
    defaults::DEFAULT_MEDIA_URL_PATH,
    parse::{trimmed, trimmed_url},
    vars::{FRONTEND_BASE_URL_VAR, MEDIA_BASE_URL_VAR},
};

/// Reads one directory-location variable (`UPLOADS_DIR`, `EMOJI_DIR`,
/// `FRONTENDS_DIR`): the configured value trimmed, or `default` when it is
/// unset or blank. Factored out of [`Config::from_lookup`], which has a
/// size budget (AGENTS.md), because each path keeps its own field there.
pub(crate) fn directory<F>(lookup: &F, var: &'static str, default: &str) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    trimmed(lookup, var).map_or_else(|| PathBuf::from(default), PathBuf::from)
}

/// Reads `MEDIA_BASE_URL`: the URL prefix media is served under, trimmed
/// and with its trailing slash dropped, or `base_url` — the normalized
/// origin — with the default media path hung onto it when unset or blank.
/// Factored out of [`endpoints`], which reads it last, off the origin it
/// defaults onto.
pub(crate) fn media_base_url<F>(lookup: &F, base_url: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    trimmed_url(lookup, MEDIA_BASE_URL_VAR)
        .unwrap_or_else(|| format!("{base_url}{DEFAULT_MEDIA_URL_PATH}"))
}

/// Reads `FRONTEND_BASE_URL`: the origin the frontend pages are served
/// from, trimmed and with its trailing slashes dropped, or `base_url` — the
/// normalized origin — when unset or blank. The `/notice/{id}` browser
/// redirect (a later slice) hangs its path onto this value, so it gets the
/// same trimmed, slash-stripped reading as the media base URL and the media
/// proxy's base URL: nothing unusable ever reaches a `Location` header.
/// Factored out of [`Config::from_lookup`], which has a size budget
/// (AGENTS.md), because it defaults onto the origin `endpoints` normalizes.
pub(crate) fn frontend_base_url<F>(lookup: &F, base_url: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    trimmed_url(lookup, FRONTEND_BASE_URL_VAR).unwrap_or_else(|| base_url.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::{
        test_support::{ORIGIN, config_with},
        vars::{BASE_URL_VAR, EMOJI_DIR_VAR, UPLOADS_DIR_VAR},
    };

    #[test]
    fn media_base_url_is_trimmed_and_defaults_to_the_base_url() {
        for (value, want) in [
            (
                " https://cdn.example/media/// ",
                "https://cdn.example/media",
            ),
            ("", "https://social.example/media"),
            ("/", "https://social.example/media"),
            ("https://cdn.example/media", "https://cdn.example/media"),
        ] {
            let config = config_with(&[(MEDIA_BASE_URL_VAR, value)]);
            assert_eq!(
                config.media_base_url, want,
                "MEDIA_BASE_URL={value:?} means {want}"
            );
        }
        let with_port = config_with(&[(BASE_URL_VAR, "http://localhost:4000")]);
        assert_eq!(
            with_port.media_base_url, "http://localhost:4000/media",
            "the default follows BASE_URL, port included"
        );
    }

    /// `FRONTEND_BASE_URL`: unset or blank falls back to the normalized
    /// `BASE_URL`, a set value wins with its surrounding whitespace and
    /// trailing slashes stripped (the reading shared with `MEDIA_BASE_URL`
    /// and the media proxy's base URL, so nothing unusable can reach a
    /// `Location` header), and the field shows in Debug output.
    #[test]
    fn frontend_base_url_is_trimmed_and_defaults_to_the_base_url() {
        let unset = config_with(&[]);
        assert_eq!(
            unset.frontend_base_url, unset.base_url,
            "unset FRONTEND_BASE_URL equals BASE_URL"
        );
        assert_eq!(
            unset.frontend_base_url, ORIGIN,
            "the default is the normalized origin"
        );
        for (value, want) in [
            ("https://frontend.example", "https://frontend.example"),
            (" https://frontend.example/// ", "https://frontend.example"),
            ("https://frontend.example/", "https://frontend.example"),
            ("", ORIGIN),
            ("/", ORIGIN),
        ] {
            let config = config_with(&[(FRONTEND_BASE_URL_VAR, value)]);
            assert_eq!(
                config.frontend_base_url, want,
                "FRONTEND_BASE_URL={value:?} means {want}"
            );
        }
        let with_port = config_with(&[(BASE_URL_VAR, "http://localhost:4000")]);
        assert_eq!(
            with_port.frontend_base_url, "http://localhost:4000",
            "the default follows BASE_URL, port included"
        );
        let debug = format!(
            "{:?}",
            config_with(&[(FRONTEND_BASE_URL_VAR, " https://frontend.example/ ")])
        );
        assert!(
            debug.contains(r#"frontend_base_url: "https://frontend.example""#),
            "frontend_base_url is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn uploads_dir_is_trimmed_and_defaults_to_uploads() {
        for (value, want) in [
            ("  /var/uploads  ", "/var/uploads"),
            ("uploads", "uploads"),
            ("", "./uploads"),
            ("   ", "./uploads"),
        ] {
            let config = config_with(&[(UPLOADS_DIR_VAR, value)]);
            assert_eq!(
                config.uploads_dir,
                PathBuf::from(want),
                "UPLOADS_DIR={value:?} means {want}"
            );
        }
    }

    #[test]
    fn emoji_dir_is_trimmed_and_defaults_to_emoji() {
        for (value, want) in [
            ("  /var/emoji  ", "/var/emoji"),
            ("emoji", "emoji"),
            ("", "./emoji"),
            ("   ", "./emoji"),
        ] {
            let config = config_with(&[(EMOJI_DIR_VAR, value)]);
            assert_eq!(
                config.emoji_dir,
                PathBuf::from(want),
                "EMOJI_DIR={value:?} means {want}"
            );
        }
    }

    #[test]
    fn debug_shows_the_upload_storage_settings() {
        let config = config_with(&[(UPLOADS_DIR_VAR, "/srv/uploads")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("uploads_dir: \"/srv/uploads\""),
            "the uploads directory is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("media_base_url: \"https://social.example/media\""),
            "the media URL prefix is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("emoji_dir: \"./emoji\""),
            "the emoji packs directory is visible in Debug output: {debug}"
        );
    }
}
