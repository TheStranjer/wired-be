//! Runtime configuration, read from environment variables.

use std::{
    error::Error,
    fmt,
    net::{AddrParseError, SocketAddr},
};

use url::Url;

/// Environment variable holding the PostgreSQL connection URL.
pub const DATABASE_URL_VAR: &str = "DATABASE_URL";
/// Environment variable holding the address to listen on.
pub const BIND_ADDR_VAR: &str = "BIND_ADDR";
/// Environment variable holding the public origin of this instance.
pub const BASE_URL_VAR: &str = "BASE_URL";
/// Environment variable holding the instance display name.
pub const INSTANCE_NAME_VAR: &str = "INSTANCE_NAME";
/// Environment variable holding the instance description.
pub const INSTANCE_DESCRIPTION_VAR: &str = "INSTANCE_DESCRIPTION";
/// Environment variable holding the admin contact email.
pub const CONTACT_EMAIL_VAR: &str = "CONTACT_EMAIL";
/// Environment variable holding the comma-separated content languages.
pub const INSTANCE_LANGUAGES_VAR: &str = "INSTANCE_LANGUAGES";
/// Environment variable holding the maximum status length in characters.
pub const MAX_STATUS_CHARS_VAR: &str = "MAX_STATUS_CHARS";
/// Environment variable holding the maximum media attachments per status.
pub const MAX_MEDIA_ATTACHMENTS_VAR: &str = "MAX_MEDIA_ATTACHMENTS";
/// Environment variable holding the maximum options per poll.
pub const MAX_POLL_OPTIONS_VAR: &str = "MAX_POLL_OPTIONS";
/// Listen address used when `BIND_ADDR` is unset.
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:4000";
/// Instance name used when `INSTANCE_NAME` is unset.
pub const DEFAULT_INSTANCE_NAME: &str = "Wired";
/// Description used when `INSTANCE_DESCRIPTION` is unset.
pub const DEFAULT_INSTANCE_DESCRIPTION: &str = "A Pleroma-compatible instance";
/// Language list used when `INSTANCE_LANGUAGES` is unset.
pub const DEFAULT_INSTANCE_LANGUAGES: &str = "en";
/// Status length used when `MAX_STATUS_CHARS` is unset.
pub const DEFAULT_MAX_STATUS_CHARS: usize = 5000;
/// Attachment limit used when `MAX_MEDIA_ATTACHMENTS` is unset.
pub const DEFAULT_MAX_MEDIA_ATTACHMENTS: usize = 4;
/// Poll option limit used when `MAX_POLL_OPTIONS` is unset.
pub const DEFAULT_MAX_POLL_OPTIONS: usize = 20;
/// `DATABASE_URL` used by [`Config::for_tests`]: a placeholder URL that
/// points at nothing; integration tests get their real pool from
/// `#[sqlx::test]`.
#[cfg(any(test, feature = "integration-tests"))]
pub const TEST_DATABASE_URL: &str = "postgres://localhost/wired_test";
/// `BASE_URL` used by [`Config::for_tests`].
#[cfg(any(test, feature = "integration-tests"))]
pub const TEST_BASE_URL: &str = "https://test.example";

/// Application configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    /// PostgreSQL connection URL, e.g. `postgres://user:pass@localhost/wired`.
    pub database_url: String,
    /// Address the HTTP server binds to.
    pub bind_addr: SocketAddr,
    /// Public origin of this instance, normalized to e.g.
    /// `https://social.example` (no path, query or trailing slash).
    pub base_url: String,
    /// Display name of the instance.
    pub instance_name: String,
    /// Short description of the instance.
    pub instance_description: String,
    /// Admin contact email, if configured.
    pub contact_email: Option<String>,
    /// Content language codes advertised by the instance.
    pub instance_languages: Vec<String>,
    /// Maximum length of a status in characters.
    pub max_status_chars: usize,
    /// Maximum number of media attachments per status.
    pub max_media_attachments: usize,
    /// Maximum number of options per poll.
    pub max_poll_options: usize,
}

impl Config {
    /// Reads configuration from the process environment.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a required variable is missing or a value is invalid.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Reads configuration through `lookup`, which maps a variable name to its
    /// value. Tests pass values in here instead of mutating the environment.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a required variable is missing or a value is invalid.
    pub fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let database_url = lookup(DATABASE_URL_VAR)
            .filter(|url| !url.is_empty())
            .ok_or(ConfigError::Missing(DATABASE_URL_VAR))?;
        let bind_addr = lookup(BIND_ADDR_VAR)
            .as_deref()
            .unwrap_or(DEFAULT_BIND_ADDR)
            .parse()
            .map_err(ConfigError::InvalidBindAddr)?;
        let base_url_raw = lookup(BASE_URL_VAR)
            .filter(|url| !url.is_empty())
            .ok_or(ConfigError::Missing(BASE_URL_VAR))?;
        let base_url = normalize_base_url(&base_url_raw)?;
        let instance_name =
            lookup(INSTANCE_NAME_VAR).unwrap_or_else(|| DEFAULT_INSTANCE_NAME.to_owned());
        let instance_description = lookup(INSTANCE_DESCRIPTION_VAR)
            .unwrap_or_else(|| DEFAULT_INSTANCE_DESCRIPTION.to_owned());
        let contact_email = lookup(CONTACT_EMAIL_VAR).filter(|email| !email.is_empty());
        let instance_languages = comma_list(
            &lookup(INSTANCE_LANGUAGES_VAR)
                .unwrap_or_else(|| DEFAULT_INSTANCE_LANGUAGES.to_owned()),
        );
        let max_status_chars = number(&lookup, MAX_STATUS_CHARS_VAR, DEFAULT_MAX_STATUS_CHARS)?;
        let max_media_attachments = number(
            &lookup,
            MAX_MEDIA_ATTACHMENTS_VAR,
            DEFAULT_MAX_MEDIA_ATTACHMENTS,
        )?;
        let max_poll_options = number(&lookup, MAX_POLL_OPTIONS_VAR, DEFAULT_MAX_POLL_OPTIONS)?;
        Ok(Self {
            database_url,
            bind_addr,
            base_url,
            instance_name,
            instance_description,
            contact_email,
            instance_languages,
            max_status_chars,
            max_media_attachments,
            max_poll_options,
        })
    }

    /// The host of [`base_url`](Self::base_url), plus `:port` when the port is
    /// not the scheme's default, e.g. `social.example` or `localhost:4000`.
    #[must_use]
    pub fn domain(&self) -> &str {
        self.base_url
            .split_once("://")
            .map_or(self.base_url.as_str(), |(_, domain)| domain)
    }

    /// Whether [`base_url`](Self::base_url) uses the `https` scheme.
    #[must_use]
    pub fn is_https(&self) -> bool {
        self.base_url.starts_with("https://")
    }

    /// Configuration for tests: `BASE_URL` is `https://test.example`, every
    /// other setting is at its default, and `DATABASE_URL` is an inert
    /// placeholder (tests get their real pool from `#[sqlx::test]`). Reads no
    /// environment variables, so it is safe in parallel tests.
    ///
    /// Available to unit tests through `cfg(test)` and to the integration
    /// tests through the `integration-tests` feature.
    #[cfg(any(test, feature = "integration-tests"))]
    #[must_use]
    pub fn for_tests() -> Self {
        // The inputs are fixed and valid, so `from_lookup` cannot fail; the
        // fallback arm builds the same config without a panic, keeping this
        // constructor infallible.
        Self::from_lookup(|key| match key {
            DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
            BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
            _ => None,
        })
        .unwrap_or_else(|_| Self::test_defaults())
    }

    /// The [`for_tests`](Self::for_tests) configuration built directly from
    /// the constants, without any parsing. Only reachable if parsing the
    /// fixed test values ever failed, which they cannot.
    #[cfg(any(test, feature = "integration-tests"))]
    fn test_defaults() -> Self {
        Self {
            database_url: TEST_DATABASE_URL.to_owned(),
            bind_addr: SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 4000)),
            base_url: TEST_BASE_URL.to_owned(),
            instance_name: DEFAULT_INSTANCE_NAME.to_owned(),
            instance_description: DEFAULT_INSTANCE_DESCRIPTION.to_owned(),
            contact_email: None,
            instance_languages: comma_list(DEFAULT_INSTANCE_LANGUAGES),
            max_status_chars: DEFAULT_MAX_STATUS_CHARS,
            max_media_attachments: DEFAULT_MAX_MEDIA_ATTACHMENTS,
            max_poll_options: DEFAULT_MAX_POLL_OPTIONS,
        }
    }
}

/// Validates a `BASE_URL` value and normalizes it to the bare origin with no
/// trailing slash, e.g. `https://social.example`.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBaseUrl`] unless the value is an absolute
/// `http`/`https` URL with a host and no path, query or trailing junk.
fn normalize_base_url(raw: &str) -> Result<String, ConfigError> {
    let Ok(url) = Url::parse(raw) else {
        return Err(ConfigError::InvalidBaseUrl);
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ConfigError::InvalidBaseUrl);
    }
    let Some(host) = url.host_str() else {
        return Err(ConfigError::InvalidBaseUrl);
    };
    if !matches!(url.path(), "" | "/") || url.query().is_some() {
        return Err(ConfigError::InvalidBaseUrl);
    }
    let domain = url
        .port()
        .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"));
    Ok(format!("{}://{domain}", url.scheme()))
}

/// Reads a non-negative integer setting, applying `default` when the variable
/// is unset or empty.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] if the value is not a whole number.
fn number<F>(lookup: &F, var: &'static str, default: usize) -> Result<usize, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(var).filter(|value| !value.is_empty()) else {
        return Ok(default);
    };
    raw.parse().ok().ok_or(ConfigError::InvalidNumber(var))
}

/// Splits a comma-separated list into trimmed, non-empty owned values.
fn comma_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Redacts `database_url`, which usually contains a password.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("database_url", &"<redacted>")
            .field("bind_addr", &self.bind_addr)
            .field("base_url", &self.base_url)
            .field("instance_name", &self.instance_name)
            .field("instance_description", &self.instance_description)
            .field("contact_email", &self.contact_email)
            .field("instance_languages", &self.instance_languages)
            .field("max_status_chars", &self.max_status_chars)
            .field("max_media_attachments", &self.max_media_attachments)
            .field("max_poll_options", &self.max_poll_options)
            .finish()
    }
}

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
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Missing(_) | Self::InvalidBaseUrl | Self::InvalidNumber(_) => None,
            Self::InvalidBindAddr(err) => Some(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "postgres://user:secret@localhost/wired";
    const ORIGIN: &str = "https://social.example";

    fn lookup_from(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|&(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    /// Reads a [`Config`] with the two required variables set, plus `extra`
    /// (which may override them).
    fn try_config_with(extra: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let mut vars: Vec<(String, String)> = vec![
            (DATABASE_URL_VAR.to_owned(), URL.to_owned()),
            (BASE_URL_VAR.to_owned(), ORIGIN.to_owned()),
        ];
        for &(key, value) in extra {
            match vars.iter_mut().find(|(k, _)| k == key) {
                Some(existing) => existing.1 = value.to_owned(),
                None => vars.push((key.to_owned(), value.to_owned())),
            }
        }
        Config::from_lookup(lookup_from(
            &vars
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect::<Vec<_>>(),
        ))
    }

    /// Like [`try_config_with`], panicking on error.
    fn config_with(extra: &[(&str, &str)]) -> Config {
        try_config_with(extra).unwrap()
    }

    /// The result of reading a config whose `BASE_URL` is `value`.
    fn base_url_result(value: &str) -> Result<Config, ConfigError> {
        Config::from_lookup(lookup_from(&[
            (DATABASE_URL_VAR, URL),
            (BASE_URL_VAR, value),
        ]))
    }

    #[test]
    fn reads_database_url_and_defaults_bind_addr() {
        let config = Config::from_lookup(lookup_from(&[
            (DATABASE_URL_VAR, URL),
            (BASE_URL_VAR, ORIGIN),
        ]))
        .unwrap();
        assert_eq!(config.database_url, URL, "database_url");
        assert_eq!(
            config.bind_addr,
            DEFAULT_BIND_ADDR.parse().unwrap(),
            "default bind_addr"
        );
    }

    #[test]
    fn reads_custom_bind_addr() {
        let config = Config::from_lookup(lookup_from(&[
            (DATABASE_URL_VAR, URL),
            (BASE_URL_VAR, ORIGIN),
            (BIND_ADDR_VAR, "0.0.0.0:8080"),
        ]))
        .unwrap();
        assert_eq!(
            config.bind_addr,
            "0.0.0.0:8080".parse().unwrap(),
            "custom bind_addr"
        );
    }

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

    #[test]
    fn instance_settings_default() {
        let config = config_with(&[]);
        assert_eq!(config.base_url, ORIGIN, "base_url");
        assert_eq!(config.instance_name, "Wired", "default instance_name");
        assert_eq!(
            config.instance_description, "A Pleroma-compatible instance",
            "default instance_description"
        );
        assert_eq!(config.contact_email, None, "default contact_email");
        assert_eq!(
            config.instance_languages,
            vec!["en".to_owned()],
            "default instance_languages"
        );
        assert_eq!(config.max_status_chars, 5000, "default max_status_chars");
        assert_eq!(
            config.max_media_attachments, 4,
            "default max_media_attachments"
        );
        assert_eq!(config.max_poll_options, 20, "default max_poll_options");
        assert_eq!(config.domain(), "social.example", "domain without port");
        assert!(config.is_https(), "https base_url");
    }

    #[test]
    fn reads_custom_instance_settings() {
        let config = config_with(&[
            (INSTANCE_NAME_VAR, "Snatter"),
            (INSTANCE_DESCRIPTION_VAR, "Where we all chatter"),
            (CONTACT_EMAIL_VAR, "admin@example.org"),
            (INSTANCE_LANGUAGES_VAR, "en, de ,fr"),
            (MAX_STATUS_CHARS_VAR, "300"),
            (MAX_MEDIA_ATTACHMENTS_VAR, "6"),
            (MAX_POLL_OPTIONS_VAR, "12"),
        ]);
        assert_eq!(config.instance_name, "Snatter", "custom instance_name");
        assert_eq!(
            config.instance_description, "Where we all chatter",
            "custom instance_description"
        );
        assert_eq!(
            config.contact_email.as_deref(),
            Some("admin@example.org"),
            "custom contact_email"
        );
        assert_eq!(
            config.instance_languages,
            vec!["en".to_owned(), "de".to_owned(), "fr".to_owned()],
            "comma list is trimmed"
        );
        assert_eq!(config.max_status_chars, 300, "custom max_status_chars");
        assert_eq!(
            config.max_media_attachments, 6,
            "custom max_media_attachments"
        );
        assert_eq!(config.max_poll_options, 12, "custom max_poll_options");
    }

    #[test]
    fn empty_contact_email_is_none() {
        let config = config_with(&[(CONTACT_EMAIL_VAR, "")]);
        assert_eq!(config.contact_email, None, "empty contact_email");
    }

    #[test]
    fn missing_base_url_is_an_error() {
        let err = Config::from_lookup(lookup_from(&[(DATABASE_URL_VAR, URL)])).unwrap_err();
        assert_eq!(err, ConfigError::Missing(BASE_URL_VAR), "error variant");
        assert_eq!(
            err.to_string(),
            "required environment variable BASE_URL is not set",
            "message"
        );
        let empty_err = base_url_result("").unwrap_err();
        assert_eq!(
            empty_err,
            ConfigError::Missing(BASE_URL_VAR),
            "empty BASE_URL is missing"
        );
    }

    #[test]
    fn base_url_with_invalid_scheme_is_an_error() {
        for value in [
            "ftp://example.com",
            "https://",
            "not a url",
            "social.example",
        ] {
            let err = base_url_result(value).unwrap_err();
            assert_eq!(err, ConfigError::InvalidBaseUrl, "error for {value}");
            assert!(err.source().is_none(), "no source for {value}");
        }
        let message = ConfigError::InvalidBaseUrl.to_string();
        assert!(
            message.starts_with("invalid BASE_URL: "),
            "message names the variable: {message}"
        );
    }

    #[test]
    fn base_url_with_path_or_query_is_an_error() {
        for value in [
            "https://example.com/instance",
            "https://example.com/a/b",
            "https://example.com?next=/login",
        ] {
            let err = base_url_result(value).unwrap_err();
            assert_eq!(err, ConfigError::InvalidBaseUrl, "error for {value}");
        }
    }

    #[test]
    fn base_url_trailing_slash_is_stripped() {
        let config = base_url_result("https://social.example/").unwrap();
        assert_eq!(config.base_url, ORIGIN, "trailing slash stripped");
        assert_eq!(config.domain(), "social.example", "domain unchanged");
    }

    #[test]
    fn base_url_with_port_keeps_it_in_domain() {
        let config = base_url_result("http://localhost:4000").unwrap();
        assert_eq!(config.base_url, "http://localhost:4000", "base_url kept");
        assert_eq!(config.domain(), "localhost:4000", "domain with port");
        assert!(!config.is_https(), "http base_url is not https");
    }

    #[test]
    fn base_url_default_port_is_dropped() {
        let config = base_url_result("https://example.com:443/").unwrap();
        assert_eq!(config.base_url, "https://example.com", "default port gone");
        assert_eq!(config.domain(), "example.com", "domain without port");
        assert!(config.is_https(), "https stays https");
    }

    #[test]
    fn invalid_numbers_are_errors() {
        for var in [
            MAX_STATUS_CHARS_VAR,
            MAX_MEDIA_ATTACHMENTS_VAR,
            MAX_POLL_OPTIONS_VAR,
        ] {
            let err = try_config_with(&[(var, "many")]).unwrap_err();
            assert_eq!(err, ConfigError::InvalidNumber(var), "error for {var}");
            assert!(err.source().is_none(), "no source for {var}");
            assert!(
                err.to_string().starts_with(&format!("invalid {var}: ")),
                "message names {var}"
            );
        }
    }

    #[test]
    fn debug_redacts_database_url() {
        let config = config_with(&[]);
        let debug = format!("{config:?}");
        assert!(!debug.contains("secret"), "password must not leak");
        assert!(debug.contains("<redacted>"), "redaction marker");
        assert!(debug.contains(ORIGIN), "base_url is shown");
    }

    #[test]
    fn from_env_reads_process_environment() {
        // Never mutates the environment; just checks `from_env` reads the real one.
        let from_env = Config::from_env();
        let from_lookup = Config::from_lookup(|key| std::env::var(key).ok());
        assert_eq!(
            from_env, from_lookup,
            "from_env matches the process environment"
        );
    }

    #[test]
    fn for_tests_is_the_fixed_lookup_config() {
        let config = Config::for_tests();
        assert_eq!(config.base_url, TEST_BASE_URL, "test base_url");
        assert_eq!(config.database_url, TEST_DATABASE_URL, "placeholder url");
        assert_eq!(config.domain(), "test.example", "test domain");
        assert!(config.is_https(), "test base_url is https");
        assert_eq!(
            config.instance_name, DEFAULT_INSTANCE_NAME,
            "defaults apply"
        );
        let expected = Config::from_lookup(lookup_from(&[
            (DATABASE_URL_VAR, TEST_DATABASE_URL),
            (BASE_URL_VAR, TEST_BASE_URL),
        ]))
        .unwrap();
        assert_eq!(config, expected, "for_tests equals from_lookup");
    }

    #[test]
    fn for_tests_fallback_matches_parsed_config() {
        // `for_tests` only uses the fallback if parsing its fixed values ever
        // fails; this proves the fallback builds the very same Config.
        let parsed = Config::from_lookup(lookup_from(&[
            (DATABASE_URL_VAR, TEST_DATABASE_URL),
            (BASE_URL_VAR, TEST_BASE_URL),
        ]))
        .unwrap();
        assert_eq!(
            Config::test_defaults(),
            parsed,
            "fallback equals the parsed test config"
        );
    }
}
