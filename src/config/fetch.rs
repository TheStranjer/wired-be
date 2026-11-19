//! Reads the settings that govern how this instance fetches and is fetched
//! by peers into [`FetchSettings`] and its [`FetchLimits`].

#[cfg(doc)]
use super::Config;

use super::{
    defaults::{
        DEFAULT_AUTHORIZED_FETCH, DEFAULT_HTTP_ALLOW_PRIVATE_HOSTS,
        DEFAULT_MAX_FETCHES_PER_ACTIVITY, DEFAULT_SIGN_FETCHES, DEFAULT_THREAD_FETCH_DEPTH,
    },
    error::ConfigError,
    federation_types::{FetchLimits, FetchSettings},
    parse::{boolean, number},
    vars::{
        AUTHORIZED_FETCH_VAR, HTTP_ALLOW_PRIVATE_HOSTS_VAR, MAX_FETCHES_PER_ACTIVITY_VAR,
        SIGN_FETCHES_VAR, THREAD_FETCH_DEPTH_VAR,
    },
};

/// Reads the settings that govern how this instance fetches and is
/// fetched by peers: whether the client may reach private/loopback hosts
/// (`HTTP_ALLOW_PRIVATE_HOSTS`), whether it signs its outgoing actor
/// `GET`s (`SIGN_FETCHES`), whether it demands signatures on incoming
/// `ActivityPub` `GET`s (`AUTHORIZED_FETCH`), and the two numbers that
/// bound how far fetching may go (`THREAD_FETCH_DEPTH`,
/// `MAX_FETCHES_PER_ACTIVITY`, read through [`fetch_limits`]). Grouped
/// into one [`FetchSettings`] because each is a plain default-or-parse
/// and [`Config::from_lookup`] has a size budget (AGENTS.md) — and
/// because the settings belong together on the wire: they are the shape
/// of this instance's outbound `GET`.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] for whichever boolean variable
/// holds a value that is neither spelling, and
/// [`ConfigError::InvalidNumber`] for whichever fetch-limit variable holds
/// a value that is not a whole number.
pub(crate) fn fetch_settings<F>(lookup: &F) -> Result<FetchSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(FetchSettings {
        http_allow_private_hosts: boolean(
            lookup,
            HTTP_ALLOW_PRIVATE_HOSTS_VAR,
            DEFAULT_HTTP_ALLOW_PRIVATE_HOSTS,
        )?,
        sign_fetches: boolean(lookup, SIGN_FETCHES_VAR, DEFAULT_SIGN_FETCHES)?.into(),
        authorized_fetch: boolean(lookup, AUTHORIZED_FETCH_VAR, DEFAULT_AUTHORIZED_FETCH)?.into(),
        limits: fetch_limits(lookup)?,
    })
}

/// Reads the two numbers that bound how far federation fetching may go:
/// how deep the [`federator.backfill`] job walks a thread
/// (`THREAD_FETCH_DEPTH`) and how many object dereferences one run may
/// trigger (`MAX_FETCHES_PER_ACTIVITY`).
///
/// [`federator.backfill`]: crate::federation::fetch_object::KIND_BACKFILL
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] for whichever variable holds a
/// value that is not a whole number.
fn fetch_limits<F>(lookup: &F) -> Result<FetchLimits, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(FetchLimits {
        thread_fetch_depth: number(lookup, THREAD_FETCH_DEPTH_VAR, DEFAULT_THREAD_FETCH_DEPTH)?,
        max_fetches_per_activity: number(
            lookup,
            MAX_FETCHES_PER_ACTIVITY_VAR,
            DEFAULT_MAX_FETCHES_PER_ACTIVITY,
        )?,
    })
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    use crate::config::test_support::{config_with, try_config_with};

    #[test]
    fn fetch_toggles_parse_booleans() {
        for var in [SIGN_FETCHES_VAR, AUTHORIZED_FETCH_VAR] {
            for (value, want) in [
                ("true", true),
                ("TRUE", true),
                (" 1 ", true),
                ("false", false),
                ("FaLsE", false),
                ("0", false),
            ] {
                let config = config_with(&[(var, value)]);
                let got = match var {
                    SIGN_FETCHES_VAR => config.fetch.sign_fetches,
                    _ => config.fetch.authorized_fetch,
                };
                assert_eq!(got.get(), want, "{var}={value:?} means {want}");
            }
        }
    }

    #[test]
    fn fetch_toggle_defaults_apply_when_unset_or_empty() {
        let unset = config_with(&[]);
        assert!(
            unset.fetch.sign_fetches.get() && !unset.fetch.authorized_fetch.get(),
            "unset toggles sit at their defaults"
        );
        let config = config_with(&[(SIGN_FETCHES_VAR, ""), (AUTHORIZED_FETCH_VAR, "")]);
        assert!(
            config.fetch.sign_fetches.get(),
            "empty SIGN_FETCHES stays at the default"
        );
        assert!(
            !config.fetch.authorized_fetch.get(),
            "empty AUTHORIZED_FETCH stays at the default"
        );
    }

    #[test]
    fn invalid_fetch_toggles_are_errors() {
        for var in [SIGN_FETCHES_VAR, AUTHORIZED_FETCH_VAR] {
            let err = try_config_with(&[(var, "maybe")]).unwrap_err();
            assert_eq!(err, ConfigError::InvalidBoolean(var), "error for {var}");
            assert!(
                err.to_string().starts_with(&format!("invalid {var}: ")),
                "message names {var}"
            );
        }
    }

    #[test]
    fn debug_shows_the_fetch_toggles() {
        let config = config_with(&[(SIGN_FETCHES_VAR, "false"), (AUTHORIZED_FETCH_VAR, "true")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("sign_fetches: Toggle(false)"),
            "the signed-fetch toggle is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("authorized_fetch: Toggle(true)"),
            "the authorized-fetch toggle is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn fetch_bounds_default_to_twenty() {
        let config = config_with(&[]);
        assert_eq!(
            config.fetch.limits.thread_fetch_depth, 20,
            "default THREAD_FETCH_DEPTH is twenty ancestors"
        );
        assert_eq!(
            config.fetch.limits.max_fetches_per_activity, 20,
            "default MAX_FETCHES_PER_ACTIVITY is twenty dereferences"
        );
    }

    #[test]
    fn fetch_bounds_parse_numbers() {
        let config = config_with(&[
            (THREAD_FETCH_DEPTH_VAR, "8"),
            (MAX_FETCHES_PER_ACTIVITY_VAR, "3"),
        ]);
        assert_eq!(
            config.fetch.limits.thread_fetch_depth, 8,
            "custom THREAD_FETCH_DEPTH"
        );
        assert_eq!(
            config.fetch.limits.max_fetches_per_activity, 3,
            "custom MAX_FETCHES_PER_ACTIVITY"
        );
    }

    #[test]
    fn debug_shows_the_fetch_bounds() {
        let config = config_with(&[(THREAD_FETCH_DEPTH_VAR, "6")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("thread_fetch_depth: 6"),
            "the thread-fetch depth is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("max_fetches_per_activity: 20"),
            "the per-activity fetch cap is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn http_allow_private_hosts_parses_booleans() {
        for (value, want) in [
            ("true", true),
            ("TRUE", true),
            (" 1 ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", false),
        ] {
            let config = config_with(&[(HTTP_ALLOW_PRIVATE_HOSTS_VAR, value)]);
            assert_eq!(
                config.fetch.http_allow_private_hosts, want,
                "HTTP_ALLOW_PRIVATE_HOSTS={value:?} means {want}"
            );
        }
    }

    #[test]
    fn invalid_http_allow_private_hosts_is_an_error() {
        let err = try_config_with(&[(HTTP_ALLOW_PRIVATE_HOSTS_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(HTTP_ALLOW_PRIVATE_HOSTS_VAR),
            "error variant"
        );
        assert!(err.source().is_none(), "no source");
        assert!(
            err.to_string()
                .starts_with(&format!("invalid {HTTP_ALLOW_PRIVATE_HOSTS_VAR}: ")),
            "message names the variable"
        );
    }

    #[test]
    fn debug_shows_http_allow_private_hosts() {
        let config = config_with(&[(HTTP_ALLOW_PRIVATE_HOSTS_VAR, "true")]);
        assert!(
            format!("{config:?}").contains("http_allow_private_hosts: true"),
            "the guard flag is visible in Debug output"
        );
    }
}
