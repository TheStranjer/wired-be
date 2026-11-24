//! Runtime configuration, read from environment variables.
//!
//! This is the boot-time baseline; the admin config store
//! ([`crate::services::runtime_config`]) overlays a whitelisted subset of
//! it with DB rows at request time.
//!
//! The module is split by theme: the environment variable names in
//! [`vars`] and their defaults in [`defaults`]; [`Config`] itself is
//! declared in [`model`] beside its value types ([`federation_types`],
//! [`frontend`], [`integrations`]); the readers take it group by group
//! ([`instance`], [`fetch`], [`federation`], [`jobs`], [`mail`], [`auth`],
//! [`registration`], [`limits`], [`wording`], [`uploads`], [`urls`],
//! [`exposure`], [`media`]) on the shared primitives of [`parse`],
//! reported as [`ConfigError`] from [`error`] and rendered redacted by
//! [`debug`]. Everything the rest of the crate imports is re-exported
//! here, so the `crate::config::…` paths outside this module keep
//! working.

#[cfg(any(test, feature = "integration-tests"))]
use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf};

#[cfg(any(test, feature = "integration-tests"))]
use crate::jobs::DEFAULT_JOB_QUEUES;

use self::{
    exposure::{groups, translation_settings, vapid_settings},
    federation::{
        accept_unsolicited_public, federation_enabled, mrf_settings, rate_limit_settings,
        unreachable_grace_secs,
    },
    instance::endpoints,
    jobs::streaming_heartbeat,
    mail::domain_of,
    media::{media_proxy_settings, rich_media_ignore_hosts, rich_media_remote},
    urls::{directory, frontend_base_url},
};

#[cfg(any(test, feature = "integration-tests"))]
use self::{mail::default_mail_from, parse::comma_list};

pub mod auth;
pub mod debug;
pub mod defaults;
pub mod error;
pub mod exposure;
pub mod federation;
pub mod federation_types;
pub mod fetch;
pub mod frontend;
pub mod instance;
pub mod integrations;
pub mod jobs;
pub mod limits;
pub mod mail;
pub mod media;
pub mod model;
pub mod parse;
pub mod registration;
pub mod uploads;
pub mod urls;
pub mod vars;
pub mod wording;

#[cfg(test)]
pub mod test_support;

pub use self::{
    defaults::*, error::*, federation_types::*, frontend::*, integrations::*, model::*, vars::*,
};

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
    pub fn from_lookup<F: Fn(&str) -> Option<String>>(lookup: F) -> Result<Self, ConfigError> {
        let (database_url, bind_addr, base_url, media_base_url) = endpoints(&lookup)?;
        let (profile, limits, text, auth, exposure, mail, rest) = groups(&lookup, &base_url)?;
        Ok(Self {
            database_url,
            bind_addr,
            frontend_base_url: frontend_base_url(&lookup, &base_url),
            base_url,
            instance_name: profile.name,
            instance_description: profile.description,
            contact_email: profile.contact_email,
            instance_languages: profile.languages,
            max_status_chars: limits.status_chars,
            max_media_attachments: limits.media_attachments,
            max_poll_options: limits.poll_options,
            max_poll_option_chars: limits.poll_option_chars,
            min_poll_expiration_secs: limits.min_poll_expiration_secs,
            max_poll_expiration_secs: limits.max_poll_expiration_secs,
            max_chat_message_chars: limits.chat_message_chars,
            max_media_description: text.media_description,
            max_display_name_chars: text.display_name_chars,
            max_bio_chars: text.bio_chars,
            max_profile_fields: text.profile_fields,
            max_report_comment: limits.report_comment,
            max_image_pixels: limits.image_pixels,
            max_pinned_statuses: limits.pinned_statuses,
            min_expiration_secs: limits.min_expiration_secs,
            min_scheduled_secs: limits.min_scheduled_secs,
            max_scheduled_per_user: limits.max_scheduled_per_user,
            max_scheduled_per_user_per_day: limits.max_scheduled_per_user_per_day,
            upload_limits: rest.upload_limits,
            avatar_limit_bytes: rest.avatar_limit_bytes,
            banner_limit_bytes: rest.banner_limit_bytes,
            inbox_max_bytes: limits.inbox_max_bytes,
            max_remote_mention_lookups: limits.max_remote_mention_lookups,
            uploads_dir: directory(&lookup, UPLOADS_DIR_VAR, DEFAULT_UPLOADS_DIR),
            emoji_dir: directory(&lookup, EMOJI_DIR_VAR, DEFAULT_EMOJI_DIR),
            media_base_url,
            accounts_email_optional: auth.accounts_email_optional,
            min_password_length: auth.min_password_length,
            oauth_token_ttl_secs: auth.oauth_token_ttl_secs,
            password_reset_ttl_secs: auth.password_reset_ttl_secs,
            allow_account_deletion: auth.allow_account_deletion,
            mongooseim_enabled: rest.mongooseim_enabled,
            captcha_enabled: rest.captcha_enabled,
            healthcheck_enabled: rest.healthcheck_enabled,
            registrations_open: exposure.registrations_open,
            invites_enabled: exposure.invites_enabled,
            registrations_approval_required: exposure.registrations_approval_required,
            account_activation_required: exposure.account_activation_required,
            search_statuses_unauthenticated: exposure.search_statuses_unauthenticated,
            restrict_unauthenticated: exposure.restrict_unauthenticated,
            oauth_providers: auth.oauth_providers,
            available_frontends: exposure.available_frontends,
            job_concurrency: rest.job_concurrency,
            job_queues: rest.job_queues,
            purge_deleted_statuses_days: rest.purge_deleted_statuses_days,
            backup_validity_days: rest.backup_validity_days,
            backup_limit_days: rest.backup_limit_days,
            backup_purge: rest.backup_purge,
            streaming_ping_interval_secs: streaming_heartbeat(&lookup)?,
            smtp_host: mail.host,
            smtp_port: mail.port,
            smtp_username: mail.username,
            smtp_password: mail.password,
            smtp_tls: mail.tls,
            mail_from: mail.from,
            media_proxy: media_proxy_settings(&lookup)?,
            statics: statics_settings(&lookup)?,
            rich_media_ignore_hosts: rich_media_ignore_hosts(&lookup),
            rich_media_remote: rich_media_remote(&lookup)?,
            vapid: vapid_settings(&lookup)?,
            translation: translation_settings(&lookup)?,
            fetch: rest.fetch,
            federation_enabled: federation_enabled(&lookup)?,
            accept_unsolicited_public: accept_unsolicited_public(&lookup)?,
            unreachable_grace_secs: unreachable_grace_secs(&lookup)?,
            mrf: mrf_settings(&lookup)?,
            rate_limit: rate_limit_settings(&lookup)?,
        })
    }

    /// The [`base_url`](Self::base_url) without its scheme: `social.example`,
    /// or `localhost:4000` when a non-default port is part of the origin.
    #[must_use]
    pub fn domain(&self) -> &str {
        domain_of(&self.base_url)
    }

    /// Whether [`base_url`](Self::base_url) uses the `https` scheme.
    #[must_use]
    pub fn is_https(&self) -> bool {
        self.base_url.starts_with("https://")
    }

    /// Whether this instance keeps `area` from anonymous readers.
    ///
    /// `area` is one of the [`RESTRICT_UNAUTHENTICATED_AREAS`] names, and
    /// the refusal is Pleroma's 403 `{"error":"This API requires an
    /// authenticated user"}` (see [`crate::api::accounts`]' account status
    /// route). A request that presents a live user token is never refused
    /// by this: the restriction is about the anonymous reader, not about
    /// the read.
    ///
    /// An `area` outside [`RESTRICT_UNAUTHENTICATED_AREAS`] restricts
    /// nothing — naming one is a startup failure
    /// ([`ConfigError::InvalidRestrictUnauthenticated`]), so reaching this
    /// with one can only be a caller's mistake, never a silent hole.
    #[must_use]
    pub fn restricts(&self, area: &str) -> bool {
        self.restrict_unauthenticated.contains(area)
    }

    /// The [`OAUTH_PROVIDERS`](OAUTH_PROVIDERS_VAR) entry named `name`,
    /// matched case-sensitively against [`OauthProvider::name`]; `None` is
    /// the endpoints' "Unknown provider" answer.
    #[must_use]
    pub fn oauth_provider(&self, name: &str) -> Option<&OauthProvider> {
        self.oauth_providers
            .iter()
            .find(|provider| provider.name == name)
    }

    /// Configuration for tests: `BASE_URL` is `https://test.example`,
    /// `HTTP_ALLOW_PRIVATE_HOSTS` is `true` (the suites talk to local
    /// mini-servers), `RATE_LIMITS_ENABLED` is `false` (so a suite can
    /// exercise an endpoint's own budget without tripping the limiter —
    /// tests that want throttling read the var through `from_lookup`),
    /// every other setting is at its default, and
    /// `DATABASE_URL` is an inert placeholder (tests get their real pool
    /// from the fixture in [`crate::test_db`]). Reads no environment
    /// variables, so it is safe in parallel tests.
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
            RATE_LIMITS_ENABLED_VAR => Some("false".to_owned()),
            RICH_MEDIA_IGNORE_HOSTS_VAR => Some(DEFAULT_RICH_MEDIA_IGNORE_HOSTS.to_owned()),
            RICH_MEDIA_REMOTE_VAR => Some(DEFAULT_RICH_MEDIA_REMOTE.to_string()),
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
            frontend_base_url: TEST_BASE_URL.to_owned(),
            instance_name: DEFAULT_INSTANCE_NAME.to_owned(),
            instance_description: DEFAULT_INSTANCE_DESCRIPTION.to_owned(),
            contact_email: None,
            instance_languages: comma_list(DEFAULT_INSTANCE_LANGUAGES),
            max_status_chars: DEFAULT_MAX_STATUS_CHARS,
            max_media_attachments: DEFAULT_MAX_MEDIA_ATTACHMENTS,
            max_poll_options: DEFAULT_MAX_POLL_OPTIONS,
            max_poll_option_chars: DEFAULT_MAX_POLL_OPTION_CHARS,
            min_poll_expiration_secs: DEFAULT_MIN_POLL_EXPIRATION,
            max_poll_expiration_secs: DEFAULT_MAX_POLL_EXPIRATION,
            max_chat_message_chars: DEFAULT_MAX_CHAT_MESSAGE_CHARS,
            max_media_description: DEFAULT_MAX_MEDIA_DESCRIPTION,
            max_display_name_chars: DEFAULT_MAX_DISPLAY_NAME_CHARS,
            max_bio_chars: DEFAULT_MAX_BIO_CHARS,
            max_profile_fields: DEFAULT_MAX_PROFILE_FIELDS,
            max_report_comment: DEFAULT_MAX_REPORT_COMMENT,
            max_image_pixels: DEFAULT_MAX_IMAGE_PIXELS,
            max_pinned_statuses: DEFAULT_MAX_PINNED_STATUSES,
            min_expiration_secs: DEFAULT_MIN_EXPIRATION_SECS,
            min_scheduled_secs: DEFAULT_MIN_SCHEDULED_SECS,
            max_scheduled_per_user: DEFAULT_MAX_SCHEDULED_PER_USER,
            max_scheduled_per_user_per_day: DEFAULT_MAX_SCHEDULED_PER_USER_PER_DAY,
            upload_limits: TEST_UPLOAD_LIMITS,
            avatar_limit_bytes: DEFAULT_AVATAR_LIMIT_BYTES,
            banner_limit_bytes: DEFAULT_BANNER_LIMIT_BYTES,
            inbox_max_bytes: DEFAULT_INBOX_MAX_BYTES,
            max_remote_mention_lookups: DEFAULT_MAX_REMOTE_MENTION_LOOKUPS,
            uploads_dir: PathBuf::from(DEFAULT_UPLOADS_DIR),
            emoji_dir: PathBuf::from(DEFAULT_EMOJI_DIR),
            media_base_url: format!("{TEST_BASE_URL}{DEFAULT_MEDIA_URL_PATH}"),
            accounts_email_optional: DEFAULT_ACCOUNTS_EMAIL_OPTIONAL,
            min_password_length: DEFAULT_MIN_PASSWORD_LENGTH,
            oauth_token_ttl_secs: DEFAULT_OAUTH_TOKEN_TTL_SECS,
            password_reset_ttl_secs: DEFAULT_PASSWORD_RESET_TTL_SECS,
            account_activation_required: Toggle::new(DEFAULT_ACCOUNT_ACTIVATION_REQUIRED),
            allow_account_deletion: Toggle::new(DEFAULT_ALLOW_ACCOUNT_DELETION),
            mongooseim_enabled: Toggle::new(DEFAULT_MONGOOSEIM_ENABLED),
            captcha_enabled: Toggle::new(DEFAULT_CAPTCHA_ENABLED),
            healthcheck_enabled: Toggle::new(DEFAULT_HEALTHCHECK_ENABLED),
            registrations_open: Toggle::new(DEFAULT_REGISTRATIONS_OPEN),
            invites_enabled: Toggle::new(DEFAULT_INVITES_ENABLED),
            registrations_approval_required: Toggle::new(DEFAULT_REGISTRATIONS_APPROVAL_REQUIRED),
            search_statuses_unauthenticated: Toggle::new(DEFAULT_SEARCH_STATUSES_UNAUTHENTICATED),
            restrict_unauthenticated: BTreeSet::new(),
            oauth_providers: Vec::new(),
            available_frontends: comma_list(DEFAULT_AVAILABLE_FRONTENDS),
            job_concurrency: DEFAULT_JOB_CONCURRENCY,
            job_queues: comma_list(DEFAULT_JOB_QUEUES),
            purge_deleted_statuses_days: DEFAULT_PURGE_DELETED_STATUSES_DAYS,
            backup_validity_days: DEFAULT_BACKUP_VALIDITY_DAYS,
            backup_limit_days: DEFAULT_BACKUP_LIMIT_DAYS,
            backup_purge: Toggle::new(DEFAULT_BACKUP_PURGE),
            streaming_ping_interval_secs: DEFAULT_STREAMING_PING_INTERVAL_SECS,
            smtp_host: None,
            smtp_port: DEFAULT_SMTP_PORT,
            smtp_username: None,
            smtp_password: None,
            smtp_tls: SmtpTls::default(),
            mail_from: default_mail_from(domain_of(TEST_BASE_URL)),
            media_proxy: TEST_MEDIA_PROXY,
            statics: test_statics(),
            rich_media_ignore_hosts: comma_list(DEFAULT_RICH_MEDIA_IGNORE_HOSTS),
            rich_media_remote: Toggle::new(DEFAULT_RICH_MEDIA_REMOTE),
            vapid: VapidConfig::default(),
            translation: TranslationConfig::default(),
            fetch: test_fetch(),
            federation_enabled: Toggle::new(DEFAULT_FEDERATION_ENABLED),
            accept_unsolicited_public: Toggle::new(DEFAULT_ACCEPT_UNSOLICITED_PUBLIC),
            unreachable_grace_secs: DEFAULT_UNREACHABLE_GRACE_SECS,
            mrf: MrfSettings::test_defaults(),
            rate_limit: test_rate_limit(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::test_support::{ORIGIN, config_with, lookup_from};

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
        assert_eq!(
            config.frontend_base_url, "https://test.example",
            "test frontend_base_url defaults onto the test base_url"
        );
        assert_eq!(config.database_url, TEST_DATABASE_URL, "placeholder url");
        assert_eq!(config.domain(), "test.example", "test domain");
        assert!(config.is_https(), "test base_url is https");
        assert!(
            config.fetch.http_allow_private_hosts,
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
            config.emoji_dir,
            PathBuf::from(DEFAULT_EMOJI_DIR),
            "test packs are read from the default directory"
        );
        assert_eq!(
            config.media_base_url, "https://test.example/media",
            "test media URLs hang off the test BASE_URL"
        );
        assert_eq!(config.smtp_host, None, "test config has mail disabled");
        assert_eq!(
            config.mail_from, "noreply@test.example",
            "test mail defaults to the test domain"
        );
        assert!(
            !config.media_proxy.enabled,
            "the media proxy is off for tests by default, so entity URLs pass through"
        );
        assert!(
            config.rich_media_ignore_hosts.is_empty(),
            "the test config ignores no rich media hosts"
        );
        assert!(
            !config.rich_media_remote.get(),
            "rich media cards fetch no remote URLs for tests by default"
        );
        let expected = Config::from_lookup(lookup_from(&[
            (DATABASE_URL_VAR, TEST_DATABASE_URL),
            (BASE_URL_VAR, TEST_BASE_URL),
            (HTTP_ALLOW_PRIVATE_HOSTS_VAR, "true"),
            (RATE_LIMITS_ENABLED_VAR, "false"),
            (RICH_MEDIA_IGNORE_HOSTS_VAR, DEFAULT_RICH_MEDIA_IGNORE_HOSTS),
            (RICH_MEDIA_REMOTE_VAR, "false"),
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
            (RATE_LIMITS_ENABLED_VAR, "false"),
            (RICH_MEDIA_IGNORE_HOSTS_VAR, DEFAULT_RICH_MEDIA_IGNORE_HOSTS),
            (RICH_MEDIA_REMOTE_VAR, "false"),
        ]))
        .unwrap();
        assert_eq!(
            Config::test_defaults(),
            parsed,
            "fallback equals the parsed test config"
        );
    }
}
