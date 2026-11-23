//! Reads the per-instance federation switches, the MRF fallback chain and
//! the rate limiter settings that [`Config`] keeps
//! beside the fetch group.

use crate::rate_limit;

#[cfg(doc)]
use super::Config;

use super::{
    defaults::{
        DEFAULT_ACCEPT_UNSOLICITED_PUBLIC, DEFAULT_FEDERATION_ENABLED, DEFAULT_MRF_TRANSPARENCY,
        DEFAULT_RATE_LIMITS_ENABLED, DEFAULT_TRUST_PROXY_HEADERS, DEFAULT_TRUSTED_PROXY_HOPS,
        DEFAULT_UNREACHABLE_GRACE_SECS,
    },
    error::ConfigError,
    federation_types::{MrfSettings, RateLimitConfig, Toggle},
    parse::{boolean, comma_list, number},
    vars::{
        ACCEPT_UNSOLICITED_PUBLIC_VAR, FEDERATION_ENABLED_VAR, MRF_POLICIES_VAR,
        MRF_TRANSPARENCY_EXCLUSIONS_VAR, MRF_TRANSPARENCY_VAR, RATE_LIMITS_ENABLED_VAR,
        RATE_LIMITS_VAR, TRUST_PROXY_HEADERS_VAR, TRUSTED_PROXY_HOPS_VAR,
        UNREACHABLE_GRACE_SECS_VAR,
    },
};

/// Reads `FEDERATION_ENABLED`: whether this instance federates at all,
/// [`DEFAULT_FEDERATION_ENABLED`] when unset or blank. Read as its own
/// setting because the inbox routes and the delivery enqueuer both gate on
/// it (see [`crate::api::activitypub`] and
/// [`crate::federation::delivery`]).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] when the variable holds a value
/// that is neither spelling.
pub(crate) fn federation_enabled<F>(lookup: &F) -> Result<Toggle, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(boolean(lookup, FEDERATION_ENABLED_VAR, DEFAULT_FEDERATION_ENABLED)?.into())
}

/// Reads `ACCEPT_UNSOLICITED_PUBLIC`, [`DEFAULT_ACCEPT_UNSOLICITED_PUBLIC`]
/// when unset: the ingest gate on public and unlisted posts from authors
/// nobody follows (see [`crate::services::ingest`]).
pub(crate) fn accept_unsolicited_public<F>(lookup: &F) -> Result<Toggle, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(boolean(
        lookup,
        ACCEPT_UNSOLICITED_PUBLIC_VAR,
        DEFAULT_ACCEPT_UNSOLICITED_PUBLIC,
    )?
    .into())
}

/// Reads `UNREACHABLE_GRACE_SECS`, [`DEFAULT_UNREACHABLE_GRACE_SECS`] when
/// unset: how long a host marked unreachable stays deliverable, so
/// [`crate::federation::publisher`] can hold a freshly marked peer in the
/// target set for one more retry cycle.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when the variable holds a value
/// that is not a whole number.
pub(crate) fn unreachable_grace_secs<F>(lookup: &F) -> Result<usize, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    number(
        lookup,
        UNREACHABLE_GRACE_SECS_VAR,
        DEFAULT_UNREACHABLE_GRACE_SECS,
    )
}

/// Reads the MRF group: the `MRF_POLICIES` fallback chain (empty when unset
/// or blank), the `MRF_TRANSPARENCY` switch on publishing the moderation
/// lists, and the `MRF_TRANSPARENCY_EXCLUSIONS` host patterns kept out of
/// them (see [`crate::federation::mrf`]). Grouped because every [`Config`]
/// reader works inside a line budget (AGENTS.md).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] when `MRF_TRANSPARENCY` holds a
/// value that is neither spelling.
pub(crate) fn mrf_settings<F>(lookup: &F) -> Result<MrfSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(MrfSettings {
        policies: comma_list(&lookup(MRF_POLICIES_VAR).unwrap_or_default()),
        transparency: boolean(lookup, MRF_TRANSPARENCY_VAR, DEFAULT_MRF_TRANSPARENCY)?.into(),
        transparency_exclusions: comma_list(
            &lookup(MRF_TRANSPARENCY_EXCLUSIONS_VAR).unwrap_or_default(),
        ),
    })
}

/// Reads the rate limiter settings (see [`crate::rate_limit`]): whether
/// limited routes throttle at all, the bucket table — Pleroma's defaults
/// with the `RATE_LIMITS` overrides merged in — and whether
/// `X-Forwarded-For` may name the client.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] when a toggle variable holds a
/// value that is neither spelling, and [`ConfigError::InvalidRateLimits`]
/// when `RATE_LIMITS` holds a malformed entry.
pub(crate) fn rate_limit_settings<F>(lookup: &F) -> Result<RateLimitConfig, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let enabled = boolean(lookup, RATE_LIMITS_ENABLED_VAR, DEFAULT_RATE_LIMITS_ENABLED)?.into();
    let trust_proxy_headers =
        boolean(lookup, TRUST_PROXY_HEADERS_VAR, DEFAULT_TRUST_PROXY_HEADERS)?.into();
    let trusted_proxy_hops = number(lookup, TRUSTED_PROXY_HOPS_VAR, DEFAULT_TRUSTED_PROXY_HOPS)?;
    let mut limits = rate_limit::default_limits();
    if let Some(text) = lookup(RATE_LIMITS_VAR).filter(|text| !text.trim().is_empty()) {
        limits.extend(rate_limit::parse_limits(&text).map_err(ConfigError::InvalidRateLimits)?);
    }
    Ok(RateLimitConfig {
        enabled,
        limits,
        trust_proxy_headers,
        trusted_proxy_hops,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::test_support::{config_with, try_config_with};

    /// `MRF_POLICIES` is unset by default: an instance runs no policy
    /// chain until a setting or the variable enables one.
    #[test]
    fn mrf_policies_default_to_the_empty_chain() {
        let config = config_with(&[]);
        assert!(
            config.mrf.policies.is_empty(),
            "no MRF policies without MRF_POLICIES or the `mrf` setting"
        );
    }

    /// `MRF_POLICIES` is a comma list: trimmed, empties dropped.
    #[test]
    fn mrf_policies_parses_a_comma_list() {
        let config = config_with(&[(MRF_POLICIES_VAR, "simple, keyword ,")]);
        assert_eq!(
            config.mrf.policies,
            vec!["simple".to_owned(), "keyword".to_owned()],
            "MRF_POLICIES lists the chain in order"
        );
    }

    /// `Debug` renders the MRF group (through the `Debug` chain's
    /// federation helper, beside the toggles the helper also prints).
    #[test]
    fn debug_shows_the_mrf_policies() {
        let config = config_with(&[(MRF_POLICIES_VAR, "noop,simple")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains(r#"policies: ["noop", "simple"]"#),
            "the policy-name list is visible in Debug output: {debug}"
        );
    }

    /// `MRF_TRANSPARENCY` is on by default: a moderating instance says so.
    #[test]
    fn mrf_transparency_is_on_by_default() {
        let config = config_with(&[]);
        assert!(
            config.mrf.transparency.get(),
            "the lists an instance moderates by are published unless its operator \
             says otherwise (Pleroma's `[:mrf, :transparency]` default)"
        );
        assert!(
            config.mrf.transparency_exclusions.is_empty(),
            "and nothing is excluded without MRF_TRANSPARENCY_EXCLUSIONS"
        );
    }

    /// `MRF_TRANSPARENCY` accepts the same two spellings as every other
    /// toggle.
    #[test]
    fn mrf_transparency_parses_booleans() {
        for (value, want) in [
            ("true", true),
            ("FALSE", false),
            (" 0 ", false),
            ("1", true),
            ("", true),
        ] {
            let config = config_with(&[(MRF_TRANSPARENCY_VAR, value)]);
            assert_eq!(
                config.mrf.transparency.get(),
                want,
                "MRF_TRANSPARENCY={value:?} means {want} (unset publishes)"
            );
        }
    }

    /// A value that is neither spelling refuses startup rather than
    /// silently publishing (or hiding) the lists.
    #[test]
    fn invalid_mrf_transparency_is_an_error() {
        let err = try_config_with(&[(MRF_TRANSPARENCY_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(MRF_TRANSPARENCY_VAR),
            "error variant"
        );
        assert!(
            err.to_string()
                .starts_with(&format!("invalid {MRF_TRANSPARENCY_VAR}: ")),
            "message names the variable"
        );
    }

    /// `MRF_TRANSPARENCY_EXCLUSIONS` is a comma list: trimmed, empties
    /// dropped, order kept.
    #[test]
    fn mrf_transparency_exclusions_parse_a_comma_list() {
        let config = config_with(&[(
            MRF_TRANSPARENCY_EXCLUSIONS_VAR,
            "*.partner.example, press.example ,",
        )]);
        assert_eq!(
            config.mrf.transparency_exclusions,
            vec!["*.partner.example".to_owned(), "press.example".to_owned()],
            "the patterns arrive as written, ready to match the lists with"
        );
    }

    /// `Debug` renders both transparency settings inside the MRF group:
    /// the switch and the hidden patterns (which are not secret — the point
    /// of the flag is that they are hidden from *anonymous* readers).
    #[test]
    fn debug_shows_the_mrf_transparency_settings() {
        let config = config_with(&[
            (MRF_TRANSPARENCY_VAR, "false"),
            (MRF_TRANSPARENCY_EXCLUSIONS_VAR, "press.example"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("transparency: Toggle(false)"),
            "the switch is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains(r#"transparency_exclusions: ["press.example"]"#),
            "and so is the hidden-pattern list: {debug}"
        );
    }

    /// `FEDERATION_ENABLED` keeps its true default when the variable is
    /// unset: an instance federates unless its operator turns it off.
    #[test]
    fn federation_is_enabled_by_default() {
        let config = config_with(&[]);
        assert!(
            config.federation_enabled.get(),
            "federation is on by default (Pleroma's `[:federator, :enabled]`)"
        );
    }

    #[test]
    fn federation_enabled_parses_booleans() {
        for (value, want) in [
            ("true", true),
            ("TRUE", true),
            (" 1 ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", true),
        ] {
            let config = config_with(&[(FEDERATION_ENABLED_VAR, value)]);
            assert_eq!(
                config.federation_enabled.get(),
                want,
                "FEDERATION_ENABLED={value:?} means {want} (unset federates)"
            );
        }
    }

    #[test]
    fn invalid_federation_enabled_is_an_error() {
        let err = try_config_with(&[(FEDERATION_ENABLED_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(FEDERATION_ENABLED_VAR),
            "error variant"
        );
        assert!(
            err.to_string()
                .starts_with(&format!("invalid {FEDERATION_ENABLED_VAR}: ")),
            "message names the variable"
        );
    }

    #[test]
    fn debug_shows_federation_enabled() {
        let config = config_with(&[(FEDERATION_ENABLED_VAR, "false")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("federation_enabled: Toggle(false)"),
            "the federation toggle is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn accept_unsolicited_public_parses_booleans() {
        for (value, want) in [
            ("true", true),
            ("TRUE", true),
            (" 1 ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", true),
        ] {
            let config = config_with(&[(ACCEPT_UNSOLICITED_PUBLIC_VAR, value)]);
            assert_eq!(
                config.accept_unsolicited_public.get(),
                want,
                "ACCEPT_UNSOLICITED_PUBLIC={value:?} means {want} (unset accepts)"
            );
        }
    }

    #[test]
    fn invalid_accept_unsolicited_public_is_an_error() {
        let err = try_config_with(&[(ACCEPT_UNSOLICITED_PUBLIC_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(ACCEPT_UNSOLICITED_PUBLIC_VAR),
            "error variant"
        );
    }

    #[test]
    fn debug_shows_accept_unsolicited_public() {
        let config = config_with(&[(ACCEPT_UNSOLICITED_PUBLIC_VAR, "false")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("accept_unsolicited_public: Toggle(false)"),
            "the ingest gate toggle is visible in Debug output: {debug}"
        );
    }

    /// The unreachable-host grace period: zero by default, so every marked
    /// host counts, the environment overriding it, and `Debug` showing the
    /// value.
    #[test]
    fn reads_custom_unreachable_grace_secs() {
        let defaults = config_with(&[]);
        assert_eq!(
            defaults.unreachable_grace_secs, 0,
            "default UNREACHABLE_GRACE_SECS"
        );

        let config = config_with(&[(UNREACHABLE_GRACE_SECS_VAR, "3600")]);
        assert_eq!(
            config.unreachable_grace_secs, 3600,
            "custom UNREACHABLE_GRACE_SECS"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("unreachable_grace_secs: 3600"),
            "the grace period is visible in Debug output: {debug}"
        );
    }

    /// An empty `UNREACHABLE_GRACE_SECS` is the unset state, and anything
    /// that is not a whole number refuses startup.
    #[test]
    fn invalid_unreachable_grace_secs_is_an_error() {
        let blank = config_with(&[(UNREACHABLE_GRACE_SECS_VAR, "")]);
        assert_eq!(
            blank.unreachable_grace_secs, 0,
            "an empty value is the default, not an error"
        );

        let err = try_config_with(&[(UNREACHABLE_GRACE_SECS_VAR, "an-hour")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidNumber(UNREACHABLE_GRACE_SECS_VAR),
            "error variant"
        );
    }
}
