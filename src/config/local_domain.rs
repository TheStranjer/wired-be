//! The reader for `LOCAL_DOMAIN`: the domain handles show
//! (`@alice@whatever.com`) when it differs from the host the instance is
//! served from.

use super::{error::ConfigError, mail::domain_of, vars::LOCAL_DOMAIN_VAR};

/// Reads `LOCAL_DOMAIN`: the normalized handle domain, or the host of
/// `base_url` (with any non-default port) when the variable is unset.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidLocalDomain`] when a set value is not a
/// bare host with an optional port.
pub(crate) fn local_domain<F>(lookup: &F, base_url: &str) -> Result<String, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(LOCAL_DOMAIN_VAR).map_or_else(
        || Ok(domain_of(base_url).to_owned()),
        |raw| normalize_local_domain(&raw),
    )
}

/// Validates a `LOCAL_DOMAIN` value: lowercases it and strips a trailing dot.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidLocalDomain`] unless the value is a bare
/// host, optionally followed by `:port`.
pub(crate) fn normalize_local_domain(raw: &str) -> Result<String, ConfigError> {
    let invalid = |reason: &str| ConfigError::InvalidLocalDomain(reason.to_owned());
    if raw.is_empty() {
        return Err(invalid("the value is empty"));
    }
    if raw.chars().any(char::is_whitespace) {
        return Err(invalid("it must not contain whitespace"));
    }
    if raw.contains("://") {
        return Err(invalid("it must not include a scheme"));
    }
    if raw.contains('@') {
        return Err(invalid("it must not include userinfo"));
    }
    if raw.contains('/') {
        return Err(invalid("it must not include a path"));
    }
    if raw.contains('?') {
        return Err(invalid("it must not include a query"));
    }
    if raw.contains('#') {
        return Err(invalid("it must not include a fragment"));
    }
    let (host, port) = raw
        .split_once(':')
        .map_or((raw, None), |(host, port)| (host, Some(port)));
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    if !is_bare_host(&host) {
        return Err(invalid("it is not a valid host name"));
    }
    match port {
        None => Ok(host),
        Some(port) if is_port(port) => Ok(format!("{host}:{port}")),
        Some(_) => Err(invalid("the port is not a number from 1 to 65535")),
    }
}

/// Whether `host` is a non-empty run of dot-separated labels made of ASCII
/// letters, digits, hyphens and underscores.
fn is_bare_host(host: &str) -> bool {
    !host.is_empty()
        && host.split('.').all(|label| {
            !label.is_empty()
                && label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
}

/// Whether `port` is a decimal number from 1 to 65535.
fn is_port(port: &str) -> bool {
    port.chars().all(|c| c.is_ascii_digit()) && port.parse::<u16>().is_ok_and(|n| n != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::{
        test_support::{ORIGIN, config_with, try_config_with},
        vars::BASE_URL_VAR,
    };

    #[test]
    fn unset_defaults_to_the_web_host() {
        let config = config_with(&[]);
        assert_eq!(config.local_domain(), "social.example", "default");
        assert_eq!(
            config.local_domain(),
            config.web_host(),
            "same as the web host"
        );
    }

    #[test]
    fn unset_keeps_a_non_default_port() {
        let config = config_with(&[(BASE_URL_VAR, "http://localhost:4000")]);
        assert_eq!(config.local_domain(), "localhost:4000", "port kept");
        assert_eq!(
            config.local_domain(),
            config.web_host(),
            "same as the web host"
        );
    }

    #[test]
    fn explicit_value_is_lowercased_and_loses_its_trailing_dot() {
        for (raw, want) in [
            ("whatever.com", "whatever.com"),
            ("Whatever.COM", "whatever.com"),
            ("whatever.com.", "whatever.com"),
            ("WHATEVER.com.", "whatever.com"),
            ("whatever.com:8443", "whatever.com:8443"),
            ("Whatever.com.:8443", "whatever.com:8443"),
            ("localhost", "localhost"),
        ] {
            let config = config_with(&[(LOCAL_DOMAIN_VAR, raw)]);
            assert_eq!(config.local_domain(), want, "LOCAL_DOMAIN={raw:?}");
            assert_eq!(config.web_host(), "social.example", "web host untouched");
        }
    }

    #[test]
    fn equal_to_the_web_host_behaves_like_the_default() {
        let explicit = config_with(&[(LOCAL_DOMAIN_VAR, "Social.Example")]);
        assert_eq!(explicit, config_with(&[]), "same config as unset");
    }

    #[test]
    fn invalid_shapes_are_rejected_with_a_clear_error() {
        for raw in [
            "",
            " ",
            " whatever.com",
            "whatever.com ",
            "what ever.com",
            "https://whatever.com",
            "whatever.com/path",
            "whatever.com/",
            "whatever.com?x=1",
            "whatever.com#frag",
            "user@whatever.com",
            "user:pw@whatever.com",
            ".",
            ".whatever.com",
            "what..ever.com",
            "whatever.com:",
            "whatever.com:0",
            "whatever.com:99999",
            "whatever.com:http",
            "whatever.com:80:81",
            "wha!tever.com",
        ] {
            let err = try_config_with(&[(LOCAL_DOMAIN_VAR, raw)]).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidLocalDomain(_)),
                "LOCAL_DOMAIN={raw:?} is rejected, got {err:?}"
            );
            assert!(
                err.to_string().starts_with("invalid LOCAL_DOMAIN: "),
                "message names the variable: {err}"
            );
        }
    }

    #[test]
    fn the_error_says_what_is_wrong() {
        let err = try_config_with(&[(LOCAL_DOMAIN_VAR, "https://whatever.com")]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid LOCAL_DOMAIN: expected a bare host such as whatever.com, optionally with \
             a port: it must not include a scheme",
            "message"
        );
    }

    #[test]
    fn the_fixture_origin_is_not_a_local_domain_value() {
        assert!(
            normalize_local_domain(ORIGIN).is_err(),
            "a full origin is a scheme, not a bare host"
        );
    }
}
