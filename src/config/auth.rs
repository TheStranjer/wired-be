//! Reads the account settings: the HMAC signing secret, the registration
//! gates that are not modes, the token lifetimes, and the external OAuth
//! login providers.

#[cfg(doc)]
use super::Config;

use super::{
    defaults::{
        DEFAULT_ACCOUNTS_EMAIL_OPTIONAL, DEFAULT_ALLOW_ACCOUNT_DELETION,
        DEFAULT_MIN_PASSWORD_LENGTH, DEFAULT_OAUTH_PROVIDERS, DEFAULT_OAUTH_TOKEN_TTL_SECS,
        DEFAULT_PASSWORD_RESET_TTL_SECS,
    },
    error::ConfigError,
    federation_types::Toggle,
    integrations::OauthProvider,
    parse::{boolean, number},
    vars::{
        ACCOUNTS_EMAIL_OPTIONAL_VAR, ALLOW_ACCOUNT_DELETION_VAR, MIN_PASSWORD_LENGTH_VAR,
        OAUTH_PROVIDERS_VAR, OAUTH_TOKEN_TTL_SECS_VAR, PASSWORD_RESET_TTL_SECS_VAR,
        SECRET_KEY_BASE_VAR,
    },
};

/// Reads `SECRET_KEY_BASE`: the trimmed signing secret, `None` when the
/// variable is unset or blank, in which case [`serve`](crate::serve)
/// fills the field with a secret generated for the boot. Read through its
/// own fn because [`Config::from_lookup`] has a size budget (AGENTS.md).
pub(crate) fn secret_key_base<F>(lookup: &F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(SECRET_KEY_BASE_VAR)
        .map(|secret| secret.trim().to_owned())
        .filter(|secret| !secret.is_empty())
}

/// What [`auth_settings`] reads: the six settings that gate
/// registration, token issuance, password resets, the external logins and
/// account deletion, in [`Config`] field order.
#[derive(Debug)]
pub(crate) struct AuthSettings {
    /// Whether registration may omit the email address
    /// (`ACCOUNTS_EMAIL_OPTIONAL`).
    pub(crate) accounts_email_optional: bool,
    /// The registration password minimum (`MIN_PASSWORD_LENGTH`).
    pub(crate) min_password_length: usize,
    /// The access-token lifetime (`OAUTH_TOKEN_TTL_SECS`).
    pub(crate) oauth_token_ttl_secs: usize,
    /// The password-reset token lifetime (`PASSWORD_RESET_TTL_SECS`).
    pub(crate) password_reset_ttl_secs: usize,
    /// Whether accounts may delete their own account through
    /// `POST /api/pleroma/delete_account` (`ALLOW_ACCOUNT_DELETION`).
    pub(crate) allow_account_deletion: Toggle,
    /// The external login providers (`OAUTH_PROVIDERS`).
    pub(crate) oauth_providers: Vec<OauthProvider>,
}

/// Reads the six settings that gate registration, token issuance,
/// password resets, the external logins and account deletion. Email
/// confirmation lives with [`registration_modes`] instead. Grouped
/// because each is a plain default-or-parse and [`Config::from_lookup`]
/// has a size budget (AGENTS.md).
pub(crate) fn auth_settings<F>(lookup: &F) -> Result<AuthSettings, ConfigError>
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
    let password_reset_ttl_secs = number(
        lookup,
        PASSWORD_RESET_TTL_SECS_VAR,
        DEFAULT_PASSWORD_RESET_TTL_SECS,
    )?;
    let allow_account_deletion = boolean(
        lookup,
        ALLOW_ACCOUNT_DELETION_VAR,
        DEFAULT_ALLOW_ACCOUNT_DELETION,
    )?
    .into();
    Ok(AuthSettings {
        accounts_email_optional,
        min_password_length,
        oauth_token_ttl_secs,
        password_reset_ttl_secs,
        allow_account_deletion,
        oauth_providers: oauth_providers(lookup)?,
    })
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

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    use crate::config::test_support::{config_with, try_config_with};

    #[test]
    fn secret_key_base_defaults_to_unset() {
        let config = config_with(&[]);
        assert_eq!(
            config.secret_key_base, None,
            "no signing secret unless configured"
        );
    }

    #[test]
    fn reads_custom_secret_key_base() {
        let config = config_with(&[(SECRET_KEY_BASE_VAR, "  a-signing-secret  ")]);
        assert_eq!(
            config.secret_key_base.as_deref(),
            Some("a-signing-secret"),
            "the secret is trimmed"
        );
    }

    #[test]
    fn blank_secret_key_base_reads_as_unset() {
        let config = config_with(&[(SECRET_KEY_BASE_VAR, "   ")]);
        assert_eq!(config.secret_key_base, None, "a blank secret is no secret");
    }

    #[test]
    fn debug_redacts_the_secret_key_base() {
        let config = config_with(&[(SECRET_KEY_BASE_VAR, "a-signing-secret")]);
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("a-signing-secret"),
            "the signing secret must not leak: {debug}"
        );
        assert!(
            debug.contains(r#"secret_key_base: Some("<redacted>")"#),
            "the marker names the field: {debug}"
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
    fn reads_password_reset_token_ttl() {
        let default = config_with(&[]);
        assert_eq!(
            default.password_reset_ttl_secs, 86_400,
            "default PASSWORD_RESET_TTL_SECS is 24 hours"
        );
        let custom = config_with(&[(PASSWORD_RESET_TTL_SECS_VAR, "7200")]);
        assert_eq!(
            custom.password_reset_ttl_secs, 7_200,
            "custom PASSWORD_RESET_TTL_SECS is honored"
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
    fn debug_shows_the_token_ttl_setting() {
        let config = config_with(&[
            (OAUTH_TOKEN_TTL_SECS_VAR, "600"),
            (PASSWORD_RESET_TTL_SECS_VAR, "1200"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("oauth_token_ttl_secs: 600"),
            "the token lifetime is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("password_reset_ttl_secs: 1200"),
            "the reset-token lifetime is visible in Debug output: {debug}"
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
}
