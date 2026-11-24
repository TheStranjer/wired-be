//! [`ConfigError`]: the one error every reader in this module returns when
//! a required variable is missing or a value is invalid, with its
//! [`Display`](std::fmt::Display) wording and [`Error::source`] chain.

use std::{error::Error, fmt, net::AddrParseError};

use super::{
    defaults::MIN_MEDIA_PROXY_SECRET_CHARS,
    vars::{
        BASE_URL_VAR, BIND_ADDR_VAR, OAUTH_PROVIDERS_VAR, RATE_LIMITS_VAR,
        RESTRICT_UNAUTHENTICATED_AREAS, RESTRICT_UNAUTHENTICATED_VAR, VAPID_PRIVATE_KEY_VAR,
    },
};

/// A configuration value is missing or invalid.
#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// A required environment variable is unset or empty.
    Missing(&'static str),
    /// `BIND_ADDR` is not a valid `ip:port`.
    InvalidBindAddr(AddrParseError),
    /// `BASE_URL` is not an http(s) origin without path or query.
    InvalidBaseUrl,
    /// A numeric setting is not a whole number; the value is the variable name.
    InvalidNumber(&'static str),
    /// A boolean setting is not `true`/`false` (or `1`/`0`); the value is the
    /// variable name.
    InvalidBoolean(&'static str),
    /// `OAUTH_PROVIDERS` is not the JSON text of an array of provider
    /// objects; the value is what the JSON parser said.
    InvalidOauthProviders(String),
    /// `SMTP_TLS` is not one of `starttls`, `tls` or `none`; the value is the
    /// variable name.
    InvalidSmtpTls(&'static str),
    /// `TRANSLATION_PROVIDER` is not one of `none`, `libretranslate` or
    /// `deepl`; the value is the variable name.
    InvalidTranslationProvider(&'static str),
    /// The media proxy is enabled but its `MEDIA_PROXY_SECRET` is shorter
    /// than [`MIN_MEDIA_PROXY_SECRET_CHARS`] requires; the value is the
    /// variable name.
    ShortMediaProxySecret(&'static str),
    /// `RATE_LIMITS` holds an entry that is not a `bucket=max:window_secs`
    /// triple (or a degenerate one); the value is the parser's reason.
    InvalidRateLimits(String),
    /// `RESTRICT_UNAUTHENTICATED` names an area outside
    /// [`RESTRICT_UNAUTHENTICATED_AREAS`]; the value is the name it read.
    InvalidRestrictUnauthenticated(String),
    /// `VAPID_PUBLIC_KEY` is set without `VAPID_PRIVATE_KEY`: a public key
    /// the instance holds no private half for can never verify a push
    /// signature; the value is the public key's variable name.
    VapidPublicWithoutPrivate(&'static str),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(var) => write!(f, "required environment variable {var} is not set"),
            Self::InvalidBindAddr(err) => write!(f, "invalid {BIND_ADDR_VAR}: {err}"),
            Self::InvalidBaseUrl => write!(
                f,
                "invalid {BASE_URL_VAR}: expected an http(s) origin such as \
                 https://social.example with no path or query"
            ),
            Self::InvalidNumber(var) => write!(f, "invalid {var}: expected a whole number"),
            Self::InvalidBoolean(var) => write!(f, "invalid {var}: expected true or false"),
            Self::InvalidOauthProviders(detail) => write!(
                f,
                "invalid {OAUTH_PROVIDERS_VAR}: expected a JSON array of provider objects: {detail}"
            ),
            Self::InvalidSmtpTls(var) => {
                write!(f, "invalid {var}: expected one of starttls, tls or none")
            }
            Self::InvalidTranslationProvider(var) => write!(
                f,
                "invalid {var}: expected one of none, libretranslate or deepl"
            ),
            Self::ShortMediaProxySecret(var) => write!(
                f,
                "invalid {var}: expected at least {MIN_MEDIA_PROXY_SECRET_CHARS} characters"
            ),
            Self::InvalidRateLimits(detail) => {
                write!(f, "invalid {RATE_LIMITS_VAR}: {detail}")
            }
            Self::InvalidRestrictUnauthenticated(area) => write!(
                f,
                "invalid {RESTRICT_UNAUTHENTICATED_VAR}: `{area}` is not one of {}",
                RESTRICT_UNAUTHENTICATED_AREAS.join(", ")
            ),
            Self::VapidPublicWithoutPrivate(var) => write!(
                f,
                "invalid {var}: it is set without {VAPID_PRIVATE_KEY_VAR}, and a public key \
                 with no matching private key can never verify a push signature"
            ),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidBindAddr(err) => Some(err),
            Self::Missing(_)
            | Self::InvalidBaseUrl
            | Self::InvalidNumber(_)
            | Self::InvalidBoolean(_)
            | Self::InvalidOauthProviders(_)
            | Self::InvalidSmtpTls(_)
            | Self::InvalidTranslationProvider(_)
            | Self::ShortMediaProxySecret(_)
            | Self::InvalidRateLimits(_)
            | Self::InvalidRestrictUnauthenticated(_)
            | Self::VapidPublicWithoutPrivate(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::{
        Config,
        test_support::{ORIGIN, URL, lookup_from},
        vars::DATABASE_URL_VAR,
    };

    #[test]
    fn missing_database_url_is_an_error() {
        let err = Config::from_lookup(lookup_from(&[])).unwrap_err();
        assert_eq!(err, ConfigError::Missing(DATABASE_URL_VAR), "error variant");
        assert!(err.source().is_none(), "missing var has no source");
        assert_eq!(
            err.to_string(),
            "required environment variable DATABASE_URL is not set",
            "message"
        );
    }

    #[test]
    fn empty_database_url_is_an_error() {
        let err = Config::from_lookup(lookup_from(&[(DATABASE_URL_VAR, "")])).unwrap_err();
        assert_eq!(err, ConfigError::Missing(DATABASE_URL_VAR), "error variant");
    }

    #[test]
    fn invalid_bind_addr_is_an_error() {
        let err = Config::from_lookup(lookup_from(&[
            (DATABASE_URL_VAR, URL),
            (BASE_URL_VAR, ORIGIN),
            (BIND_ADDR_VAR, "not-an-addr"),
        ]))
        .unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidBindAddr(_)),
            "error variant"
        );
        assert!(err.source().is_some(), "parse error is the source");
        assert!(
            err.to_string().starts_with("invalid BIND_ADDR: "),
            "message"
        );
    }
}
