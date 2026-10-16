//! Runtime configuration, read from environment variables.

use std::{
    error::Error,
    fmt,
    net::{AddrParseError, SocketAddr},
    path::PathBuf,
};

use url::Url;

use crate::extract::multipart::UploadLimits;

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
/// Environment variable holding the maximum size of one uploaded file.
pub const UPLOAD_LIMIT_BYTES_VAR: &str = "UPLOAD_LIMIT_BYTES";
/// Environment variable holding the maximum number of files per request.
pub const UPLOAD_MAX_FILES_VAR: &str = "UPLOAD_MAX_FILES";
/// Environment variable holding whether outbound HTTP may reach private
/// hosts (disabling the SSRF guard).
pub const HTTP_ALLOW_PRIVATE_HOSTS_VAR: &str = "HTTP_ALLOW_PRIVATE_HOSTS";
/// Environment variable holding the directory uploaded files are stored in.
pub const UPLOADS_DIR_VAR: &str = "UPLOADS_DIR";
/// Environment variable holding the public prefix of uploaded files' URLs.
pub const MEDIA_BASE_URL_VAR: &str = "MEDIA_BASE_URL";
/// Environment variable holding whether registration may omit the email
/// address.
pub const ACCOUNTS_EMAIL_OPTIONAL_VAR: &str = "ACCOUNTS_EMAIL_OPTIONAL";
/// Environment variable holding the minimum length of a registration
/// password.
pub const MIN_PASSWORD_LENGTH_VAR: &str = "MIN_PASSWORD_LENGTH";
/// Environment variable holding the lifetime of issued OAuth access
/// tokens, in seconds.
pub const OAUTH_TOKEN_TTL_SECS_VAR: &str = "OAUTH_TOKEN_TTL_SECS";
/// Environment variable holding whether the media proxy rewrites remote
/// media URLs (see [`crate::services::media_proxy`]).
pub const MEDIA_PROXY_ENABLED_VAR: &str = "MEDIA_PROXY_ENABLED";
/// Environment variable holding the HMAC-SHA256 secret the media proxy
/// signs its URLs with; required, at least
/// [`MIN_MEDIA_PROXY_SECRET_CHARS`] characters, whenever the proxy is
/// enabled.
pub const MEDIA_PROXY_SECRET_VAR: &str = "MEDIA_PROXY_SECRET";
/// Environment variable overriding the base URL media proxy links are
/// built from (e.g. a CDN in front of this instance's `/proxy` routes).
pub const MEDIA_PROXY_BASE_URL_VAR: &str = "MEDIA_PROXY_BASE_URL";
/// Environment variable holding the hard byte cap for one proxied media
/// fetch.
pub const MEDIA_PROXY_MAX_BYTES_VAR: &str = "MEDIA_PROXY_MAX_BYTES";
/// Environment variable holding the comma-separated list of hosts the
/// media proxy never proxies.
pub const MEDIA_PROXY_BLACKLIST_VAR: &str = "MEDIA_PROXY_BLACKLIST";
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
/// Upload file size used when `UPLOAD_LIMIT_BYTES` is unset: the 16 MB of
/// Pleroma's `:upload, max_size` (decimal, so just under the middleware's
/// 16 MiB body cap).
pub const DEFAULT_UPLOAD_LIMIT_BYTES: usize = 16_000_000;
/// Upload file count used when `UPLOAD_MAX_FILES` is unset: Pleroma's
/// `:upload, max_number_of_files`.
pub const DEFAULT_UPLOAD_MAX_FILES: usize = 16;
/// Whether the outbound SSRF guard is disabled when
/// `HTTP_ALLOW_PRIVATE_HOSTS` is unset: it is on by default, so private
/// and loopback targets are blocked.
pub const DEFAULT_HTTP_ALLOW_PRIVATE_HOSTS: bool = false;
/// Directory uploaded files are stored in when `UPLOADS_DIR` is unset,
/// relative to the working directory (Pleroma's `Pleroma.Uploaders.Local`
/// default is a `public/uploads` directory in the same spirit).
pub const DEFAULT_UPLOADS_DIR: &str = "./uploads";
/// Path appended to `BASE_URL` for the media URL prefix when
/// `MEDIA_BASE_URL` is unset: the route in [`crate::api::media_files`].
pub const DEFAULT_MEDIA_URL_PATH: &str = "/media";
/// Email requirement used when `ACCOUNTS_EMAIL_OPTIONAL` is unset: an email
/// address is required at registration.
pub const DEFAULT_ACCOUNTS_EMAIL_OPTIONAL: bool = false;
/// Password length used when `MIN_PASSWORD_LENGTH` is unset: the 8
/// characters of Pleroma-era Mastodon password minimums.
pub const DEFAULT_MIN_PASSWORD_LENGTH: usize = 8;
/// Access-token lifetime used when `OAUTH_TOKEN_TTL_SECS` is unset:
/// `2_592_000` seconds, 30 days.
///
/// Pleroma's own `[:oauth2, :token_expires_in]` ships far longer; the
/// ticket default rules here. [`DEFAULT_TOKEN_TTL`] is the same span as
/// a [`std::time::Duration`], kept consistent by a test in
/// [`crate::models::oauth_token`].
///
/// [`DEFAULT_TOKEN_TTL`]: crate::models::oauth_token::DEFAULT_TOKEN_TTL
pub const DEFAULT_OAUTH_TOKEN_TTL_SECS: usize = 2_592_000;
/// Media proxy enablement used when `MEDIA_PROXY_ENABLED` is unset: the
/// proxy is off, so entity URLs pass through untouched.
pub const DEFAULT_MEDIA_PROXY_ENABLED: bool = false;
/// Proxied media byte cap used when `MEDIA_PROXY_MAX_BYTES` is unset:
/// 50 MB, the order of magnitude Pleroma's `max_body_length` media
/// proxying allows.
pub const DEFAULT_MEDIA_PROXY_MAX_BYTES: usize = 50_000_000;
/// Minimum length, in characters, of `MEDIA_PROXY_SECRET` when the media
/// proxy is enabled: 32 bytes of key material for HMAC-SHA256.
pub const MIN_MEDIA_PROXY_SECRET_CHARS: usize = 32;
/// `DATABASE_URL` used by [`Config::for_tests`]: a placeholder URL that
/// points at nothing; integration tests get their real pool from
/// `#[sqlx::test]`.
#[cfg(any(test, feature = "integration-tests"))]
pub const TEST_DATABASE_URL: &str = "postgres://localhost/wired_test";
/// `BASE_URL` used by [`Config::for_tests`].
#[cfg(any(test, feature = "integration-tests"))]
pub const TEST_BASE_URL: &str = "https://test.example";

/// Media proxy settings, from the five `MEDIA_PROXY_*` variables.
///
/// When [`enabled`](Self::enabled) is set, remote media URLs in API
/// responses are rewritten to signed links this instance serves itself
/// (see [`crate::services::media_proxy`]).
#[derive(Clone, PartialEq, Eq)]
pub struct MediaProxyConfig {
    /// Whether remote media URLs are rewritten to proxy links, from
    /// `MEDIA_PROXY_ENABLED`.
    pub enabled: bool,
    /// HMAC-SHA256 signing secret, from `MEDIA_PROXY_SECRET`; required
    /// (and validated to be at least [`MIN_MEDIA_PROXY_SECRET_CHARS`]
    /// characters) whenever [`enabled`](Self::enabled) is set. Redacted
    /// in `Debug` output like `Config`'s `database_url`.
    pub secret: Option<String>,
    /// Base URL proxy links are built from, from `MEDIA_PROXY_BASE_URL`;
    /// `None` means [`Config::base_url`]. Never ends in a slash.
    pub base_url: Option<String>,
    /// Hard cap on the bytes served for one proxied media file, from
    /// `MEDIA_PROXY_MAX_BYTES`.
    pub max_bytes: usize,
    /// Hosts that are never proxied, from `MEDIA_PROXY_BLACKLIST`;
    /// compared case-insensitively against the URL's host.
    pub blacklist: Vec<String>,
}

/// Redacts [`secret`](Self::secret), which is signing key material.
impl fmt::Debug for MediaProxyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaProxyConfig")
            .field("enabled", &self.enabled)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .field("base_url", &self.base_url)
            .field("max_bytes", &self.max_bytes)
            .field("blacklist", &self.blacklist)
            .finish()
    }
}

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
    /// Per-request limits for multipart uploads, from `UPLOAD_LIMIT_BYTES`
    /// and `UPLOAD_MAX_FILES`.
    pub upload_limits: UploadLimits,
    /// Directory uploaded files are stored in, from `UPLOADS_DIR`.
    pub uploads_dir: PathBuf,
    /// Public prefix uploaded files' URLs are built from, from
    /// `MEDIA_BASE_URL`; defaults to `{base_url}/media` and never ends in a
    /// slash, so `/{key}` appends cleanly.
    pub media_base_url: String,
    /// Whether outbound HTTP may connect to private, loopback and other
    /// reserved addresses, i.e. the SSRF guard is disabled, from
    /// `HTTP_ALLOW_PRIVATE_HOSTS`. Development and tests only.
    pub http_allow_private_hosts: bool,
    /// Whether registration may omit the email address, from
    /// `ACCOUNTS_EMAIL_OPTIONAL`. See
    /// [`create_local_user`](crate::models::user::register::create_local_user).
    pub accounts_email_optional: bool,
    /// Minimum length of a registration password in characters, from
    /// `MIN_PASSWORD_LENGTH`.
    pub min_password_length: usize,
    /// Lifetime granted to newly issued OAuth access tokens, in seconds,
    /// from `OAUTH_TOKEN_TTL_SECS`. The `/oauth` endpoints pass it to
    /// [`create_token`](crate::models::oauth_token::create_token) as the
    /// token's `ttl`.
    pub oauth_token_ttl_secs: usize,
    /// The media proxy settings, from the `MEDIA_PROXY_*` variables
    /// (see [`crate::services::media_proxy`]).
    pub media_proxy: MediaProxyConfig,
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
        let upload_limits = UploadLimits {
            max_file_bytes: number(&lookup, UPLOAD_LIMIT_BYTES_VAR, DEFAULT_UPLOAD_LIMIT_BYTES)?,
            max_files: number(&lookup, UPLOAD_MAX_FILES_VAR, DEFAULT_UPLOAD_MAX_FILES)?,
        };
        let http_allow_private_hosts = boolean(
            &lookup,
            HTTP_ALLOW_PRIVATE_HOSTS_VAR,
            DEFAULT_HTTP_ALLOW_PRIVATE_HOSTS,
        )?;
        let uploads_dir = PathBuf::from(
            lookup(UPLOADS_DIR_VAR)
                .map(|dir| dir.trim().to_owned())
                .filter(|dir| !dir.is_empty())
                .unwrap_or_else(|| DEFAULT_UPLOADS_DIR.to_owned()),
        );
        let media_base_url = lookup(MEDIA_BASE_URL_VAR)
            .map(|url| url.trim().trim_end_matches('/').to_owned())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| format!("{base_url}{DEFAULT_MEDIA_URL_PATH}"));
        let (accounts_email_optional, min_password_length, oauth_token_ttl_secs) =
            auth_settings(&lookup)?;
        let media_proxy = media_proxy_settings(&lookup)?;
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
            upload_limits,
            uploads_dir,
            media_base_url,
            http_allow_private_hosts,
            accounts_email_optional,
            min_password_length,
            oauth_token_ttl_secs,
            media_proxy,
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

    /// Configuration for tests: `BASE_URL` is `https://test.example`,
    /// `HTTP_ALLOW_PRIVATE_HOSTS` is `true` (the suites talk to local
    /// mini-servers), every other setting is at its default, and
    /// `DATABASE_URL` is an inert placeholder (tests get their real pool
    /// from `#[sqlx::test]`). Reads no environment variables, so it is
    /// safe in parallel tests.
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
            HTTP_ALLOW_PRIVATE_HOSTS_VAR => Some("true".to_owned()),
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
            upload_limits: UploadLimits {
                max_file_bytes: DEFAULT_UPLOAD_LIMIT_BYTES,
                max_files: DEFAULT_UPLOAD_MAX_FILES,
            },
            uploads_dir: PathBuf::from(DEFAULT_UPLOADS_DIR),
            media_base_url: format!("{TEST_BASE_URL}{DEFAULT_MEDIA_URL_PATH}"),
            http_allow_private_hosts: true,
            accounts_email_optional: DEFAULT_ACCOUNTS_EMAIL_OPTIONAL,
            min_password_length: DEFAULT_MIN_PASSWORD_LENGTH,
            oauth_token_ttl_secs: DEFAULT_OAUTH_TOKEN_TTL_SECS,
            media_proxy: MediaProxyConfig {
                enabled: DEFAULT_MEDIA_PROXY_ENABLED,
                secret: None,
                base_url: None,
                max_bytes: DEFAULT_MEDIA_PROXY_MAX_BYTES,
                blacklist: Vec::new(),
            },
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

/// Reads the three settings that gate registration and token issuance:
/// whether registration may omit the email address, the registration
/// password minimum and the access-token lifetime. Grouped because each
/// is a plain default-or-parse and [`Config::from_lookup`] has a size
/// budget (AGENTS.md); the tuple comes back in field order.
fn auth_settings<F>(lookup: &F) -> Result<(bool, usize, usize), ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let accounts_email_optional = boolean(
        lookup,
        ACCOUNTS_EMAIL_OPTIONAL_VAR,
        DEFAULT_ACCOUNTS_EMAIL_OPTIONAL,
    )?;
    let min_password_length = number(lookup, MIN_PASSWORD_LENGTH_VAR, DEFAULT_MIN_PASSWORD_LENGTH)?;
    let oauth_token_ttl_secs = number(
        lookup,
        OAUTH_TOKEN_TTL_SECS_VAR,
        DEFAULT_OAUTH_TOKEN_TTL_SECS,
    )?;
    Ok((
        accounts_email_optional,
        min_password_length,
        oauth_token_ttl_secs,
    ))
}

/// Reads the five settings that configure the media proxy
/// (see [`crate::services::media_proxy`]). Grouped because each is a
/// plain default-or-parse and [`Config::from_lookup`] has a size budget
/// (AGENTS.md); enabling the proxy without a long-enough secret is an
/// error, since every proxy link would be unsignable (and the route
/// unservable) without it.
fn media_proxy_settings<F>(lookup: &F) -> Result<MediaProxyConfig, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let enabled = boolean(lookup, MEDIA_PROXY_ENABLED_VAR, DEFAULT_MEDIA_PROXY_ENABLED)?;
    let secret = lookup(MEDIA_PROXY_SECRET_VAR)
        .map(|secret| secret.trim().to_owned())
        .filter(|secret| !secret.is_empty());
    if enabled {
        let secret = secret
            .as_deref()
            .ok_or(ConfigError::Missing(MEDIA_PROXY_SECRET_VAR))?;
        if secret.chars().count() < MIN_MEDIA_PROXY_SECRET_CHARS {
            return Err(ConfigError::ShortMediaProxySecret(MEDIA_PROXY_SECRET_VAR));
        }
    }
    let base_url = lookup(MEDIA_PROXY_BASE_URL_VAR)
        .map(|url| url.trim().trim_end_matches('/').to_owned())
        .filter(|url| !url.is_empty());
    let max_bytes = number(
        lookup,
        MEDIA_PROXY_MAX_BYTES_VAR,
        DEFAULT_MEDIA_PROXY_MAX_BYTES,
    )?;
    let blacklist = comma_list(&lookup(MEDIA_PROXY_BLACKLIST_VAR).unwrap_or_default());
    Ok(MediaProxyConfig {
        enabled,
        secret,
        base_url,
        max_bytes,
        blacklist,
    })
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

/// Reads a boolean setting, applying `default` when the variable is unset or
/// empty. Accepted spellings are `true`/`false` (case-insensitive) and `1`/`0`.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] if the value is neither spelling.
fn boolean<F>(lookup: &F, var: &'static str, default: bool) -> Result<bool, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(var).filter(|value| !value.is_empty()) else {
        return Ok(default);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(ConfigError::InvalidBoolean(var)),
    }
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
            .field("upload_limits", &self.upload_limits)
            .field("http_allow_private_hosts", &self.http_allow_private_hosts)
            .field("uploads_dir", &self.uploads_dir)
            .field("media_base_url", &self.media_base_url)
            .field("accounts_email_optional", &self.accounts_email_optional)
            .field("min_password_length", &self.min_password_length)
            .field("oauth_token_ttl_secs", &self.oauth_token_ttl_secs)
            .field("media_proxy", &self.media_proxy)
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
    /// A boolean setting is not `true`/`false` (or `1`/`0`); the value is the
    /// variable name.
    InvalidBoolean(&'static str),
    /// The media proxy is enabled but its `MEDIA_PROXY_SECRET` is shorter
    /// than [`MIN_MEDIA_PROXY_SECRET_CHARS`] requires; the value is the
    /// variable name.
    ShortMediaProxySecret(&'static str),
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
            Self::ShortMediaProxySecret(var) => write!(
                f,
                "invalid {var}: expected at least {MIN_MEDIA_PROXY_SECRET_CHARS} characters"
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
            | Self::ShortMediaProxySecret(_) => None,
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
        assert_eq!(
            config.upload_limits.max_file_bytes, 16_000_000,
            "default UPLOAD_LIMIT_BYTES"
        );
        assert_eq!(
            config.upload_limits.max_files, 16,
            "default UPLOAD_MAX_FILES"
        );
        assert!(
            !config.http_allow_private_hosts,
            "the SSRF guard is on by default"
        );
        assert_eq!(
            config.uploads_dir,
            PathBuf::from("./uploads"),
            "default UPLOADS_DIR"
        );
        assert_eq!(
            config.media_base_url, "https://social.example/media",
            "MEDIA_BASE_URL defaults to BASE_URL plus /media"
        );
        assert!(
            !config.accounts_email_optional,
            "an email address is required at registration by default"
        );
        assert_eq!(config.min_password_length, 8, "default MIN_PASSWORD_LENGTH");
        assert_eq!(
            config.oauth_token_ttl_secs, 2_592_000,
            "default OAUTH_TOKEN_TTL_SECS is 30 days"
        );
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
            (UPLOAD_LIMIT_BYTES_VAR, "1048576"),
            (UPLOAD_MAX_FILES_VAR, "4"),
            (HTTP_ALLOW_PRIVATE_HOSTS_VAR, "true"),
            (UPLOADS_DIR_VAR, "/srv/wired/uploads"),
            (MEDIA_BASE_URL_VAR, "https://cdn.example/media"),
            (ACCOUNTS_EMAIL_OPTIONAL_VAR, "true"),
            (MIN_PASSWORD_LENGTH_VAR, "12"),
            (OAUTH_TOKEN_TTL_SECS_VAR, "3600"),
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
        assert_eq!(
            config.upload_limits.max_file_bytes, 1_048_576,
            "custom UPLOAD_LIMIT_BYTES"
        );
        assert_eq!(config.upload_limits.max_files, 4, "custom UPLOAD_MAX_FILES");
        assert!(
            config.http_allow_private_hosts,
            "custom HTTP_ALLOW_PRIVATE_HOSTS"
        );
        assert_eq!(
            config.uploads_dir,
            PathBuf::from("/srv/wired/uploads"),
            "custom UPLOADS_DIR"
        );
        assert_eq!(
            config.media_base_url, "https://cdn.example/media",
            "custom MEDIA_BASE_URL"
        );
        assert!(
            config.accounts_email_optional,
            "custom ACCOUNTS_EMAIL_OPTIONAL"
        );
        assert_eq!(config.min_password_length, 12, "custom MIN_PASSWORD_LENGTH");
        assert_eq!(
            config.oauth_token_ttl_secs, 3_600,
            "custom OAUTH_TOKEN_TTL_SECS"
        );
    }

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
                config.http_allow_private_hosts, want,
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
    fn accounts_email_optional_parses_booleans() {
        for (value, want) in [
            ("true", true),
            ("TRUE", true),
            (" 1 ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", false),
        ] {
            let config = config_with(&[(ACCOUNTS_EMAIL_OPTIONAL_VAR, value)]);
            assert_eq!(
                config.accounts_email_optional, want,
                "ACCOUNTS_EMAIL_OPTIONAL={value:?} means {want}"
            );
        }
    }

    #[test]
    fn invalid_accounts_email_optional_is_an_error() {
        let err = try_config_with(&[(ACCOUNTS_EMAIL_OPTIONAL_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(ACCOUNTS_EMAIL_OPTIONAL_VAR),
            "error variant"
        );
        assert!(
            err.to_string()
                .starts_with(&format!("invalid {ACCOUNTS_EMAIL_OPTIONAL_VAR}: ")),
            "message names the variable"
        );
    }

    #[test]
    fn reads_custom_min_password_length() {
        let config = config_with(&[(MIN_PASSWORD_LENGTH_VAR, "3")]);
        assert_eq!(
            config.min_password_length, 3,
            "MIN_PASSWORD_LENGTH below the 8-character default is honored"
        );
    }

    #[test]
    fn debug_shows_the_registration_settings() {
        let config = config_with(&[
            (ACCOUNTS_EMAIL_OPTIONAL_VAR, "true"),
            (MIN_PASSWORD_LENGTH_VAR, "10"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("accounts_email_optional: true"),
            "the email flag is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("min_password_length: 10"),
            "the password minimum is visible in Debug output: {debug}"
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
    }

    #[test]
    fn debug_shows_the_token_ttl_setting() {
        let config = config_with(&[(OAUTH_TOKEN_TTL_SECS_VAR, "600")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("oauth_token_ttl_secs: 600"),
            "the token lifetime is visible in Debug output: {debug}"
        );
    }

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
            UPLOAD_LIMIT_BYTES_VAR,
            UPLOAD_MAX_FILES_VAR,
            MIN_PASSWORD_LENGTH_VAR,
            OAUTH_TOKEN_TTL_SECS_VAR,
            MEDIA_PROXY_MAX_BYTES_VAR,
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
        // The URL is `postgres://user:secret@localhost/wired`; the literal
        // word `secret` also names the media proxy's (redacted) field, so
        // the leak check is against the credential, not the word.
        assert!(!debug.contains("user:secret@"), "password must not leak");
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
        assert!(
            config.http_allow_private_hosts,
            "test config disables the SSRF guard for local mini-servers"
        );
        assert_eq!(
            config.instance_name, DEFAULT_INSTANCE_NAME,
            "defaults apply"
        );
        assert_eq!(
            config.uploads_dir,
            PathBuf::from(DEFAULT_UPLOADS_DIR),
            "test uploads go to the default directory"
        );
        assert_eq!(
            config.media_base_url, "https://test.example/media",
            "test media URLs hang off the test BASE_URL"
        );
        assert!(
            !config.media_proxy.enabled,
            "the media proxy is off for tests by default, so entity URLs pass through"
        );
        let expected = Config::from_lookup(lookup_from(&[
            (DATABASE_URL_VAR, TEST_DATABASE_URL),
            (BASE_URL_VAR, TEST_BASE_URL),
            (HTTP_ALLOW_PRIVATE_HOSTS_VAR, "true"),
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
            (HTTP_ALLOW_PRIVATE_HOSTS_VAR, "true"),
        ]))
        .unwrap();
        assert_eq!(
            Config::test_defaults(),
            parsed,
            "fallback equals the parsed test config"
        );
    }
}
