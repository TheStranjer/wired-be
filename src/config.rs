//! Runtime configuration, read from environment variables.

use std::{
    error::Error,
    fmt,
    net::{AddrParseError, SocketAddr},
    path::PathBuf,
};

use url::Url;

use crate::{extract::multipart::UploadLimits, jobs::DEFAULT_JOB_QUEUES};

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
/// Environment variable holding the maximum chat message length in
/// characters.
pub const MAX_CHAT_MESSAGE_CHARS_VAR: &str = "MAX_CHAT_MESSAGE_CHARS";
/// Environment variable holding the maximum media description length in
/// characters.
pub const MAX_MEDIA_DESCRIPTION_VAR: &str = "MAX_MEDIA_DESCRIPTION";
/// Environment variable holding the maximum size of one uploaded file.
pub const UPLOAD_LIMIT_BYTES_VAR: &str = "UPLOAD_LIMIT_BYTES";
/// Environment variable holding the maximum number of files per request.
pub const UPLOAD_MAX_FILES_VAR: &str = "UPLOAD_MAX_FILES";
/// Environment variable holding whether outbound HTTP may reach private
/// hosts (disabling the SSRF guard).
pub const HTTP_ALLOW_PRIVATE_HOSTS_VAR: &str = "HTTP_ALLOW_PRIVATE_HOSTS";
/// Environment variable holding the directory uploaded files are stored in.
pub const UPLOADS_DIR_VAR: &str = "UPLOADS_DIR";
/// Environment variable holding the directory emoji packs are stored in.
pub const EMOJI_DIR_VAR: &str = "EMOJI_DIR";
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
/// Environment variable holding whether the `MongooseIM` auth endpoints
/// (`GET /user_exists`, `GET /check_password`) are served.
pub const MONGOOSEIM_ENABLED_VAR: &str = "MONGOOSEIM_ENABLED";
/// Environment variable holding whether a captcha provider is configured
/// for `GET /api/v1/pleroma/captcha`.
pub const CAPTCHA_ENABLED_VAR: &str = "CAPTCHA_ENABLED";
/// Environment variable holding whether `GET /api/v1/pleroma/healthcheck`
/// reports pool and liveness data.
pub const HEALTHCHECK_ENABLED_VAR: &str = "HEALTHCHECK_ENABLED";
/// Environment variable holding whether the instance accepts new local
/// registrations; `NodeInfo` reports it as `openRegistrations`.
pub const REGISTRATIONS_OPEN_VAR: &str = "REGISTRATIONS_OPEN";
/// Environment variable holding the external OAuth login providers, as the
/// JSON text of an [`OauthProvider`] array.
pub const OAUTH_PROVIDERS_VAR: &str = "OAUTH_PROVIDERS";
/// Environment variable holding the comma-separated list of frontends
/// clients may pick at `PUT /api/v1/pleroma/preferred_frontend`.
pub const AVAILABLE_FRONTENDS_VAR: &str = "AVAILABLE_FRONTENDS";
/// Environment variable holding the job worker's concurrent handler count.
pub const JOB_CONCURRENCY_VAR: &str = "JOB_CONCURRENCY";
/// Environment variable holding the comma-separated job queue names.
pub const JOB_QUEUES_VAR: &str = "JOB_QUEUES";
/// Environment variable holding the SMTP relay host; mail is disabled while
/// it is unset.
pub const SMTP_HOST_VAR: &str = "SMTP_HOST";
/// Environment variable holding the SMTP relay port.
pub const SMTP_PORT_VAR: &str = "SMTP_PORT";
/// Environment variable holding the SMTP relay login name.
pub const SMTP_USERNAME_VAR: &str = "SMTP_USERNAME";
/// Environment variable holding the SMTP relay login secret.
pub const SMTP_PASSWORD_VAR: &str = "SMTP_PASSWORD";
/// Environment variable holding the SMTP encryption mode.
pub const SMTP_TLS_VAR: &str = "SMTP_TLS";
/// Environment variable holding the `From:` address of every sent email.
pub const MAIL_FROM_VAR: &str = "MAIL_FROM";
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
/// Chat message length used when `MAX_CHAT_MESSAGE_CHARS` is unset.
///
/// The 5000 characters of Pleroma's `[:instance, :chat_limit]` default —
/// the same number the chat model's storage backstop keeps, pinned together
/// by a test in this module.
pub const DEFAULT_MAX_CHAT_MESSAGE_CHARS: usize = 5_000;
/// Media description length used when `MAX_MEDIA_DESCRIPTION` is unset.
///
/// It is the 1500 characters Mastodon caps an attachment's alt text at
/// (`MAX_ALTERNATIVE_TEXT_SIZE` in `config/settings.yml`);
/// `PUT /api/v1/media/{id}` refuses a longer `description`.
pub const DEFAULT_MAX_MEDIA_DESCRIPTION: usize = 1_500;
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
/// Directory emoji packs live in when `EMOJI_DIR` is unset, relative to
/// the working directory.
///
/// Pleroma keeps them in an `emoji` subdirectory of its static dir
/// (`lib/pleroma/emoji/pack.ex`), one directory per pack.
pub const DEFAULT_EMOJI_DIR: &str = "./emoji";
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
/// `MongooseIM` auth endpoint state used when `MONGOOSEIM_ENABLED` is
/// unset: the endpoints answer the standard fallback 404 until an operator
/// turns them on for their `extauth` backend.
pub const DEFAULT_MONGOOSEIM_ENABLED: bool = false;
/// Captcha state used when `CAPTCHA_ENABLED` is unset: no captcha provider
/// is configured, so `GET /api/v1/pleroma/captcha` reports `{"type":"none"}`
/// like Pleroma with no captcha plugin.
pub const DEFAULT_CAPTCHA_ENABLED: bool = false;
/// Healthcheck state used when `HEALTHCHECK_ENABLED` is unset: the
/// endpoint answers the standard fallback 404 until it is turned on.
pub const DEFAULT_HEALTHCHECK_ENABLED: bool = false;
/// Registration state used when `REGISTRATIONS_OPEN` is unset: Pleroma's
/// `[:instance, :registrations_open]` default is open, so closing
/// registration is always an explicit operator decision.
pub const DEFAULT_REGISTRATIONS_OPEN: bool = true;
/// The provider list used when `OAUTH_PROVIDERS` is unset: no external
/// login provider is configured, so `GET /oauth/prepare_request` and the
/// provider endpoints answer their errors for every name.
pub const DEFAULT_OAUTH_PROVIDERS: &str = "[]";
/// Frontend list used when `AVAILABLE_FRONTENDS` is unset: nothing is pickable.
///
/// So `GET /api/v1/pleroma/preferred_frontend/available` answers `[]` and
/// `PUT /api/v1/pleroma/preferred_frontend` refuses every name.
pub const DEFAULT_AVAILABLE_FRONTENDS: &str = "";
/// Worker concurrency used when `JOB_CONCURRENCY` is unset.
pub const DEFAULT_JOB_CONCURRENCY: usize = 10;
/// SMTP relay port used when `SMTP_PORT` is unset: the `SUBMISSION` port,
/// where a relay accepts authenticated client connections.
pub const DEFAULT_SMTP_PORT: u16 = 587;
/// Sender user part used when `MAIL_FROM` is unset: the `noreply` of the
/// default address, attached to the instance domain, so an unset `MAIL_FROM`
/// on `social.example` sends as `noreply@social.example`.
pub const DEFAULT_MAIL_FROM_USER: &str = "noreply";
/// `DATABASE_URL` used by [`Config::for_tests`]: a placeholder URL that
/// points at nothing; integration tests get their real pool from
/// `#[sqlx::test]`.
#[cfg(any(test, feature = "integration-tests"))]
pub const TEST_DATABASE_URL: &str = "postgres://localhost/wired_test";
/// `BASE_URL` used by [`Config::for_tests`].
#[cfg(any(test, feature = "integration-tests"))]
pub const TEST_BASE_URL: &str = "https://test.example";

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

/// One external OAuth login provider, as one element of the
/// `OAUTH_PROVIDERS` JSON array (Pleroma's `config :ueberauth, Ueberauth,
/// providers:` list, expressed as data instead of compiled-in strategies).
///
/// The four `*_field` names say where the provider's userinfo document
/// carries each fact; a dotted path digs into nested objects (`"data.id"`),
/// and an empty name means the provider does not tell us that much.
/// `client_secret` is secret material: the manual `Debug` impl below
/// redacts it, so a configuration written to the log never carries it.
#[derive(Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OauthProvider {
    /// Provider name: the `{provider}` path segment and the value of the
    /// `provider` parameter, e.g. `github`.
    pub name: String,
    /// The id this instance registered with the provider, sent as
    /// `client_id`.
    pub client_id: String,
    /// The provider's client secret, sent to the token endpoint. Never in
    /// `Debug` output.
    pub client_secret: String,
    /// Where the browser is redirected to authorize us, sent the provider
    /// `client_id`, our callback URL and our state.
    pub authorize_url: String,
    /// Where the returned code is exchanged for an `access_token`.
    pub token_url: String,
    /// Where the `access_token` fetches the account document from.
    pub userinfo_url: String,
    /// The `scope` string sent to the provider (its own vocabulary, not
    /// Mastodon scopes); empty sends none.
    #[serde(default)]
    pub scopes: String,
    /// Dotted path of the stable provider account id in the userinfo
    /// document, the key `user_identities.uid` stores.
    #[serde(default = "default_uid_field")]
    pub uid_field: String,
    /// Dotted path of the login name, offered to the registration form.
    /// Empty means the document carries no such name.
    #[serde(default)]
    pub nickname_field: String,
    /// Dotted path of the email address, offered to the registration form.
    #[serde(default)]
    pub email_field: String,
    /// Dotted path of the display name, offered to the registration form.
    #[serde(default)]
    pub name_field: String,
}

/// [`OauthProvider::uid_field`] when a provider object omits it: the
/// top-level `id` key, the spelling the majority of providers use for the
/// stable account id (GitHub's `id`; a provider that names it otherwise,
/// like Keycloak's `sub`, says so in its own object).
fn default_uid_field() -> String {
    "id".to_owned()
}

/// [`client_secret`](Self::client_secret) is secret material and renders as
/// `<redacted>`; everything else shows.
impl fmt::Debug for OauthProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OauthProvider")
            .field("name", &self.name)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("authorize_url", &self.authorize_url)
            .field("token_url", &self.token_url)
            .field("userinfo_url", &self.userinfo_url)
            .field("scopes", &self.scopes)
            .field("uid_field", &self.uid_field)
            .field("nickname_field", &self.nickname_field)
            .field("email_field", &self.email_field)
            .field("name_field", &self.name_field)
            .finish()
    }
}

/// How the SMTP relay connection is encrypted, the value of `SMTP_TLS`.
///
/// Parsed by the `SMTP_TLS` reader in `mailer_settings`; the unset or blank
/// default is [`Starttls`](Self::Starttls), the mode submission-port relays
/// speak.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SmtpTls {
    /// Plain connection upgraded with STARTTLS, required before any
    /// credential is sent.
    #[default]
    Starttls,
    /// Implicit TLS for the whole connection (SMTPS, traditionally port
    /// 465).
    Tls,
    /// No encryption at all, for a trusted local relay.
    None,
}

impl SmtpTls {
    /// The mode an `SMTP_TLS` value names, case-insensitively and trimmed;
    /// `None` for any other spelling.
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "starttls" => Some(Self::Starttls),
            "tls" => Some(Self::Tls),
            "none" => Some(Self::None),
            _ => None,
        }
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
    /// Maximum length of a chat message in characters, from
    /// `MAX_CHAT_MESSAGE_CHARS`; `POST /api/v1/pleroma/chats/{id}/messages`
    /// refuses longer content.
    pub max_chat_message_chars: usize,
    /// Maximum length of a media attachment's `description` (alt text) in
    /// characters, from `MAX_MEDIA_DESCRIPTION`; `PUT /api/v1/media/{id}`
    /// refuses longer values.
    pub max_media_description: usize,
    /// Per-request limits for multipart uploads, from `UPLOAD_LIMIT_BYTES`
    /// and `UPLOAD_MAX_FILES`.
    pub upload_limits: UploadLimits,
    /// Directory uploaded files are stored in, from `UPLOADS_DIR`.
    pub uploads_dir: PathBuf,
    /// Directory emoji packs are stored in, from `EMOJI_DIR`: one
    /// subdirectory per pack holding a `pack.json` and its image files
    /// (see [`crate::services::emoji_packs`]).
    pub emoji_dir: PathBuf,
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
    /// Whether the `MongooseIM` auth endpoints (`GET /user_exists`,
    /// `GET /check_password`) are served, from `MONGOOSEIM_ENABLED`. See
    /// [`crate::api::misc`].
    pub mongooseim_enabled: Toggle,
    /// Whether a captcha provider is configured, from `CAPTCHA_ENABLED`.
    /// While no captcha implementation exists, a set toggle makes
    /// `GET /api/v1/pleroma/captcha` answer 501; see [`crate::api::misc`].
    pub captcha_enabled: Toggle,
    /// Whether `GET /api/v1/pleroma/healthcheck` reports pool and
    /// liveness data, from `HEALTHCHECK_ENABLED`. See [`crate::api::misc`].
    pub healthcheck_enabled: Toggle,
    /// Whether the instance accepts new local registrations, from
    /// `REGISTRATIONS_OPEN`; `NodeInfo` reports it as `openRegistrations`
    /// (see [`crate::api::nodeinfo`]).
    pub registrations_open: Toggle,
    /// The external OAuth login providers, from `OAUTH_PROVIDERS`; empty
    /// when none is configured. Each one answers the `/oauth` provider
    /// endpoints under its own name (see [`crate::api::oauth_external`]).
    pub oauth_providers: Vec<OauthProvider>,
    /// The frontends clients may pick, from `AVAILABLE_FRONTENDS` (a
    /// comma-separated list, empty by default): the answer of
    /// `GET /api/v1/pleroma/preferred_frontend/available` and the whitelist
    /// `PUT /api/v1/pleroma/preferred_frontend` validates against.
    pub available_frontends: Vec<String>,
    /// Job handlers a single worker runs at the same time.
    pub job_concurrency: usize,
    /// Job queue names the worker drains, in claim priority order.
    pub job_queues: Vec<String>,
    /// SMTP relay host the mailer connects to, from `SMTP_HOST`; `None`
    /// (the default) disables mail entirely, so nothing is even queued.
    /// See [`crate::mailer`].
    pub smtp_host: Option<String>,
    /// SMTP relay port, from `SMTP_PORT`; 587, the submission port, by
    /// default. With [`SmtpTls::Tls`] an operator usually sets 465.
    pub smtp_port: u16,
    /// SMTP relay login name, from `SMTP_USERNAME`; `None` sends
    /// unauthenticated.
    pub smtp_username: Option<String>,
    /// SMTP relay login secret, from `SMTP_PASSWORD`. Secret material: the
    /// manual `Debug` impl below redacts it.
    pub smtp_password: Option<String>,
    /// How the relay connection is encrypted, from `SMTP_TLS`
    /// ([`SmtpTls::Starttls`] by default).
    pub smtp_tls: SmtpTls,
    /// The `From:` address every sent email carries, from `MAIL_FROM`;
    /// `noreply@<instance domain>` by default.
    pub mail_from: String,
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
        let (instance_name, instance_description, contact_email, instance_languages) =
            instance_profile(&lookup);
        let (
            max_status_chars,
            max_media_attachments,
            max_poll_options,
            max_chat_message_chars,
            max_media_description,
        ) = content_limits(&lookup)?;
        let upload_limits = UploadLimits {
            max_file_bytes: number(&lookup, UPLOAD_LIMIT_BYTES_VAR, DEFAULT_UPLOAD_LIMIT_BYTES)?,
            max_files: number(&lookup, UPLOAD_MAX_FILES_VAR, DEFAULT_UPLOAD_MAX_FILES)?,
        };
        let http_allow_private_hosts = boolean(
            &lookup,
            HTTP_ALLOW_PRIVATE_HOSTS_VAR,
            DEFAULT_HTTP_ALLOW_PRIVATE_HOSTS,
        )?;
        let (uploads_dir, emoji_dir, media_base_url) = storage_paths(&lookup, &base_url);
        let (accounts_email_optional, min_password_length, oauth_token_ttl_secs, oauth_providers) =
            auth_settings(&lookup)?;
        let (mongooseim_enabled, captcha_enabled, healthcheck_enabled) = misc_toggles(&lookup)?;
        let (registrations_open, available_frontends) = client_exposure(&lookup)?;
        let (job_concurrency, job_queues) = job_worker(&lookup)?;
        let (smtp_host, smtp_port, smtp_username, smtp_password, smtp_tls, mail_from) =
            mailer_settings(&lookup, domain_of(&base_url))?;
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
            max_chat_message_chars,
            max_media_description,
            upload_limits,
            uploads_dir,
            emoji_dir,
            media_base_url,
            http_allow_private_hosts,
            accounts_email_optional,
            min_password_length,
            oauth_token_ttl_secs,
            mongooseim_enabled,
            captcha_enabled,
            healthcheck_enabled,
            registrations_open,
            oauth_providers,
            available_frontends,
            job_concurrency,
            job_queues,
            smtp_host,
            smtp_port,
            smtp_username,
            smtp_password,
            smtp_tls,
            mail_from,
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
            max_chat_message_chars: DEFAULT_MAX_CHAT_MESSAGE_CHARS,
            max_media_description: DEFAULT_MAX_MEDIA_DESCRIPTION,
            upload_limits: UploadLimits {
                max_file_bytes: DEFAULT_UPLOAD_LIMIT_BYTES,
                max_files: DEFAULT_UPLOAD_MAX_FILES,
            },
            uploads_dir: PathBuf::from(DEFAULT_UPLOADS_DIR),
            emoji_dir: PathBuf::from(DEFAULT_EMOJI_DIR),
            media_base_url: format!("{TEST_BASE_URL}{DEFAULT_MEDIA_URL_PATH}"),
            http_allow_private_hosts: true,
            accounts_email_optional: DEFAULT_ACCOUNTS_EMAIL_OPTIONAL,
            min_password_length: DEFAULT_MIN_PASSWORD_LENGTH,
            oauth_token_ttl_secs: DEFAULT_OAUTH_TOKEN_TTL_SECS,
            mongooseim_enabled: Toggle::new(DEFAULT_MONGOOSEIM_ENABLED),
            captcha_enabled: Toggle::new(DEFAULT_CAPTCHA_ENABLED),
            healthcheck_enabled: Toggle::new(DEFAULT_HEALTHCHECK_ENABLED),
            registrations_open: Toggle::new(DEFAULT_REGISTRATIONS_OPEN),
            oauth_providers: Vec::new(),
            available_frontends: comma_list(DEFAULT_AVAILABLE_FRONTENDS),
            job_concurrency: DEFAULT_JOB_CONCURRENCY,
            job_queues: comma_list(DEFAULT_JOB_QUEUES),
            smtp_host: None,
            smtp_port: DEFAULT_SMTP_PORT,
            smtp_username: None,
            smtp_password: None,
            smtp_tls: SmtpTls::default(),
            mail_from: default_mail_from(domain_of(TEST_BASE_URL)),
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

/// What [`instance_profile`] reads, in [`Config`] field order: the display
/// name, the short description, the admin contact email and the content
/// language codes. Grouped because each is a plain default-or-list and
/// [`Config::from_lookup`] has a size budget (AGENTS.md); the tuple comes
/// back in field order.
fn instance_profile<F>(lookup: &F) -> (String, String, Option<String>, Vec<String>)
where
    F: Fn(&str) -> Option<String>,
{
    let instance_name =
        lookup(INSTANCE_NAME_VAR).unwrap_or_else(|| DEFAULT_INSTANCE_NAME.to_owned());
    let instance_description =
        lookup(INSTANCE_DESCRIPTION_VAR).unwrap_or_else(|| DEFAULT_INSTANCE_DESCRIPTION.to_owned());
    let contact_email = lookup(CONTACT_EMAIL_VAR).filter(|email| !email.is_empty());
    let instance_languages = comma_list(
        &lookup(INSTANCE_LANGUAGES_VAR).unwrap_or_else(|| DEFAULT_INSTANCE_LANGUAGES.to_owned()),
    );
    (
        instance_name,
        instance_description,
        contact_email,
        instance_languages,
    )
}

/// Reads the two settings that shape the job worker: how many handlers a
/// single worker runs at the same time, and the queue names it drains, in
/// claim priority order. Grouped because each is a plain default-or-parse
/// and [`Config::from_lookup`] has a size budget (AGENTS.md); the tuple
/// comes back in field order.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when `JOB_CONCURRENCY` holds a
/// value that is not a whole number.
fn job_worker<F>(lookup: &F) -> Result<(usize, Vec<String>), ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let job_concurrency = number(lookup, JOB_CONCURRENCY_VAR, DEFAULT_JOB_CONCURRENCY)?;
    let job_queues =
        comma_list(&lookup(JOB_QUEUES_VAR).unwrap_or_else(|| DEFAULT_JOB_QUEUES.to_owned()));
    Ok((job_concurrency, job_queues))
}

/// What [`content_limits`] reads, in [`Config`] field order: the status
/// length, the attachment and poll-option limits, the chat message length and
/// the media description length.
type ContentLimits = (usize, usize, usize, usize, usize);

/// Reads the five plain numeric content limits: the maximum status length,
/// media attachment count, poll option count, chat message length and media
/// description length. Grouped because each is the same `number` read and
/// [`Config::from_lookup`] has a size budget (AGENTS.md); the tuple comes
/// back in field order.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] for whichever variable holds a
/// value that is not a whole number.
fn content_limits<F>(lookup: &F) -> Result<ContentLimits, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok((
        number(lookup, MAX_STATUS_CHARS_VAR, DEFAULT_MAX_STATUS_CHARS)?,
        number(
            lookup,
            MAX_MEDIA_ATTACHMENTS_VAR,
            DEFAULT_MAX_MEDIA_ATTACHMENTS,
        )?,
        number(lookup, MAX_POLL_OPTIONS_VAR, DEFAULT_MAX_POLL_OPTIONS)?,
        number(
            lookup,
            MAX_CHAT_MESSAGE_CHARS_VAR,
            DEFAULT_MAX_CHAT_MESSAGE_CHARS,
        )?,
        number(
            lookup,
            MAX_MEDIA_DESCRIPTION_VAR,
            DEFAULT_MAX_MEDIA_DESCRIPTION,
        )?,
    ))
}

/// What [`mailer_settings`] reads, in [`Config`] field order: the relay
/// host, the relay port, the login name and secret, the encryption mode and
/// the sender address.
type MailerSettings = (
    Option<String>,
    u16,
    Option<String>,
    Option<String>,
    SmtpTls,
    String,
);

/// Reads the six settings that configure outbound mail: the relay host
/// (unset means mail is disabled), the relay port, the login name and
/// secret, the encryption mode and the sender address. Grouped because each
/// is a plain default-or-parse and [`Config::from_lookup`] has a size budget
/// (AGENTS.md); the tuple comes back in field order. `domain` is the
/// [`domain_of`] the default sender address is built from.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when `SMTP_PORT` is not a whole
/// number in the `u16` range, and [`ConfigError::InvalidSmtpTls`] when
/// `SMTP_TLS` names no known mode.
fn mailer_settings<F>(lookup: &F, domain: &str) -> Result<MailerSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let smtp_host = lookup(SMTP_HOST_VAR)
        .map(|host| host.trim().to_owned())
        .filter(|host| !host.is_empty());
    let smtp_port = smtp_port(lookup)?;
    let smtp_username = lookup(SMTP_USERNAME_VAR)
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty());
    let smtp_password = lookup(SMTP_PASSWORD_VAR).filter(|secret| !secret.is_empty());
    let smtp_tls = smtp_tls(lookup)?;
    let mail_from = lookup(MAIL_FROM_VAR)
        .map(|from| from.trim().to_owned())
        .filter(|from| !from.is_empty())
        .unwrap_or_else(|| default_mail_from(domain));
    Ok((
        smtp_host,
        smtp_port,
        smtp_username,
        smtp_password,
        smtp_tls,
        mail_from,
    ))
}

/// Reads `SMTP_PORT`: a whole number in the `u16` range,
/// [`DEFAULT_SMTP_PORT`] when unset.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] for a non-number or a value
/// outside the port range.
fn smtp_port<F>(lookup: &F) -> Result<u16, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let port = number(lookup, SMTP_PORT_VAR, usize::from(DEFAULT_SMTP_PORT))?;
    u16::try_from(port).map_err(|_out_of_range| ConfigError::InvalidNumber(SMTP_PORT_VAR))
}

/// Reads `SMTP_TLS`: one of the [`SmtpTls`] spellings, case-insensitive,
/// [`SmtpTls::default`] when unset or blank.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidSmtpTls`] for any other spelling.
fn smtp_tls<F>(lookup: &F) -> Result<SmtpTls, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = lookup(SMTP_TLS_VAR).filter(|value| !value.trim().is_empty()) else {
        return Ok(SmtpTls::default());
    };
    SmtpTls::parse(&raw).ok_or(ConfigError::InvalidSmtpTls(SMTP_TLS_VAR))
}

/// The [`Config::domain`] of a normalized base URL: everything after the
/// `://`.
fn domain_of(base_url: &str) -> &str {
    base_url
        .split_once("://")
        .map_or(base_url, |(_, domain)| domain)
}

/// The `MAIL_FROM` value used when the variable is unset:
/// `noreply@<instance domain>`. A port is not part of an email domain, so
/// `localhost:4000` contributes only `localhost`.
fn default_mail_from(domain: &str) -> String {
    let host = domain.split_once(':').map_or(domain, |(host, _)| host);
    format!("{DEFAULT_MAIL_FROM_USER}@{host}")
}

/// What [`auth_settings`] reads, in [`Config`] field order: the email
/// requirement, the password minimum, the token lifetime and the providers.
type AuthSettings = (bool, usize, usize, Vec<OauthProvider>);

/// Reads the four settings that gate registration, token issuance and the
/// external logins: whether registration may omit the email address, the
/// registration password minimum, the access-token lifetime and the
/// provider list. Grouped because each is a plain default-or-parse and
/// [`Config::from_lookup`] has a size budget (AGENTS.md); the tuple comes
/// back in field order.
fn auth_settings<F>(lookup: &F) -> Result<AuthSettings, ConfigError>
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
        oauth_providers(lookup)?,
    ))
}

/// Reads the three settings that locate stored files and the URLs they
/// are served under: the uploads directory, the emoji packs directory and
/// the media URL prefix. Grouped because each is a plain trim-or-default
/// and [`Config::from_lookup`] has a size budget (AGENTS.md); the tuple
/// comes back in field order. `base_url` is the normalized origin the
/// media prefix defaults onto.
fn storage_paths<F>(lookup: &F, base_url: &str) -> (PathBuf, PathBuf, String)
where
    F: Fn(&str) -> Option<String>,
{
    let uploads_dir = PathBuf::from(
        lookup(UPLOADS_DIR_VAR)
            .map(|dir| dir.trim().to_owned())
            .filter(|dir| !dir.is_empty())
            .unwrap_or_else(|| DEFAULT_UPLOADS_DIR.to_owned()),
    );
    let emoji_dir = PathBuf::from(
        lookup(EMOJI_DIR_VAR)
            .map(|dir| dir.trim().to_owned())
            .filter(|dir| !dir.is_empty())
            .unwrap_or_else(|| DEFAULT_EMOJI_DIR.to_owned()),
    );
    let media_base_url = lookup(MEDIA_BASE_URL_VAR)
        .map(|url| url.trim().trim_end_matches('/').to_owned())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| format!("{base_url}{DEFAULT_MEDIA_URL_PATH}"));
    (uploads_dir, emoji_dir, media_base_url)
}

/// Reads the three feature toggles that gate the endpoints of
/// [`crate::api::misc`]: the `MongooseIM` auth endpoints, the captcha
/// provider and the healthcheck. Grouped because each is a plain
/// default-or-parse and [`Config::from_lookup`] has a size budget
/// (AGENTS.md); the tuple comes back in field order.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] for whichever variable holds a
/// value that is neither spelling.
fn misc_toggles<F>(lookup: &F) -> Result<(Toggle, Toggle, Toggle), ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok((
        boolean(lookup, MONGOOSEIM_ENABLED_VAR, DEFAULT_MONGOOSEIM_ENABLED)?.into(),
        boolean(lookup, CAPTCHA_ENABLED_VAR, DEFAULT_CAPTCHA_ENABLED)?.into(),
        boolean(lookup, HEALTHCHECK_ENABLED_VAR, DEFAULT_HEALTHCHECK_ENABLED)?.into(),
    ))
}

/// Reads the two settings that gate what the instance exposes to clients:
/// whether new registrations are open (`NodeInfo`'s `openRegistrations`)
/// and the frontends the settings endpoints let a user pick. Grouped
/// because each is a plain default-or-parse and [`Config::from_lookup`]
/// has a size budget (AGENTS.md); the tuple comes back in field order.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] when `REGISTRATIONS_OPEN` holds
/// a value that is neither spelling.
fn client_exposure<F>(lookup: &F) -> Result<(Toggle, Vec<String>), ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let registrations_open =
        boolean(lookup, REGISTRATIONS_OPEN_VAR, DEFAULT_REGISTRATIONS_OPEN)?.into();
    let available_frontends = comma_list(
        &lookup(AVAILABLE_FRONTENDS_VAR).unwrap_or_else(|| DEFAULT_AVAILABLE_FRONTENDS.to_owned()),
    );
    Ok((registrations_open, available_frontends))
}

/// Reads `OAUTH_PROVIDERS`: the JSON text of the external OAuth login
/// provider array, [`DEFAULT_OAUTH_PROVIDERS`] when the variable is unset
/// or blank.
///
/// # Errors
///
/// [`ConfigError::InvalidOauthProviders`] when the text is not a JSON array
/// of provider objects; the parser's own wording rides along so an operator
/// can see which key broke.
fn oauth_providers<F>(lookup: &F) -> Result<Vec<OauthProvider>, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let configured = lookup(OAUTH_PROVIDERS_VAR).unwrap_or_default();
    let text = if configured.trim().is_empty() {
        DEFAULT_OAUTH_PROVIDERS
    } else {
        configured.as_str()
    };
    serde_json::from_str(text).map_err(|err| ConfigError::InvalidOauthProviders(err.to_string()))
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
            .field("max_chat_message_chars", &self.max_chat_message_chars)
            .field("max_media_description", &self.max_media_description)
            .field("upload_limits", &self.upload_limits)
            .field("http_allow_private_hosts", &self.http_allow_private_hosts)
            .field("uploads_dir", &self.uploads_dir)
            .field("emoji_dir", &self.emoji_dir)
            .field("media_base_url", &self.media_base_url)
            .field("accounts_email_optional", &self.accounts_email_optional)
            .field("min_password_length", &self.min_password_length)
            .field("oauth_token_ttl_secs", &self.oauth_token_ttl_secs)
            .field("mongooseim_enabled", &self.mongooseim_enabled)
            .field("captcha_enabled", &self.captcha_enabled)
            .field("healthcheck_enabled", &self.healthcheck_enabled)
            .field("registrations_open", &self.registrations_open)
            .field("oauth_providers", &self.oauth_providers)
            .field("available_frontends", &self.available_frontends)
            .field("job_concurrency", &self.job_concurrency)
            .field("job_queues", &self.job_queues)
            .field("smtp_host", &self.smtp_host)
            .field("smtp_port", &self.smtp_port)
            .field("smtp_username", &self.smtp_username)
            .field("smtp_password", &"<redacted>")
            .field("smtp_tls", &self.smtp_tls)
            .field("mail_from", &self.mail_from)
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
    /// `OAUTH_PROVIDERS` is not the JSON text of an array of provider
    /// objects; the value is what the JSON parser said.
    InvalidOauthProviders(String),
    /// `SMTP_TLS` is not one of `starttls`, `tls` or `none`; the value is the
    /// variable name.
    InvalidSmtpTls(&'static str),
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
            | Self::InvalidSmtpTls(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

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
            !config.http_allow_private_hosts,
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
            config.http_allow_private_hosts,
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
    fn job_worker_settings_default() {
        let config = config_with(&[]);
        assert_eq!(config.job_concurrency, 10, "default job_concurrency");
        assert_eq!(
            config.job_queues,
            vec![
                "default".to_owned(),
                "federator_incoming".to_owned(),
                "federator_outgoing".to_owned(),
                "mailer".to_owned(),
                "scheduled".to_owned(),
                "background".to_owned(),
            ],
            "default job_queues"
        );
    }

    #[test]
    fn reads_custom_job_worker_settings() {
        let config = config_with(&[
            (JOB_CONCURRENCY_VAR, "3"),
            (JOB_QUEUES_VAR, " default , mailer ,,scheduled"),
        ]);
        assert_eq!(config.job_concurrency, 3, "custom job_concurrency");
        assert_eq!(
            config.job_queues,
            vec![
                "default".to_owned(),
                "mailer".to_owned(),
                "scheduled".to_owned(),
            ],
            "comma list is trimmed, empty entries dropped"
        );
    }

    #[test]
    fn mailer_settings_default_to_disabled_mail() {
        let config = config_with(&[]);
        assert_eq!(config.smtp_host, None, "mail is disabled by default");
        assert_eq!(config.smtp_port, 587, "default SMTP_PORT is submission");
        assert_eq!(config.smtp_username, None, "default SMTP_USERNAME");
        assert_eq!(config.smtp_password, None, "default SMTP_PASSWORD");
        assert_eq!(config.smtp_tls, SmtpTls::Starttls, "default SMTP_TLS");
        assert_eq!(
            config.mail_from, "noreply@social.example",
            "MAIL_FROM defaults to noreply at the instance domain"
        );
    }

    #[test]
    fn reads_custom_mailer_settings() {
        let config = config_with(&[
            (SMTP_HOST_VAR, " smtp.example.org "),
            (SMTP_PORT_VAR, "465"),
            (SMTP_USERNAME_VAR, " relay-user "),
            (SMTP_PASSWORD_VAR, "relay-secret"),
            (SMTP_TLS_VAR, "tls"),
            (MAIL_FROM_VAR, " hello@example.org "),
        ]);
        assert_eq!(
            config.smtp_host.as_deref(),
            Some("smtp.example.org"),
            "custom SMTP_HOST, trimmed"
        );
        assert_eq!(config.smtp_port, 465, "custom SMTP_PORT");
        assert_eq!(
            config.smtp_username.as_deref(),
            Some("relay-user"),
            "custom SMTP_USERNAME, trimmed"
        );
        assert_eq!(
            config.smtp_password.as_deref(),
            Some("relay-secret"),
            "custom SMTP_PASSWORD, kept verbatim"
        );
        assert_eq!(config.smtp_tls, SmtpTls::Tls, "custom SMTP_TLS");
        assert_eq!(
            config.mail_from, "hello@example.org",
            "custom MAIL_FROM, trimmed"
        );
    }

    #[test]
    fn empty_mailer_variables_read_as_unset() {
        let config = config_with(&[
            (SMTP_HOST_VAR, "  "),
            (SMTP_USERNAME_VAR, ""),
            (SMTP_PASSWORD_VAR, ""),
            (MAIL_FROM_VAR, " "),
        ]);
        assert_eq!(config.smtp_host, None, "blank SMTP_HOST leaves mail off");
        assert_eq!(config.smtp_username, None, "blank SMTP_USERNAME");
        assert_eq!(config.smtp_password, None, "blank SMTP_PASSWORD");
        assert_eq!(
            config.mail_from, "noreply@social.example",
            "blank MAIL_FROM falls back to the default"
        );
    }

    #[test]
    fn smtp_tls_parses_its_spellings() {
        for (value, want) in [
            ("starttls", SmtpTls::Starttls),
            (" STARTTLS ", SmtpTls::Starttls),
            ("tls", SmtpTls::Tls),
            ("TLS", SmtpTls::Tls),
            ("none", SmtpTls::None),
            ("None", SmtpTls::None),
        ] {
            let config = config_with(&[(SMTP_TLS_VAR, value)]);
            assert_eq!(config.smtp_tls, want, "SMTP_TLS={value:?}");
        }
        for value in ["", "  "] {
            let config = config_with(&[(SMTP_TLS_VAR, value)]);
            assert_eq!(
                config.smtp_tls,
                SmtpTls::Starttls,
                "SMTP_TLS={value:?} falls back to the default"
            );
        }
    }

    #[test]
    fn invalid_smtp_tls_is_an_error() {
        let err = try_config_with(&[(SMTP_TLS_VAR, "opportunistic")]).unwrap_err();
        assert_eq!(err, ConfigError::InvalidSmtpTls(SMTP_TLS_VAR), "variant");
        assert!(err.source().is_none(), "no source");
        assert_eq!(
            err.to_string(),
            "invalid SMTP_TLS: expected one of starttls, tls or none",
            "message names the variable and its vocabulary"
        );
    }

    #[test]
    fn smtp_port_must_be_a_port() {
        for value in ["many", "0.5", "65536"] {
            let err = try_config_with(&[(SMTP_PORT_VAR, value)]).unwrap_err();
            assert_eq!(
                err,
                ConfigError::InvalidNumber(SMTP_PORT_VAR),
                "SMTP_PORT={value:?} is refused"
            );
        }
    }

    #[test]
    fn mail_from_default_drops_the_base_url_port() {
        let config = config_with(&[(BASE_URL_VAR, "http://localhost:4000")]);
        assert_eq!(
            config.mail_from, "noreply@localhost",
            "a port is not part of an email domain"
        );
    }

    #[test]
    fn debug_redacts_the_smtp_password() {
        let config = config_with(&[
            (SMTP_HOST_VAR, "smtp.example.org"),
            (SMTP_USERNAME_VAR, "relay-user"),
            (SMTP_PASSWORD_VAR, "relay-secret"),
            (SMTP_TLS_VAR, "tls"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("relay-secret"),
            "the SMTP password must not leak: {debug}"
        );
        assert!(
            debug.contains(r#"smtp_password: "<redacted>""#),
            "the marker names the field: {debug}"
        );
        assert!(
            debug.contains(r#"smtp_host: Some("smtp.example.org")"#),
            "the relay host shows: {debug}"
        );
        assert!(
            debug.contains(r#"smtp_username: Some("relay-user")"#),
            "the login name shows: {debug}"
        );
        assert!(
            debug.contains("smtp_tls: Tls"),
            "the encryption mode shows: {debug}"
        );
    }

    #[test]
    fn reads_custom_max_chat_message_chars() {
        let config = config_with(&[(MAX_CHAT_MESSAGE_CHARS_VAR, "140")]);
        assert_eq!(
            config.max_chat_message_chars, 140,
            "custom MAX_CHAT_MESSAGE_CHARS"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_chat_message_chars: 140"),
            "the chat length is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn reads_custom_max_media_description() {
        let config = config_with(&[(MAX_MEDIA_DESCRIPTION_VAR, "100")]);
        assert_eq!(
            config.max_media_description, 100,
            "custom MAX_MEDIA_DESCRIPTION"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("max_media_description: 100"),
            "the description limit is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn the_chat_length_default_matches_the_storage_backstop() {
        // Same pairing as `models::oauth_token` keeps between the token-ttl
        // default and its model constant: the endpoint refuses above the
        // config number, the model above its own, and by default the two
        // numbers are one number.
        assert_eq!(
            DEFAULT_MAX_CHAT_MESSAGE_CHARS,
            crate::models::chat::MAX_CHAT_MESSAGE_CHARS,
            "the config default and the model's backstop agree"
        );
    }

    #[test]
    fn registrations_open_parses_booleans() {
        for (value, want) in [
            ("true", true),
            ("TRUE", true),
            (" 1 ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", true),
        ] {
            let config = config_with(&[(REGISTRATIONS_OPEN_VAR, value)]);
            assert_eq!(
                config.registrations_open.get(),
                want,
                "REGISTRATIONS_OPEN={value:?} means {want} (unset is open)"
            );
        }
    }

    #[test]
    fn invalid_registrations_open_is_an_error() {
        let err = try_config_with(&[(REGISTRATIONS_OPEN_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(REGISTRATIONS_OPEN_VAR),
            "error variant"
        );
        assert!(
            err.to_string()
                .starts_with(&format!("invalid {REGISTRATIONS_OPEN_VAR}: ")),
            "message names the variable"
        );
    }

    #[test]
    fn debug_shows_registrations_open() {
        let config = config_with(&[(REGISTRATIONS_OPEN_VAR, "false")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("registrations_open: Toggle(false)"),
            "the registration toggle is visible in Debug output: {debug}"
        );
    }

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
    fn emoji_dir_is_trimmed_and_defaults_to_emoji() {
        for (value, want) in [
            ("  /var/emoji  ", "/var/emoji"),
            ("emoji", "emoji"),
            ("", "./emoji"),
            ("   ", "./emoji"),
        ] {
            let config = config_with(&[(EMOJI_DIR_VAR, value)]);
            assert_eq!(
                config.emoji_dir,
                PathBuf::from(want),
                "EMOJI_DIR={value:?} means {want}"
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
        assert!(
            debug.contains("emoji_dir: \"./emoji\""),
            "the emoji packs directory is visible in Debug output: {debug}"
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
    fn misc_toggles_parse_booleans() {
        for var in [
            MONGOOSEIM_ENABLED_VAR,
            CAPTCHA_ENABLED_VAR,
            HEALTHCHECK_ENABLED_VAR,
        ] {
            for (value, want) in [
                ("true", true),
                ("TRUE", true),
                (" 1 ", true),
                ("false", false),
                ("FaLsE", false),
                ("0", false),
                ("", false),
            ] {
                let config = config_with(&[(var, value)]);
                let got = match var {
                    MONGOOSEIM_ENABLED_VAR => config.mongooseim_enabled,
                    CAPTCHA_ENABLED_VAR => config.captcha_enabled,
                    _ => config.healthcheck_enabled,
                };
                assert_eq!(got.get(), want, "{var}={value:?} means {want}");
            }
        }
    }

    #[test]
    fn invalid_misc_toggles_are_errors() {
        for var in [
            MONGOOSEIM_ENABLED_VAR,
            CAPTCHA_ENABLED_VAR,
            HEALTHCHECK_ENABLED_VAR,
        ] {
            let err = try_config_with(&[(var, "maybe")]).unwrap_err();
            assert_eq!(err, ConfigError::InvalidBoolean(var), "error for {var}");
            assert!(
                err.to_string().starts_with(&format!("invalid {var}: ")),
                "message names {var}"
            );
        }
    }

    #[test]
    fn debug_shows_the_misc_toggles() {
        let config = config_with(&[
            (MONGOOSEIM_ENABLED_VAR, "true"),
            (CAPTCHA_ENABLED_VAR, "false"),
            (HEALTHCHECK_ENABLED_VAR, "true"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("mongooseim_enabled: Toggle(true)"),
            "the `MongooseIM` toggle is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("captcha_enabled: Toggle(false)"),
            "the captcha toggle is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("healthcheck_enabled: Toggle(true)"),
            "the healthcheck toggle is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn empty_contact_email_is_none() {
        let config = config_with(&[(CONTACT_EMAIL_VAR, "")]);
        assert_eq!(config.contact_email, None, "empty contact_email");
    }

    #[test]
    fn available_frontends_default_to_empty() {
        let config = config_with(&[]);
        assert!(
            config.available_frontends.is_empty(),
            "no frontend is pickable until the operator names some"
        );
    }

    #[test]
    fn available_frontends_parses_a_comma_list() {
        let config = config_with(&[(AVAILABLE_FRONTENDS_VAR, " pleroma-fe, mochi-pub ,,x")]);
        assert_eq!(
            config.available_frontends,
            vec![
                "pleroma-fe".to_owned(),
                "mochi-pub".to_owned(),
                "x".to_owned()
            ],
            "the list is trimmed and blanks drop, order kept"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains(r#"available_frontends: ["pleroma-fe", "mochi-pub", "x"]"#),
            "the frontend list is visible in Debug output: {debug}"
        );
    }

    /// The ticket's one-provider `OAUTH_PROVIDERS` document, verbatim.
    const GITHUB_PROVIDERS: &str = r#"[{"name":"github","client_id":"cid","client_secret":"csecret","authorize_url":"https://github.com/login/oauth/authorize","token_url":"https://github.com/login/oauth/access_token","userinfo_url":"https://api.github.com/user","scopes":"read:user user:email","uid_field":"id","nickname_field":"login","email_field":"email","name_field":"name"}]"#;

    #[test]
    fn oauth_providers_default_to_empty() {
        for value in [None, Some(""), Some("   ")] {
            let extra = value.map_or_else(Vec::new, |text| vec![(OAUTH_PROVIDERS_VAR, text)]);
            let config = config_with(&extra);
            assert!(
                config.oauth_providers.is_empty(),
                "OAUTH_PROVIDERS={value:?} means no provider"
            );
            assert!(
                config.oauth_provider("github").is_none(),
                "and no name is configured"
            );
        }
    }

    #[test]
    fn oauth_providers_parses_the_documented_shape() {
        let config = config_with(&[(OAUTH_PROVIDERS_VAR, GITHUB_PROVIDERS)]);
        assert_eq!(config.oauth_providers.len(), 1, "one provider");
        let github = config.oauth_provider("github").expect("github");
        assert_eq!(github.name, "github", "name");
        assert_eq!(github.client_id, "cid", "client_id");
        assert_eq!(github.client_secret, "csecret", "client_secret");
        assert_eq!(
            github.authorize_url, "https://github.com/login/oauth/authorize",
            "authorize_url"
        );
        assert_eq!(
            github.token_url, "https://github.com/login/oauth/access_token",
            "token_url"
        );
        assert_eq!(
            github.userinfo_url, "https://api.github.com/user",
            "userinfo_url"
        );
        assert_eq!(github.scopes, "read:user user:email", "scopes");
        assert_eq!(github.uid_field, "id", "uid_field");
        assert_eq!(github.nickname_field, "login", "nickname_field");
        assert_eq!(github.email_field, "email", "email_field");
        assert_eq!(github.name_field, "name", "name_field");
        assert!(
            config.oauth_provider("GitLab").is_none(),
            "a name nobody configured is not found"
        );
    }

    #[test]
    fn oauth_providers_lists_them_all_and_applies_the_defaults() {
        let config = config_with(&[(
            OAUTH_PROVIDERS_VAR,
            r#"[{"name":"github","client_id":"a","client_secret":"b","authorize_url":"https://g.example/authorize","token_url":"https://g.example/token","userinfo_url":"https://g.example/user"},{"name":"keycloak","client_id":"c","client_secret":"d","authorize_url":"https://k.example/authorize","token_url":"https://k.example/token","userinfo_url":"https://k.example/userinfo","scopes":"openid","uid_field":"sub","nickname_field":"preferred_username","email_field":"email","name_field":"name"}]"#,
        )]);
        let names: Vec<&str> = config
            .oauth_providers
            .iter()
            .map(|provider| provider.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["github", "keycloak"],
            "the array keeps its order"
        );
        let github = config.oauth_provider("github").expect("github");
        assert_eq!(github.scopes, "", "scopes defaults to sending none");
        assert_eq!(github.uid_field, "id", "uid_field defaults to id");
        assert_eq!(
            github.nickname_field, "",
            "an unnamed field reads as absent"
        );
        assert_eq!(
            config
                .oauth_provider("keycloak")
                .map(|p| p.uid_field.as_str()),
            Some("sub"),
            "an explicit name wins"
        );
    }

    #[test]
    fn invalid_oauth_providers_is_a_startup_error() {
        for value in [
            "not json",
            r#"{"name":"github"}"#,
            "[]]",
            r#"[{"name":"github","client_id":"a","client_secret":"b","authorize_url":"https://g.example/a","token_url":"https://g.example/t","userinfo_url":"https://g.example/u","bogus":1}]"#,
            r#"[{"client_id":"a","client_secret":"b","authorize_url":"https://g.example/a","token_url":"https://g.example/t","userinfo_url":"https://g.example/u"}]"#,
        ] {
            let err = try_config_with(&[(OAUTH_PROVIDERS_VAR, value)]).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidOauthProviders(_)),
                "`{value}` is refused, got {err}"
            );
            assert!(
                err.to_string()
                    .starts_with(&format!("invalid {OAUTH_PROVIDERS_VAR}: ")),
                "the message names the variable"
            );
            assert!(err.source().is_none(), "no source");
        }
    }

    #[test]
    fn debug_redacts_the_provider_client_secret() {
        let config = config_with(&[(OAUTH_PROVIDERS_VAR, GITHUB_PROVIDERS)]);
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("csecret"),
            "the provider secret must not leak: {debug}"
        );
        assert!(
            debug.contains(r#"client_secret: "<redacted>""#),
            "the marker names the field: {debug}"
        );
        assert!(
            debug.contains("https://api.github.com/user"),
            "the rest of the provider shows: {debug}"
        );
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
            MAX_CHAT_MESSAGE_CHARS_VAR,
            UPLOAD_LIMIT_BYTES_VAR,
            UPLOAD_MAX_FILES_VAR,
            MIN_PASSWORD_LENGTH_VAR,
            OAUTH_TOKEN_TTL_SECS_VAR,
            JOB_CONCURRENCY_VAR,
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
