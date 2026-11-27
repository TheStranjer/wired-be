//! Reads what the instance exposes to clients (`client_exposure`), the
//! top-level `groups` that hand [`Config::from_lookup`] its reader
//! bundles, and the VAPID and translation settings.
//!
//! [`Config::from_lookup`]: crate::config::Config::from_lookup

use std::collections::BTreeSet;

#[cfg(doc)]
use super::Config;

use super::{
    auth::AuthSettings,
    defaults::{
        DEFAULT_INVITES_ENABLED, DEFAULT_REGISTRATIONS_OPEN,
        DEFAULT_SEARCH_STATUSES_UNAUTHENTICATED,
    },
    error::ConfigError,
    federation_types::Toggle,
    instance::{Endpoints, InstanceProfile, endpoints},
    integrations::{TranslationConfig, TranslationProvider, VapidConfig},
    jobs::{RestSettings, rest_settings},
    limits::ContentLimits,
    mail::{MailerSettings, domain_of},
    parse::{boolean, trimmed, trimmed_url},
    registration::{registration_modes, restrict_areas, signup_and_mail},
    vars::{
        INVITES_ENABLED_VAR, REGISTRATIONS_OPEN_VAR, SEARCH_STATUSES_UNAUTHENTICATED_VAR,
        TRANSLATION_API_KEY_VAR, TRANSLATION_API_URL_VAR, TRANSLATION_PROVIDER_VAR,
        VAPID_PRIVATE_KEY_VAR, VAPID_PUBLIC_KEY_VAR,
    },
    wording::{TextLengthLimits, profile_and_limits},
};

/// What [`client_exposure`] reads: the toggles that gate what the instance
/// exposes to clients and the areas it keeps from anonymous readers, named
/// by what they are and hung on the [`Config`]
/// fields in [`Config::from_lookup`] (`REGISTRATIONS_OPEN`,
/// `INVITES_ENABLED`, `SEARCH_STATUSES_UNAUTHENTICATED`,
/// `RESTRICT_UNAUTHENTICATED`).
#[derive(Debug)]
pub(crate) struct ExposureSettings {
    /// Whether new registrations are open (`REGISTRATIONS_OPEN`).
    pub(crate) registrations_open: Toggle,
    /// Whether invite tokens substitute for open registrations
    /// (`INVITES_ENABLED`).
    pub(crate) invites_enabled: Toggle,
    /// Whether a new account waits for staff approval
    /// (`REGISTRATIONS_APPROVAL_REQUIRED`).
    pub(crate) registrations_approval_required: Toggle,
    /// Whether a new account must confirm its email address before it may
    /// log in (`ACCOUNT_ACTIVATION_REQUIRED`).
    pub(crate) account_activation_required: Toggle,
    /// Whether anonymous requests may search statuses at the combined
    /// search (`SEARCH_STATUSES_UNAUTHENTICATED`).
    pub(crate) search_statuses_unauthenticated: Toggle,
    /// The areas an anonymous request may not read
    /// (`RESTRICT_UNAUTHENTICATED`).
    pub(crate) restrict_unauthenticated: BTreeSet<String>,
}

/// Reads the settings that gate what the instance exposes to clients:
/// whether new registrations are open (`NodeInfo`'s `openRegistrations`),
/// whether invite tokens substitute for open registrations
/// (`invites_enabled` on the instance endpoints), what a finished
/// registration means — waiting for staff approval and confirming the
/// email ([`registration_modes`]) — whether anonymous requests may search
/// statuses at the combined search, which areas anonymous reads are
/// refused.
/// Grouped because each is a plain default-or-parse and
/// [`Config::from_lookup`] has a size budget (AGENTS.md).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] when `REGISTRATIONS_OPEN`,
/// `INVITES_ENABLED`, `REGISTRATIONS_APPROVAL_REQUIRED`,
/// `ACCOUNT_ACTIVATION_REQUIRED` or `SEARCH_STATUSES_UNAUTHENTICATED`
/// holds a value that is neither spelling, and whatever [`restrict_areas`]
/// reports.
pub(crate) fn client_exposure<F>(lookup: &F) -> Result<ExposureSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let registrations_open =
        boolean(lookup, REGISTRATIONS_OPEN_VAR, DEFAULT_REGISTRATIONS_OPEN)?.into();
    let invites_enabled = boolean(lookup, INVITES_ENABLED_VAR, DEFAULT_INVITES_ENABLED)?.into();
    let (registrations_approval_required, account_activation_required) =
        registration_modes(lookup)?;
    let search_statuses_unauthenticated = boolean(
        lookup,
        SEARCH_STATUSES_UNAUTHENTICATED_VAR,
        DEFAULT_SEARCH_STATUSES_UNAUTHENTICATED,
    )?
    .into();
    let restrict_unauthenticated = restrict_areas(lookup)?;
    Ok(ExposureSettings {
        registrations_open,
        invites_enabled,
        registrations_approval_required,
        account_activation_required,
        search_statuses_unauthenticated,
        restrict_unauthenticated,
    })
}

/// What [`groups`] answers: [`Endpoints`], [`Wording`], [`Signup`] and
/// [`RestSettings`] flattened into one eight-value tuple, so
/// [`Config::from_lookup`] binds all eight groups with a single `let` and
/// stays inside Clippy's line budget.
pub(crate) type Groups = (
    Endpoints,
    InstanceProfile,
    ContentLimits,
    TextLengthLimits,
    AuthSettings,
    ExposureSettings,
    MailerSettings,
    RestSettings,
);

/// [`profile_and_limits`] and [`signup_and_mail`] in one call: the
/// instance-wording trio followed by the signup, exposure and mailer
/// trio, flattened, after the [`Endpoints`] they hang off. The origin's domain
/// is what the mailer's `From:` default is derived from (the pair the two grouped
/// readers used to take apart).
pub(crate) fn groups<F>(lookup: &F) -> Result<Groups, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let ends = endpoints(lookup)?;
    let (profile, limits, text) = profile_and_limits(lookup)?;
    let (auth, exposure, mail) = signup_and_mail(lookup, domain_of(&ends.base_url))?;
    Ok((
        ends,
        profile,
        limits,
        text,
        auth,
        exposure,
        mail,
        rest_settings(lookup)?,
    ))
}

/// Reads the two VAPID overrides, each trimmed, a blank standing for
/// unset (the generated pair then stays in charge). A public key without a
/// private one is refused: the instance would advertise a key it cannot
/// sign for.
///
/// # Errors
///
/// Returns [`ConfigError::VapidPublicWithoutPrivate`] when
/// `VAPID_PUBLIC_KEY` is set and `VAPID_PRIVATE_KEY` is not.
pub(crate) fn vapid_settings<F>(lookup: &F) -> Result<VapidConfig, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let private_key = trimmed(lookup, VAPID_PRIVATE_KEY_VAR);
    let public_key = trimmed(lookup, VAPID_PUBLIC_KEY_VAR);
    if public_key.is_some() && private_key.is_none() {
        return Err(ConfigError::VapidPublicWithoutPrivate(VAPID_PUBLIC_KEY_VAR));
    }
    Ok(VapidConfig {
        private_key,
        public_key,
    })
}

/// Reads the three settings that configure status translation
/// (see [`crate::services::translate`]). Grouped because each is a plain
/// default-or-parse and [`Config::from_lookup`] has a size budget (AGENTS.md).
/// An unset or blank provider is [`TranslationProvider::None`], the default;
/// a name that is not one of the three is a startup error, because it would
/// silently leave the instance translating nothing.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidTranslationProvider`] when
/// `TRANSLATION_PROVIDER` names no provider this instance speaks.
pub(crate) fn translation_settings<F>(lookup: &F) -> Result<TranslationConfig, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let raw = lookup(TRANSLATION_PROVIDER_VAR).unwrap_or_default();
    let text = if raw.trim().is_empty() {
        "none".to_owned()
    } else {
        raw
    };
    let provider = TranslationProvider::parse(&text).ok_or(
        ConfigError::InvalidTranslationProvider(TRANSLATION_PROVIDER_VAR),
    )?;
    let api_url = trimmed_url(lookup, TRANSLATION_API_URL_VAR);
    let api_key = trimmed(lookup, TRANSLATION_API_KEY_VAR);
    Ok(TranslationConfig {
        provider,
        api_url,
        api_key,
    })
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    use crate::config::test_support::{config_with, try_config_with};

    /// The unauthenticated status-search toggle: off by default, the
    /// environment turning it on, blank falling back, and junk refused.
    #[test]
    fn search_statuses_unauthenticated_defaults_off_and_reads_its_variable() {
        assert!(
            !config_with(&[]).search_statuses_unauthenticated.get(),
            "anonymous requests search no statuses by default"
        );
        for value in ["true", "1", " TRUE "] {
            let config = config_with(&[(SEARCH_STATUSES_UNAUTHENTICATED_VAR, value)]);
            assert!(
                config.search_statuses_unauthenticated.get(),
                "`SEARCH_STATUSES_UNAUTHENTICATED={value:?}` turns it on"
            );
        }
        for value in ["false", "0"] {
            let config = config_with(&[(SEARCH_STATUSES_UNAUTHENTICATED_VAR, value)]);
            assert!(
                !config.search_statuses_unauthenticated.get(),
                "`SEARCH_STATUSES_UNAUTHENTICATED={value:?}` keeps it off"
            );
        }
        assert!(
            !config_with(&[(SEARCH_STATUSES_UNAUTHENTICATED_VAR, "")])
                .search_statuses_unauthenticated
                .get(),
            "an empty value applies the default"
        );
        let err = try_config_with(&[(SEARCH_STATUSES_UNAUTHENTICATED_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(SEARCH_STATUSES_UNAUTHENTICATED_VAR),
            "a value that is neither spelling is refused"
        );
        let debug = format!("{:?}", config_with(&[]));
        assert!(
            debug.contains("search_statuses_unauthenticated: Toggle(false)"),
            "the toggle is visible in Debug output: {debug}"
        );
    }

    #[test]
    fn translation_is_off_by_default() {
        let config = config_with(&[]);
        assert_eq!(
            config.translation.provider,
            TranslationProvider::None,
            "no provider until the operator names one"
        );
        assert_eq!(config.translation.api_url, None, "no API URL by default");
        assert_eq!(config.translation.api_key, None, "no API key by default");
        assert!(
            !config.translation.is_configured(),
            "an unconfigured provider is reported as such"
        );
    }

    #[test]
    fn reads_custom_translation_settings() {
        let config = config_with(&[
            (TRANSLATION_PROVIDER_VAR, " LibreTranslate "),
            (TRANSLATION_API_URL_VAR, " https://translate.example/ "),
            (TRANSLATION_API_KEY_VAR, "  lt-key  "),
        ]);
        assert_eq!(
            config.translation.provider,
            TranslationProvider::LibreTranslate,
            "the provider name is matched case-insensitively and trimmed"
        );
        assert_eq!(
            config.translation.api_url.as_deref(),
            Some("https://translate.example"),
            "TRANSLATION_API_URL is trimmed and loses its trailing slash"
        );
        assert_eq!(
            config.translation.api_key.as_deref(),
            Some("lt-key"),
            "TRANSLATION_API_KEY is trimmed"
        );
        assert!(
            config.translation.is_configured(),
            "a named provider with a URL is usable"
        );
    }

    #[test]
    fn translation_provider_accepts_every_spelling() {
        for (value, provider) in [
            ("none", TranslationProvider::None),
            ("NONE", TranslationProvider::None),
            ("", TranslationProvider::None),
            ("   ", TranslationProvider::None),
            ("libretranslate", TranslationProvider::LibreTranslate),
            ("LibreTranslate", TranslationProvider::LibreTranslate),
            ("deepl", TranslationProvider::DeepL),
            ("DeepL", TranslationProvider::DeepL),
        ] {
            let config = config_with(&[(TRANSLATION_PROVIDER_VAR, value)]);
            assert_eq!(
                config.translation.provider, provider,
                "`{value}` selects its provider"
            );
        }
    }

    #[test]
    fn a_public_vapid_key_without_a_private_one_is_an_error() {
        let err = try_config_with(&[(VAPID_PUBLIC_KEY_VAR, "some-public-key")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::VapidPublicWithoutPrivate(VAPID_PUBLIC_KEY_VAR),
            "error variant"
        );
        assert!(err.source().is_none(), "no source");
        assert!(
            err.to_string().starts_with("invalid VAPID_PUBLIC_KEY: "),
            "message names the variable: {err}"
        );
    }

    #[test]
    fn vapid_overrides_accept_a_private_key_alone_or_a_pair_and_blank_counts_as_unset() {
        let private_only = config_with(&[(VAPID_PRIVATE_KEY_VAR, " pem ")]);
        assert_eq!(private_only.vapid.private_key.as_deref(), Some("pem"));
        assert_eq!(private_only.vapid.public_key, None);

        let pair = config_with(&[
            (VAPID_PRIVATE_KEY_VAR, "pem"),
            (VAPID_PUBLIC_KEY_VAR, "pub"),
        ]);
        assert_eq!(pair.vapid.public_key.as_deref(), Some("pub"));

        let blank_public = config_with(&[(VAPID_PUBLIC_KEY_VAR, "   ")]);
        assert_eq!(blank_public.vapid, VapidConfig::default(), "blank is unset");
    }

    #[test]
    fn invalid_translation_provider_is_an_error() {
        let err = try_config_with(&[(TRANSLATION_PROVIDER_VAR, "google")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidTranslationProvider(TRANSLATION_PROVIDER_VAR),
            "error variant"
        );
        assert!(err.source().is_none(), "no source");
        assert_eq!(
            err.to_string(),
            "invalid TRANSLATION_PROVIDER: expected one of none, libretranslate or deepl",
            "message names the variable and its three values"
        );
    }

    #[test]
    fn a_provider_without_a_url_or_key_is_not_configured() {
        let urlless = config_with(&[(TRANSLATION_PROVIDER_VAR, "libretranslate")]);
        assert!(
            !urlless.translation.is_configured(),
            "a provider with no server to talk to is unusable"
        );
        let keyless = config_with(&[
            (TRANSLATION_PROVIDER_VAR, "deepl"),
            (TRANSLATION_API_URL_VAR, "https://api.deepl.example"),
        ]);
        assert!(
            !keyless.translation.is_configured(),
            "DeepL has no unauthenticated mode, so its key is required"
        );
        let with_key = config_with(&[
            (TRANSLATION_PROVIDER_VAR, "deepl"),
            (TRANSLATION_API_URL_VAR, "https://api.deepl.example"),
            (TRANSLATION_API_KEY_VAR, "dl-key"),
        ]);
        assert!(
            with_key.translation.is_configured(),
            "DeepL with URL and key is usable"
        );
        let hosted = config_with(&[
            (TRANSLATION_PROVIDER_VAR, "libretranslate"),
            (TRANSLATION_API_URL_VAR, "https://translate.example"),
        ]);
        assert!(
            hosted.translation.is_configured(),
            "LibreTranslate needs no key: a self-hosted server asks for none"
        );
    }

    #[test]
    fn empty_translation_url_and_key_are_unset() {
        let config = config_with(&[
            (TRANSLATION_PROVIDER_VAR, "deepl"),
            (TRANSLATION_API_URL_VAR, "   "),
            (TRANSLATION_API_KEY_VAR, ""),
        ]);
        assert_eq!(
            config.translation.api_url, None,
            "a blank URL reads as unset"
        );
        assert_eq!(
            config.translation.api_key, None,
            "a blank key reads as unset"
        );
        assert!(
            !config.translation.is_configured(),
            "so the provider stays unconfigured"
        );
    }

    #[test]
    fn the_retired_available_frontends_variable_is_not_read() {
        let config = config_with(&[("AVAILABLE_FRONTENDS", "x")]);
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("available_frontends"),
            "the retired frontend list is not in Debug output: {debug}"
        );
    }
}
