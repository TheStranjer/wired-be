//! Reads the seven settings that configure the media proxy and the two
//! rich-media card settings.

#[cfg(doc)]
use super::Config;

use super::{
    defaults::{
        DEFAULT_MEDIA_PROXY_CACHE_LIMIT, DEFAULT_MEDIA_PROXY_ENABLED,
        DEFAULT_MEDIA_PROXY_MAX_BYTES, DEFAULT_RICH_MEDIA_IGNORE_HOSTS, DEFAULT_RICH_MEDIA_REMOTE,
        MIN_MEDIA_PROXY_SECRET_CHARS,
    },
    error::ConfigError,
    federation_types::Toggle,
    integrations::MediaProxyConfig,
    parse::{boolean, comma_list, number, trimmed, trimmed_url},
    vars::{
        MEDIA_PROXY_BASE_URL_VAR, MEDIA_PROXY_BLACKLIST_VAR, MEDIA_PROXY_CACHE_LIMIT_VAR,
        MEDIA_PROXY_ENABLED_VAR, MEDIA_PROXY_MAX_BYTES_VAR, MEDIA_PROXY_PURGE_URL_VAR,
        MEDIA_PROXY_SECRET_VAR, RICH_MEDIA_IGNORE_HOSTS_VAR, RICH_MEDIA_REMOTE_VAR,
    },
};

/// Reads the seven settings that configure the media proxy
/// (see [`crate::services::media_proxy`]). Grouped because each is a
/// plain default-or-parse and [`Config::from_lookup`] has a size budget
/// (AGENTS.md); enabling the proxy without a long-enough secret is an
/// error, since every proxy link would be unsignable (and the route
/// unservable) without it.
pub(crate) fn media_proxy_settings<F>(lookup: &F) -> Result<MediaProxyConfig, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let enabled = boolean(lookup, MEDIA_PROXY_ENABLED_VAR, DEFAULT_MEDIA_PROXY_ENABLED)?;
    let secret = trimmed(lookup, MEDIA_PROXY_SECRET_VAR);
    if enabled {
        let secret = secret
            .as_deref()
            .ok_or(ConfigError::Missing(MEDIA_PROXY_SECRET_VAR))?;
        if secret.chars().count() < MIN_MEDIA_PROXY_SECRET_CHARS {
            return Err(ConfigError::ShortMediaProxySecret(MEDIA_PROXY_SECRET_VAR));
        }
    }
    let base_url = trimmed_url(lookup, MEDIA_PROXY_BASE_URL_VAR);
    let max_bytes = number(
        lookup,
        MEDIA_PROXY_MAX_BYTES_VAR,
        DEFAULT_MEDIA_PROXY_MAX_BYTES,
    )?;
    let blacklist = comma_list(&lookup(MEDIA_PROXY_BLACKLIST_VAR).unwrap_or_default());
    let cache_limit = number(
        lookup,
        MEDIA_PROXY_CACHE_LIMIT_VAR,
        DEFAULT_MEDIA_PROXY_CACHE_LIMIT,
    )?;
    let purge_url = trimmed_url(lookup, MEDIA_PROXY_PURGE_URL_VAR);
    Ok(MediaProxyConfig {
        enabled,
        secret,
        base_url,
        max_bytes,
        blacklist,
        cache_limit,
        purge_url,
    })
}

/// Reads `RICH_MEDIA_IGNORE_HOSTS`: the comma-separated list of hosts rich
/// media card fetching never visits, empty when unset or blank — the same
/// trimmed-list reading as the media proxy's blacklist.
pub(crate) fn rich_media_ignore_hosts<F>(lookup: &F) -> Vec<String>
where
    F: Fn(&str) -> Option<String>,
{
    comma_list(
        &lookup(RICH_MEDIA_IGNORE_HOSTS_VAR)
            .unwrap_or_else(|| DEFAULT_RICH_MEDIA_IGNORE_HOSTS.to_owned()),
    )
}

/// Reads `RICH_MEDIA_REMOTE`: whether preview cards are fetched for the
/// remote URLs statuses link to, off by default.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] when the variable holds a value
/// that is neither spelling.
pub(crate) fn rich_media_remote<F>(lookup: &F) -> Result<Toggle, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(boolean(lookup, RICH_MEDIA_REMOTE_VAR, DEFAULT_RICH_MEDIA_REMOTE)?.into())
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    use crate::config::test_support::{config_with, try_config_with};

    #[test]
    fn media_proxy_defaults_to_disabled_passthrough() {
        let config = config_with(&[]);
        assert!(!config.media_proxy.enabled, "the proxy is off by default");
        assert_eq!(
            config.media_proxy.secret, None,
            "no signing secret unless configured"
        );
        assert_eq!(
            config.media_proxy.base_url, None,
            "proxy links default to BASE_URL"
        );
        assert_eq!(
            config.media_proxy.max_bytes, 50_000_000,
            "default MEDIA_PROXY_MAX_BYTES"
        );
        assert!(
            config.media_proxy.blacklist.is_empty(),
            "nothing is blacklisted by default"
        );
    }

    #[test]
    fn reads_custom_media_proxy_settings() {
        let config = config_with(&[
            (MEDIA_PROXY_ENABLED_VAR, "true"),
            (
                MEDIA_PROXY_SECRET_VAR,
                "  a-secret-of-exactly-32-characters  ",
            ),
            (MEDIA_PROXY_BASE_URL_VAR, " https://cdn.example/ "),
            (MEDIA_PROXY_MAX_BYTES_VAR, "1024"),
            (
                MEDIA_PROXY_BLACKLIST_VAR,
                "evil.example, cdn-only.example ,",
            ),
        ]);
        assert!(config.media_proxy.enabled, "custom MEDIA_PROXY_ENABLED");
        assert_eq!(
            config.media_proxy.secret.as_deref(),
            Some("a-secret-of-exactly-32-characters"),
            "the secret is trimmed"
        );
        assert_eq!(
            config.media_proxy.base_url.as_deref(),
            Some("https://cdn.example"),
            "MEDIA_PROXY_BASE_URL is trimmed and loses its trailing slash"
        );
        assert_eq!(config.media_proxy.max_bytes, 1024, "custom byte cap");
        assert_eq!(
            config.media_proxy.blacklist,
            vec!["evil.example".to_owned(), "cdn-only.example".to_owned()],
            "the blacklist is a trimmed comma list"
        );
    }

    #[test]
    fn media_proxy_cache_ledger_defaults() {
        let config = config_with(&[]);
        assert_eq!(
            config.media_proxy.cache_limit, 100_000,
            "default MEDIA_PROXY_CACHE_LIMIT"
        );
        assert_eq!(
            config.media_proxy.purge_url, None,
            "no PURGE forwarding until the operator names a cache"
        );
        for blank in ["", "   ", "  /  "] {
            let blank_purge = config_with(&[(MEDIA_PROXY_PURGE_URL_VAR, blank)]);
            assert_eq!(
                blank_purge.media_proxy.purge_url, None,
                "MEDIA_PROXY_PURGE_URL={blank:?} means unset"
            );
        }
    }

    #[test]
    fn reads_the_media_proxy_cache_limit_and_purge_url() {
        let config = config_with(&[
            (MEDIA_PROXY_CACHE_LIMIT_VAR, "500"),
            (MEDIA_PROXY_PURGE_URL_VAR, " http://cache.example/ "),
        ]);
        assert_eq!(config.media_proxy.cache_limit, 500, "custom row cap");
        assert_eq!(
            config.media_proxy.purge_url.as_deref(),
            Some("http://cache.example"),
            "MEDIA_PROXY_PURGE_URL is trimmed and loses its trailing slash"
        );
        let debug = format!("{:?}", config.media_proxy);
        assert!(
            debug.contains("cache_limit: 500")
                && debug.contains("purge_url: Some(\"http://cache.example\")"),
            "both ledger settings are visible in Debug output: {debug}"
        );
        let err = try_config_with(&[(MEDIA_PROXY_CACHE_LIMIT_VAR, "many")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidNumber(MEDIA_PROXY_CACHE_LIMIT_VAR),
            "a non-number cap is refused"
        );
    }

    #[test]
    fn media_proxy_secret_empty_string_is_unset() {
        let err = try_config_with(&[
            (MEDIA_PROXY_ENABLED_VAR, "true"),
            (MEDIA_PROXY_SECRET_VAR, "   "),
        ])
        .unwrap_err();
        assert_eq!(
            err,
            ConfigError::Missing(MEDIA_PROXY_SECRET_VAR),
            "an empty secret is a missing secret when enabled"
        );
    }

    #[test]
    fn enabling_the_media_proxy_without_a_secret_is_an_error() {
        let err = try_config_with(&[(MEDIA_PROXY_ENABLED_VAR, "true")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::Missing(MEDIA_PROXY_SECRET_VAR),
            "error variant"
        );
        assert!(
            err.to_string().contains(MEDIA_PROXY_SECRET_VAR),
            "message names the variable: {err}"
        );
    }

    #[test]
    fn short_media_proxy_secret_is_an_error() {
        let err = try_config_with(&[
            (MEDIA_PROXY_ENABLED_VAR, "true"),
            (MEDIA_PROXY_SECRET_VAR, "too-short"),
        ])
        .unwrap_err();
        assert_eq!(
            err,
            ConfigError::ShortMediaProxySecret(MEDIA_PROXY_SECRET_VAR),
            "error variant"
        );
        assert!(err.source().is_none(), "no source");
        assert_eq!(
            err.to_string(),
            "invalid MEDIA_PROXY_SECRET: expected at least 32 characters",
            "message states the minimum"
        );
    }

    #[test]
    fn media_proxy_secret_needs_no_length_when_disabled() {
        let config = config_with(&[
            (MEDIA_PROXY_ENABLED_VAR, "false"),
            (MEDIA_PROXY_SECRET_VAR, "short"),
        ]);
        assert!(
            !config.media_proxy.enabled,
            "the disabled proxy accepts any configured secret"
        );
        assert_eq!(
            config.media_proxy.secret.as_deref(),
            Some("short"),
            "the configured secret is kept for when it is enabled"
        );
    }

    #[test]
    fn invalid_media_proxy_enabled_is_an_error() {
        let err = try_config_with(&[(MEDIA_PROXY_ENABLED_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(MEDIA_PROXY_ENABLED_VAR),
            "error variant"
        );
    }

    #[test]
    fn rich_media_defaults_to_nothing_ignored_and_no_remote_cards() {
        let config = config_with(&[]);
        assert!(
            config.rich_media_ignore_hosts.is_empty(),
            "nothing is ignored by default"
        );
        assert!(
            !config.rich_media_remote.get(),
            "remote cards are off by default"
        );
    }

    #[test]
    fn reads_custom_rich_media_settings() {
        let config = config_with(&[
            (RICH_MEDIA_IGNORE_HOSTS_VAR, "internal.example, 127.0.0.1 ,"),
            (RICH_MEDIA_REMOTE_VAR, "true"),
        ]);
        assert_eq!(
            config.rich_media_ignore_hosts,
            vec!["internal.example".to_owned(), "127.0.0.1".to_owned()],
            "the ignore list is a trimmed comma list"
        );
        assert!(config.rich_media_remote.get(), "custom RICH_MEDIA_REMOTE");
    }

    #[test]
    fn rich_media_ignore_list_of_only_separators_is_empty() {
        let config = config_with(&[(RICH_MEDIA_IGNORE_HOSTS_VAR, " , , ")]);
        assert!(
            config.rich_media_ignore_hosts.is_empty(),
            "blank entries fold away, like the media proxy's blacklist"
        );
    }

    #[test]
    fn invalid_rich_media_remote_is_an_error() {
        let err = try_config_with(&[(RICH_MEDIA_REMOTE_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(RICH_MEDIA_REMOTE_VAR),
            "error variant"
        );
    }

    #[test]
    fn debug_shows_the_rich_media_settings() {
        let config = config_with(&[
            (RICH_MEDIA_IGNORE_HOSTS_VAR, "blocked.example"),
            (RICH_MEDIA_REMOTE_VAR, "true"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("rich_media_ignore_hosts: [\"blocked.example\"]"),
            "the ignore list is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("rich_media_remote: Toggle(true)"),
            "the remote toggle is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn debug_redacts_the_media_proxy_secret() {
        let config = config_with(&[
            (MEDIA_PROXY_ENABLED_VAR, "true"),
            (MEDIA_PROXY_SECRET_VAR, "a-secret-of-exactly-32-characters"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("a-secret-of-exactly-32-characters"),
            "the signing secret must not leak: {debug}"
        );
        assert!(
            debug.contains("secret: Some(\"<redacted>\")"),
            "the redaction marker stands in: {debug}"
        );
        assert!(
            debug.contains("enabled: true"),
            "the enablement flag is visible: {debug}"
        );
    }
}
