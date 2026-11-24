//! The settings that say where this instance is: the database URL, the
//! listen socket, the public origin and its normalization, and the four
//! instance-wording strings read into `InstanceProfile`.

use std::net::SocketAddr;

#[cfg(doc)]
use super::Config;

use super::{
    defaults::{
        DEFAULT_BIND_ADDR, DEFAULT_INSTANCE_DESCRIPTION, DEFAULT_INSTANCE_LANGUAGES,
        DEFAULT_INSTANCE_NAME,
    },
    error::ConfigError,
    parse::{comma_list, required_var},
    urls::media_base_url,
    vars::{
        BASE_URL_VAR, BIND_ADDR_VAR, CONTACT_EMAIL_VAR, DATABASE_URL_VAR, INSTANCE_DESCRIPTION_VAR,
        INSTANCE_LANGUAGES_VAR, INSTANCE_NAME_VAR,
    },
};
use crate::util::url::{host_with_port, parse_http_url};

/// The shape [`endpoints`] answers: the database URL, the listen socket,
/// the public origin and the media URL prefix, in [`Config`] field order;
/// factored out because a four-value tuple is a very complex type to
/// spell inline.
pub(crate) type Endpoints = (String, SocketAddr, String, String);

/// Reads the four settings that say where the process and its media are
/// reached: the database URL it connects to, the socket address it listens
/// on, the public origin it is known by, and the media URL prefix that
/// defaults onto that origin. Grouped because each is a few lines of its
/// own and [`Config::from_lookup`] has a size budget (AGENTS.md); the tuple
/// comes back in [`Config`] field order. `media_base_url` is read last, off
/// the normalized origin it defaults onto.
///
/// # Errors
///
/// Returns [`ConfigError::Missing`] when `DATABASE_URL` or `BASE_URL` is
/// unset or blank, [`ConfigError::InvalidBindAddr`] when `BIND_ADDR` holds
/// something that is not a socket address, and
/// [`ConfigError::InvalidBaseUrl`] when `BASE_URL` is not an http(s)
/// origin without path or query.
pub(crate) fn endpoints<F>(lookup: &F) -> Result<Endpoints, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let database_url = required_var(lookup, DATABASE_URL_VAR)?;
    let bind_addr = listen_addr(lookup)?;
    let base_url = normalize_base_url(&required_var(lookup, BASE_URL_VAR)?)?;
    let media_base_url = media_base_url(lookup, &base_url);
    Ok((database_url, bind_addr, base_url, media_base_url))
}

/// Validates a `BASE_URL` value and normalizes it to the bare origin with no
/// trailing slash, e.g. `https://social.example`.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBaseUrl`] unless the value is an absolute
/// `http`/`https` URL with a host and no path, query or trailing junk.
fn normalize_base_url(raw: &str) -> Result<String, ConfigError> {
    let Some(url) = parse_http_url(raw) else {
        return Err(ConfigError::InvalidBaseUrl);
    };
    if !matches!(url.path(), "" | "/") || url.query().is_some() {
        return Err(ConfigError::InvalidBaseUrl);
    }
    let domain = host_with_port(&url);
    Ok(format!("{}://{domain}", url.scheme()))
}

/// What [`instance_profile`] reads: the four settings that describe the
/// instance to the world, named by what they are and hung on the
/// [`Config`] fields in [`Config::from_lookup`] (`INSTANCE_NAME`,
/// `INSTANCE_DESCRIPTION`, `CONTACT_EMAIL`, `INSTANCE_LANGUAGES`).
#[derive(Debug)]
pub(crate) struct InstanceProfile {
    /// The instance display name (`INSTANCE_NAME`).
    pub(crate) name: String,
    /// The short description (`INSTANCE_DESCRIPTION`).
    pub(crate) description: String,
    /// The admin contact email (`CONTACT_EMAIL`).
    pub(crate) contact_email: Option<String>,
    /// The content language codes (`INSTANCE_LANGUAGES`).
    pub(crate) languages: Vec<String>,
}

/// Reads the four settings that describe the instance to the world.
/// Grouped because each is a plain default-or-list and
/// [`Config::from_lookup`] has a size budget (AGENTS.md).
pub(crate) fn instance_profile<F>(lookup: &F) -> InstanceProfile
where
    F: Fn(&str) -> Option<String>,
{
    let name = lookup(INSTANCE_NAME_VAR).unwrap_or_else(|| DEFAULT_INSTANCE_NAME.to_owned());
    let description =
        lookup(INSTANCE_DESCRIPTION_VAR).unwrap_or_else(|| DEFAULT_INSTANCE_DESCRIPTION.to_owned());
    let contact_email = lookup(CONTACT_EMAIL_VAR).filter(|email| !email.is_empty());
    let languages = comma_list(
        &lookup(INSTANCE_LANGUAGES_VAR).unwrap_or_else(|| DEFAULT_INSTANCE_LANGUAGES.to_owned()),
    );
    InstanceProfile {
        name,
        description,
        contact_email,
        languages,
    }
}

/// Reads `BIND_ADDR`: the address the HTTP server listens on,
/// [`DEFAULT_BIND_ADDR`] when unset. Factored out of
/// [`Config::from_lookup`], which has a size budget (AGENTS.md).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBindAddr`] when the value is not a socket
/// address.
fn listen_addr<F>(lookup: &F) -> Result<SocketAddr, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(BIND_ADDR_VAR)
        .as_deref()
        .unwrap_or(DEFAULT_BIND_ADDR)
        .parse()
        .map_err(ConfigError::InvalidBindAddr)
}

#[cfg(test)]
mod tests {
    use std::{error::Error as _, path::PathBuf};

    use super::*;

    use crate::config::{
        Config,
        test_support::{ORIGIN, URL, base_url_result, config_with, lookup_from},
        vars::{
            ACCOUNTS_EMAIL_OPTIONAL_VAR, CAPTCHA_ENABLED_VAR, EMOJI_DIR_VAR,
            HEALTHCHECK_ENABLED_VAR, HTTP_ALLOW_PRIVATE_HOSTS_VAR, MAX_MEDIA_ATTACHMENTS_VAR,
            MAX_POLL_OPTIONS_VAR, MAX_STATUS_CHARS_VAR, MEDIA_BASE_URL_VAR,
            MIN_PASSWORD_LENGTH_VAR, MONGOOSEIM_ENABLED_VAR, OAUTH_TOKEN_TTL_SECS_VAR,
            UPLOAD_LIMIT_BYTES_VAR, UPLOAD_MAX_FILES_VAR, UPLOADS_DIR_VAR,
        },
    };

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
    fn instance_settings_default() {
        let config = config_with(&[]);
        assert_eq!(config.base_url, ORIGIN, "base_url");
        assert_eq!(config.instance_name, "Wired", "default instance_name");
        assert_eq!(
            config.instance_description, "A Wired instance, compatible with Pleroma clients",
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
            config.max_chat_message_chars, 5_000,
            "default max_chat_message_chars"
        );
        assert_eq!(
            config.max_media_description, 1_500,
            "default max_media_description"
        );
        assert_eq!(
            config.upload_limits.max_file_bytes, 16_000_000,
            "default UPLOAD_LIMIT_BYTES"
        );
        assert_eq!(
            config.upload_limits.max_files, 16,
            "default UPLOAD_MAX_FILES"
        );
        assert!(
            !config.fetch.http_allow_private_hosts,
            "the SSRF guard is on by default"
        );
        assert_eq!(
            config.uploads_dir,
            PathBuf::from("./uploads"),
            "default UPLOADS_DIR"
        );
        assert_eq!(
            config.emoji_dir,
            PathBuf::from("./emoji"),
            "default EMOJI_DIR"
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
        assert!(
            !config.mongooseim_enabled.get(),
            "the `MongooseIM` auth endpoints are off by default"
        );
        assert!(
            !config.captcha_enabled.get(),
            "no captcha provider is configured by default"
        );
        assert!(
            !config.healthcheck_enabled.get(),
            "the healthcheck is off by default"
        );
        assert!(
            config.registrations_open.get(),
            "registration is open by default (Pleroma's default)"
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
            (EMOJI_DIR_VAR, "/srv/wired/emoji"),
            (MEDIA_BASE_URL_VAR, "https://cdn.example/media"),
            (ACCOUNTS_EMAIL_OPTIONAL_VAR, "true"),
            (MIN_PASSWORD_LENGTH_VAR, "12"),
            (OAUTH_TOKEN_TTL_SECS_VAR, "3600"),
            (MONGOOSEIM_ENABLED_VAR, "true"),
            (CAPTCHA_ENABLED_VAR, "true"),
            (HEALTHCHECK_ENABLED_VAR, "true"),
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
            config.fetch.http_allow_private_hosts,
            "custom HTTP_ALLOW_PRIVATE_HOSTS"
        );
        assert_eq!(
            config.uploads_dir,
            PathBuf::from("/srv/wired/uploads"),
            "custom UPLOADS_DIR"
        );
        assert_eq!(
            config.emoji_dir,
            PathBuf::from("/srv/wired/emoji"),
            "custom EMOJI_DIR"
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
        assert!(config.mongooseim_enabled.get(), "custom MONGOOSEIM_ENABLED");
        assert!(config.captcha_enabled.get(), "custom CAPTCHA_ENABLED");
        assert!(
            config.healthcheck_enabled.get(),
            "custom HEALTHCHECK_ENABLED"
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
}
