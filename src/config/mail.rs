//! Reads the six settings that configure outbound mail, plus the helpers
//! that derive the instance domain and the default `MAIL_FROM` address.

#[cfg(doc)]
use super::Config;

use super::{
    defaults::{DEFAULT_MAIL_FROM_USER, DEFAULT_SMTP_PORT},
    error::ConfigError,
    integrations::SmtpTls,
    parse::{number, trimmed},
    vars::{
        MAIL_FROM_VAR, SMTP_HOST_VAR, SMTP_PASSWORD_VAR, SMTP_PORT_VAR, SMTP_TLS_VAR,
        SMTP_USERNAME_VAR,
    },
};

/// What [`mailer_settings`] reads: the six settings that configure
/// outbound mail, named by what they are and hung on the [`Config`] fields
/// in [`Config::from_lookup`] (`SMTP_HOST`, `SMTP_PORT`, `SMTP_USERNAME`,
/// `SMTP_PASSWORD`, `SMTP_TLS`, `MAIL_FROM`).
#[derive(Debug)]
pub(crate) struct MailerSettings {
    /// The relay host; `None` means mail is disabled.
    pub(crate) host: Option<String>,
    /// The relay port.
    pub(crate) port: u16,
    /// The login name.
    pub(crate) username: Option<String>,
    /// The login secret.
    pub(crate) password: Option<String>,
    /// The encryption mode.
    pub(crate) tls: SmtpTls,
    /// The sender address.
    pub(crate) from: String,
}

/// Reads the six settings that configure outbound mail: the relay host
/// (unset means mail is disabled), the relay port, the login name and
/// secret, the encryption mode and the sender address. Grouped because each
/// is a plain default-or-parse and [`Config::from_lookup`] has a size budget
/// (AGENTS.md). `domain` is the [`domain_of`] the default sender address is
/// built from.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidNumber`] when `SMTP_PORT` is not a whole
/// number in the `u16` range, and [`ConfigError::InvalidSmtpTls`] when
/// `SMTP_TLS` names no known mode.
pub(crate) fn mailer_settings<F>(lookup: &F, domain: &str) -> Result<MailerSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let smtp_host = trimmed(lookup, SMTP_HOST_VAR);
    let smtp_port = smtp_port(lookup)?;
    let smtp_username = trimmed(lookup, SMTP_USERNAME_VAR);
    let smtp_password = lookup(SMTP_PASSWORD_VAR).filter(|secret| !secret.is_empty());
    let smtp_tls = smtp_tls(lookup)?;
    let mail_from = trimmed(lookup, MAIL_FROM_VAR).unwrap_or_else(|| default_mail_from(domain));
    Ok(MailerSettings {
        host: smtp_host,
        port: smtp_port,
        username: smtp_username,
        password: smtp_password,
        tls: smtp_tls,
        from: mail_from,
    })
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
pub(crate) fn domain_of(base_url: &str) -> &str {
    base_url
        .split_once("://")
        .map_or(base_url, |(_, domain)| domain)
}

/// The `MAIL_FROM` value used when the variable is unset:
/// `noreply@<instance domain>`. A port is not part of an email domain, so
/// `localhost:4000` contributes only `localhost`.
pub(crate) fn default_mail_from(domain: &str) -> String {
    let host = domain.split_once(':').map_or(domain, |(host, _)| host);
    format!("{DEFAULT_MAIL_FROM_USER}@{host}")
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    use crate::config::{
        test_support::{config_with, try_config_with},
        vars::BASE_URL_VAR,
    };

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
}
