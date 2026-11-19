//! The value types of the third-party integrations
//! [`Config`] configures.
//!
//! These are the external OAuth login providers, the media proxy, status
//! translation, the Web Push VAPID pair, and the SMTP encryption mode —
//! each with the manual `Debug` impl that keeps its secret material out of
//! the log.

use std::fmt;

#[cfg(doc)]
use super::Config;

#[cfg(doc)]
use super::defaults::MIN_MEDIA_PROXY_SECRET_CHARS;

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

/// Media proxy settings, from the seven `MEDIA_PROXY_*` variables.
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
    /// How many served URLs the media-proxy cache ledger keeps at most,
    /// from `MEDIA_PROXY_CACHE_LIMIT`; [`record`](crate::models::media_proxy_cache::record)
    /// prunes the oldest rows past it.
    pub cache_limit: usize,
    /// Origin `PURGE` cache-invalidation requests are sent to, from
    /// `MEDIA_PROXY_PURGE_URL` with any trailing slash trimmed; `None`
    /// (the default) means the admin purge endpoint sends no requests.
    pub purge_url: Option<String>,
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
            .field("cache_limit", &self.cache_limit)
            .field("purge_url", &self.purge_url)
            .finish()
    }
}

/// Which service translates statuses, the value of
/// `TRANSLATION_PROVIDER` (Pleroma's `[:language, :translation, :provider]`).
///
/// The two remote providers are the ones
/// [`crate::services::translate`] speaks; anything else is
/// [`None`](Self::None), which is also the default, so an instance
/// translates nothing until an operator points it at a server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TranslationProvider {
    /// No provider: `POST /api/v1/statuses/{id}/translate` answers 503 and
    /// the language matrix stays empty.
    #[default]
    None,
    /// A `LibreTranslate` server (self-hosted or a hosted instance of it).
    LibreTranslate,
    /// The `DeepL` API.
    DeepL,
}

impl TranslationProvider {
    /// The provider a `TRANSLATION_PROVIDER` value names, case-insensitively
    /// and trimmed; `None` for any other spelling.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "libretranslate" => Some(Self::LibreTranslate),
            "deepl" => Some(Self::DeepL),
            _ => None,
        }
    }

    /// Whether this name selects a provider at all
    /// ([`None`](Self::None) selects none).
    #[must_use]
    pub const fn is_none(self) -> bool {
        matches!(self, Self::None)
    }
}

/// VAPID key material for Web Push, from `VAPID_PRIVATE_KEY` and
/// `VAPID_PUBLIC_KEY`.
///
/// The instance's ES256 key pair normally lives in the `instance_secrets`
/// table, generated on first startup (see [`crate::services::vapid`]);
/// these settings override that pair with the operator's own material
/// instead. [`private_key`](Self::private_key) is secret: the manual
/// `Debug` impl below redacts it. [`public_key`](Self::public_key) is the
/// half every push client is handed as `server_key`, so it is no secret.
///
/// [`crate::services::vapid`]: crate::services::vapid
#[derive(Clone, Default, PartialEq, Eq)]
pub struct VapidConfig {
    /// The signing key as PKCS#8 PEM, from `VAPID_PRIVATE_KEY`; `None`
    /// keeps the generated pair in `instance_secrets`. Redacted in
    /// `Debug` output like `Config`'s `database_url`.
    pub private_key: Option<String>,
    /// The public point as URL-safe base64 (no padding), from
    /// `VAPID_PUBLIC_KEY`; `None` means the public half is read from (or
    /// derived from [`private_key`](Self::private_key)).
    pub public_key: Option<String>,
}

/// Redacts [`private_key`](Self::private_key), which is signing key
/// material; the public half shows.
impl fmt::Debug for VapidConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VapidConfig")
            .field(
                "private_key",
                &self.private_key.as_ref().map(|_| "<redacted>"),
            )
            .field("public_key", &self.public_key)
            .finish()
    }
}

/// Status-translation settings, from the three `TRANSLATION_*` variables.
///
/// [`crate::services::translate`] turns one of these into the translator the
/// endpoints use. [`api_key`](Self::api_key) is secret material: the manual
/// `Debug` impl below redacts it. The [`Default`] is the unset state: no
/// provider, no URL, no key.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct TranslationConfig {
    /// The provider to translate with, from `TRANSLATION_PROVIDER`;
    /// [`None`](TranslationProvider::None) by default.
    pub provider: TranslationProvider,
    /// The provider's API base URL, from `TRANSLATION_API_URL` with any
    /// trailing slash trimmed; `None` when unset.
    pub api_url: Option<String>,
    /// The provider's API key, from `TRANSLATION_API_KEY`; `None` when
    /// unset. Redacted in `Debug` output like `Config`'s `database_url`.
    pub api_key: Option<String>,
}

impl TranslationConfig {
    /// Whether these settings name a provider the instance can actually
    /// talk to: a provider other than [`None`](TranslationProvider::None),
    /// an [`api_url`](Self::api_url) to reach it at, and — for
    /// [`DeepL`](TranslationProvider::DeepL), whose API has no unauthenticated
    /// mode — an [`api_key`](Self::api_key). This is what
    /// `configuration.translation.enabled` of the v2 instance document
    /// reports (Pleroma's `Translation.configured?()`).
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        if self.provider.is_none() || self.api_url.is_none() {
            return false;
        }
        !matches!(self.provider, TranslationProvider::DeepL) || self.api_key.is_some()
    }
}

/// Redacts [`api_key`](Self::api_key), which is provider credentials.
impl fmt::Debug for TranslationConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TranslationConfig")
            .field("provider", &self.provider)
            .field("api_url", &self.api_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
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
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "starttls" => Some(Self::Starttls),
            "tls" => Some(Self::Tls),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{
        Config,
        test_support::config_with,
        vars::{
            TRANSLATION_API_KEY_VAR, TRANSLATION_API_URL_VAR, TRANSLATION_PROVIDER_VAR,
            VAPID_PRIVATE_KEY_VAR, VAPID_PUBLIC_KEY_VAR,
        },
    };

    #[test]
    fn vapid_overrides_default_to_unset() {
        let config = Config::for_tests();
        assert_eq!(config.vapid.private_key, None, "default private key");
        assert_eq!(config.vapid.public_key, None, "default public key");
    }

    #[test]
    fn vapid_overrides_are_trimmed_and_blanks_unset() {
        let set = config_with(&[
            (VAPID_PRIVATE_KEY_VAR, "  PEM-BODY-WITH-SPACES  "),
            (VAPID_PUBLIC_KEY_VAR, "  url-safe-public-key  "),
        ]);
        assert_eq!(
            set.vapid.private_key.as_deref(),
            Some("PEM-BODY-WITH-SPACES"),
            "the private half reads trimmed"
        );
        assert_eq!(
            set.vapid.public_key.as_deref(),
            Some("url-safe-public-key"),
            "the public half reads trimmed"
        );
        for (private, public) in [("   ", ""), ("", "  ")] {
            let blank = config_with(&[
                (VAPID_PRIVATE_KEY_VAR, private),
                (VAPID_PUBLIC_KEY_VAR, public),
            ]);
            assert_eq!(
                (
                    blank.vapid.private_key.as_deref(),
                    blank.vapid.public_key.as_deref()
                ),
                (None, None),
                "blank overrides read as unset, keeping the generated pair"
            );
        }
    }

    #[test]
    fn debug_redacts_the_vapid_private_key() {
        let config = config_with(&[
            (VAPID_PRIVATE_KEY_VAR, "a-signing-secret-never-logged"),
            (VAPID_PUBLIC_KEY_VAR, "the-advertised-public-half"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("a-signing-secret-never-logged"),
            "the private key must not leak: {debug}"
        );
        assert!(
            debug.contains("private_key: Some(\"<redacted>\")"),
            "the redaction marker stands in: {debug}"
        );
        assert!(
            debug.contains("public_key: Some(\"the-advertised-public-half\")"),
            "the public half shows: {debug}"
        );
    }

    #[test]
    fn debug_redacts_the_translation_api_key() {
        let config = config_with(&[
            (TRANSLATION_PROVIDER_VAR, "deepl"),
            (TRANSLATION_API_URL_VAR, "https://api.deepl.example"),
            (TRANSLATION_API_KEY_VAR, "a-provider-secret-never-logged"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("a-provider-secret-never-logged"),
            "the API key must not leak: {debug}"
        );
        assert!(
            debug.contains("api_key: Some(\"<redacted>\")"),
            "the redaction marker stands in: {debug}"
        );
        assert!(
            debug.contains("provider: DeepL"),
            "the provider itself is visible: {debug}"
        );
    }
}
