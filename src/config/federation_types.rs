//! The federation, moderation-filter and rate-limiter value types
//! [`Config`] carries: [`Toggle`], [`MrfSettings`],
//! [`RateLimitConfig`], [`FetchLimits`] and [`FetchSettings`], with the
//! shared defaults their `Default` impls spell.

use std::collections::BTreeMap;

#[cfg(doc)]
use super::Config;

use crate::rate_limit::RateLimit;

use super::defaults::{
    DEFAULT_AUTHORIZED_FETCH, DEFAULT_HTTP_ALLOW_PRIVATE_HOSTS, DEFAULT_MAX_FETCHES_PER_ACTIVITY,
    DEFAULT_SIGN_FETCHES, DEFAULT_THREAD_FETCH_DEPTH,
};

#[cfg(any(test, feature = "integration-tests"))]
use super::defaults::DEFAULT_MRF_TRANSPARENCY;

/// A boolean feature toggle held by [`Config`].
///
/// [`Config`] already carries the two registration/SSRF-guard booleans, and
/// Clippy's denied `struct_excessive_bools` (threshold 2) wants more
/// booleans modelled, not piled up — the same reason
/// [`Flag`](crate::models::user::Flag) exists for `users`' columns. A
/// `Toggle` is a plain named wrapper: parsed from the environment by the
/// `boolean` reader below, read by the gated handlers with
/// [`get`](Self::get). `Serialize` writes it as its inner `bool`, so the
/// JSON documents that read a toggle (`NodeInfo`'s `openRegistrations`)
/// see a plain `true`/`false`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Toggle(bool);

impl Toggle {
    /// Wraps a raw boolean as a toggle.
    #[must_use]
    pub const fn new(value: bool) -> Self {
        Self(value)
    }

    /// The raw boolean behind the toggle.
    #[must_use]
    pub const fn get(self) -> bool {
        self.0
    }
}

/// A `bool` is already a valid toggle value; `true.into()` sets one.
impl From<bool> for Toggle {
    fn from(value: bool) -> Self {
        Self(value)
    }
}

/// The moderation-filter (MRF) settings: which policies run over every
/// activity, and how much of what they decide the instance tells the world
/// (see [`crate::federation::mrf`]).
///
/// The three settings ride together because the [`Config`] readers and the
/// [`Debug`] impl below each have an 80-line budget (AGENTS.md) that one
/// field per line would not fit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MrfSettings {
    /// The policy names from `MRF_POLICIES`, in chain order: the fallback
    /// chain configuration, used whenever the config store's `:mrf`
    /// entry carries no `policies` array of its own (see
    /// [`crate::services::runtime_config`]).
    pub policies: Vec<String>,
    /// Whether the policies publish their lists at all, from
    /// `MRF_TRANSPARENCY` (Pleroma's `[:mrf, :transparency]`).
    pub transparency: Toggle,
    /// The host patterns kept out of what is published, from
    /// `MRF_TRANSPARENCY_EXCLUSIONS` (comma list, Pleroma's
    /// `[:mrf, :transparency_exclusions]`).
    pub transparency_exclusions: Vec<String>,
}

impl MrfSettings {
    /// The settings the test configuration writes: no policy without a
    /// setting row, and the moderation published whole.
    #[cfg(any(test, feature = "integration-tests"))]
    pub(crate) const fn test_defaults() -> Self {
        Self {
            policies: Vec::new(),
            transparency: Toggle::new(DEFAULT_MRF_TRANSPARENCY),
            transparency_exclusions: Vec::new(),
        }
    }
}

/// Rate limiter settings, from `RATE_LIMITS_ENABLED`, `RATE_LIMITS` and
/// `TRUST_PROXY_HEADERS`.
///
/// While [`enabled`](Self::enabled) is set — the default — routes that
/// mount a bucket throttle their requests against the windows of
/// [`crate::rate_limit`]; the rest of the struct fixes what they throttle
/// with and whom they count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimitConfig {
    /// Whether limited routes throttle, from `RATE_LIMITS_ENABLED`: while
    /// off they answer uncounted and without `x-ratelimit-*` headers.
    pub enabled: Toggle,
    /// The bucket table, from `RATE_LIMITS`: Pleroma's defaults
    /// (see [`DEFAULT_LIMITS`](crate::rate_limit::DEFAULT_LIMITS)) with the
    /// variable's `bucket=max:window_secs` entries overriding them; a
    /// bucket the table does not name is unlimited.
    pub limits: BTreeMap<String, RateLimit>,
    /// Whether the leftmost `X-Forwarded-For` entry may name the client a
    /// window counts against, from `TRUST_PROXY_HEADERS`: while off (the
    /// default) the socket peer always does.
    pub trust_proxy_headers: Toggle,
}

/// The two numbers that bound how far federation fetching may go, from
/// `THREAD_FETCH_DEPTH` and `MAX_FETCHES_PER_ACTIVITY` (see
/// [`crate::federation::fetch_object`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FetchLimits {
    /// How far up a fetched thread [`federator.backfill`] walks to link a
    /// remote reply to its ancestors; a deeper thread stays linked only as
    /// far as this.
    ///
    /// [`federator.backfill`]: crate::federation::fetch_object::KIND_BACKFILL
    pub thread_fetch_depth: usize,
    /// The most remote objects one incoming activity (or one resolver run)
    /// may dereference; a fetch past the cap fails instead of fanning out
    /// further.
    pub max_fetches_per_activity: usize,
}

/// The settings that govern how this instance fetches and is fetched by
/// peers.
///
/// Grouped because [`Config::from_lookup`] has a size budget
/// (AGENTS.md) and all four describe one outbound `GET`: the
/// `HTTP_ALLOW_PRIVATE_HOSTS`, `SIGN_FETCHES` and `AUTHORIZED_FETCH`
/// toggles and the two numbers of [`FetchLimits`] (`THREAD_FETCH_DEPTH`,
/// `MAX_FETCHES_PER_ACTIVITY`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchSettings {
    /// Whether outbound HTTP may connect to private, loopback and other
    /// reserved addresses, i.e. the SSRF guard is disabled, from
    /// `HTTP_ALLOW_PRIVATE_HOSTS` — and, in the same breath, whether a
    /// `WebFinger` lookup of a `nick@host:port` handle asks `http://` that
    /// authority instead of the spec's `https://` (see
    /// [`crate::federation::fetch_actor`]). Development and tests only.
    pub http_allow_private_hosts: bool,
    /// Whether outgoing actor `GET`s are HTTP-signed with the instance
    /// actor's key, from `SIGN_FETCHES` (see
    /// [`crate::federation::fetch_actor`]).
    pub sign_fetches: Toggle,
    /// Whether incoming `GET`s to the `ActivityPub` actor routes must carry
    /// a valid HTTP signature, from `AUTHORIZED_FETCH` (see
    /// [`crate::federation::authorized_fetch`]).
    pub authorized_fetch: Toggle,
    /// How far federation fetching may go, from `THREAD_FETCH_DEPTH` and
    /// `MAX_FETCHES_PER_ACTIVITY` (see [`crate::federation::fetch_object`]).
    pub limits: FetchLimits,
}

/// The unset-everything shape: each variable's documented default.
impl Default for FetchSettings {
    fn default() -> Self {
        Self {
            http_allow_private_hosts: DEFAULT_HTTP_ALLOW_PRIVATE_HOSTS,
            sign_fetches: DEFAULT_SIGN_FETCHES.into(),
            authorized_fetch: DEFAULT_AUTHORIZED_FETCH.into(),
            limits: FetchLimits::default(),
        }
    }
}

/// The unset-everything shape of the two fetch bounds: the documented
/// defaults of `THREAD_FETCH_DEPTH` and `MAX_FETCHES_PER_ACTIVITY`.
impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            thread_fetch_depth: DEFAULT_THREAD_FETCH_DEPTH,
            max_fetches_per_activity: DEFAULT_MAX_FETCHES_PER_ACTIVITY,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use serde_json::json;

    use super::*;

    use crate::{
        config::{
            Config,
            defaults::{DEFAULT_RATE_LIMITS_ENABLED, DEFAULT_TRUST_PROXY_HEADERS},
            error::ConfigError,
            test_support::{config_with, try_config_with},
            vars::{RATE_LIMITS_ENABLED_VAR, RATE_LIMITS_VAR, TRUST_PROXY_HEADERS_VAR},
        },
        rate_limit,
    };

    #[test]
    fn toggle_serializes_as_its_inner_bool() {
        assert_eq!(
            serde_json::to_value(Toggle::new(true)).unwrap(),
            json!(true)
        );
        assert_eq!(
            serde_json::to_value(Toggle::new(false)).unwrap(),
            json!(false)
        );
    }

    /// The limiter's three settings, at their defaults: on, Pleroma's
    /// bucket table, and no proxy trust.
    #[test]
    fn rate_limit_settings_default() {
        let config = config_with(&[]);
        assert!(
            config.rate_limit.enabled.get(),
            "rate limiting is on by default"
        );
        assert!(
            !config.rate_limit.trust_proxy_headers.get(),
            "X-Forwarded-For is not trusted by default"
        );
        assert_eq!(
            config.rate_limit.limits,
            rate_limit::default_limits(),
            "the table is Pleroma's defaults"
        );
        let authentication = config
            .rate_limit
            .limits
            .get("authentication")
            .copied()
            .unwrap();
        assert_eq!(
            (authentication.max, authentication.window),
            (20, std::time::Duration::from_secs(5)),
            "authentication is 20 per 5 seconds"
        );
        let confirmation = config
            .rate_limit
            .limits
            .get("account_confirmation_resend")
            .copied()
            .unwrap();
        assert_eq!(
            (confirmation.max, confirmation.window),
            (5, std::time::Duration::from_secs(8_600)),
            "account_confirmation_resend is 5 per 8600 seconds"
        );
        assert_eq!(config.rate_limit.limits.len(), 9, "exactly nine buckets");
    }

    /// `RATE_LIMITS` overrides entries of the default table and adds
    /// buckets to it; the untouched defaults stay.
    #[test]
    fn reads_custom_rate_limits() {
        let config = config_with(&[(
            RATE_LIMITS_VAR,
            "authentication=20:5,app_account_creation=5:1800,search=2:2",
        )]);
        assert_eq!(
            config.rate_limit.limits.len(),
            9,
            "no bucket left the table"
        );
        assert_eq!(
            config.rate_limit.limits.get("search").copied().unwrap().max,
            2,
            "the override replaces the default"
        );
        assert_eq!(
            config
                .rate_limit
                .limits
                .get("relations_actions")
                .copied()
                .unwrap()
                .max,
            10,
            "an untouched bucket keeps its default"
        );
    }

    /// A malformed `RATE_LIMITS` entry refuses startup, and the error
    /// names the variable and the reason.
    #[test]
    fn invalid_rate_limits_is_an_error() {
        let err = try_config_with(&[(RATE_LIMITS_VAR, "authentication=20")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidRateLimits(
                "entry 'authentication=20' is not max:window_secs".to_owned()
            ),
            "error variant"
        );
        assert!(err.source().is_none(), "no source");
        assert_eq!(
            err.to_string(),
            "invalid RATE_LIMITS: entry 'authentication=20' is not max:window_secs",
            "message names the variable and the reason"
        );
    }

    /// The two new toggles parse the shared boolean spellings, and a blank
    /// value keeps the default.
    #[test]
    fn rate_limit_toggles_parse_booleans() {
        for var in [RATE_LIMITS_ENABLED_VAR, TRUST_PROXY_HEADERS_VAR] {
            for (value, want) in [
                ("true", true),
                ("TRUE", true),
                (" 1 ", true),
                ("false", false),
                ("FaLsE", false),
                ("0", false),
            ] {
                let config = config_with(&[(var, value)]);
                assert_eq!(
                    toggle_of(&config, var),
                    want,
                    "{var}={value:?} means {want}"
                );
            }
            let default = if var == RATE_LIMITS_ENABLED_VAR {
                DEFAULT_RATE_LIMITS_ENABLED
            } else {
                DEFAULT_TRUST_PROXY_HEADERS
            };
            let config = config_with(&[(var, "")]);
            assert_eq!(
                toggle_of(&config, var),
                default,
                "empty {var} keeps the default"
            );
            let err = try_config_with(&[(var, "maybe")]).unwrap_err();
            assert_eq!(err, ConfigError::InvalidBoolean(var), "error for {var}");
        }
    }

    /// The named toggle's value on this config.
    fn toggle_of(config: &Config, var: &str) -> bool {
        if var == RATE_LIMITS_ENABLED_VAR {
            config.rate_limit.enabled.get()
        } else {
            config.rate_limit.trust_proxy_headers.get()
        }
    }

    /// `Debug` shows the limiter settings; nothing here is secret material.
    #[test]
    fn debug_shows_the_rate_limit_settings() {
        let config = config_with(&[
            (RATE_LIMITS_ENABLED_VAR, "false"),
            (TRUST_PROXY_HEADERS_VAR, "true"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("enabled: Toggle(false)"),
            "the enablement is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("trust_proxy_headers: Toggle(true)"),
            "the proxy trust is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("limits:"),
            "the bucket table is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("\"authentication\": RateLimit"),
            "and it shows the buckets by name: {debug}"
        );
    }

    /// The test configuration runs with the limiter off, so a suite never
    /// trips its own budget, and the table still carries the defaults.
    #[test]
    fn for_tests_disables_rate_limiting() {
        let config = Config::for_tests();
        assert!(
            !config.rate_limit.enabled.get(),
            "tests answer unthrottled unless they read the var themselves"
        );
        assert_eq!(
            config.rate_limit.limits,
            rate_limit::default_limits(),
            "the table keeps the defaults for tests that opt in"
        );
        assert!(
            !config.rate_limit.trust_proxy_headers.get(),
            "tests are unproxied by default"
        );
    }
}
