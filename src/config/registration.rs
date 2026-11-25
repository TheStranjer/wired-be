//! Reads what a finished registration means and who may get an account.
//!
//! These are the approval and email-confirmation modes, the areas anonymous
//! readers are refused, and the `Signup` bundle the exposure module's
//! `groups` grows for the mailer.

use std::collections::BTreeSet;

#[cfg(doc)]
use super::Config;

use super::{
    auth::{AuthSettings, auth_settings},
    defaults::{
        DEFAULT_ACCOUNT_ACTIVATION_REQUIRED, DEFAULT_REGISTRATIONS_APPROVAL_REQUIRED,
        DEFAULT_RESTRICT_UNAUTHENTICATED,
    },
    error::ConfigError,
    exposure::{ExposureSettings, client_exposure},
    federation_types::Toggle,
    mail::{MailerSettings, mailer_settings},
    parse::{boolean, comma_list},
    vars::{
        ACCOUNT_ACTIVATION_REQUIRED_VAR, REGISTRATIONS_APPROVAL_REQUIRED_VAR,
        RESTRICT_UNAUTHENTICATED_AREAS, RESTRICT_UNAUTHENTICATED_VAR,
    },
};

/// The shape [`signup_and_mail`] answers: the account settings, what the
/// client surface exposes and the mailer settings the confirmation mails
/// go out over, in the order the three groups feed the [`Config`] fields;
/// factored out because a three-value tuple of structs is a very complex
/// type to spell inline.
pub(crate) type Signup = (AuthSettings, ExposureSettings, MailerSettings);

/// Reads the three groups behind who may get an account and how they are
/// reached: the [`auth_settings`], the [`client_exposure`] the registration
/// modes grew, and the [`mailer_settings`] the approval and confirmation
/// mails go out over, built for `domain` — the instance's own, read off
/// the normalized base URL. Grouped because [`Config::from_lookup`] has a
/// size budget (AGENTS.md); see [`Signup`].
///
/// # Errors
///
/// Those of [`auth_settings`], [`client_exposure`] and [`mailer_settings`].
pub(crate) fn signup_and_mail<F>(lookup: &F, domain: &str) -> Result<Signup, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok((
        auth_settings(lookup)?,
        client_exposure(lookup)?,
        mailer_settings(lookup, domain)?,
    ))
}

/// The areas [`RESTRICT_UNAUTHENTICATED_VAR`] names, as a set of the
/// canonical lowercase spellings in
/// [`RESTRICT_UNAUTHENTICATED_AREAS`]: a comma-separated list, each entry
/// trimmed and lowercased, so repeats fold and the order the operator wrote
/// is irrelevant; unset or empty restricts nothing.
///
/// # Errors
///
/// [`ConfigError::InvalidRestrictUnauthenticated`] for a name outside the
/// six areas — an operator's typo must stop the process, not silently leave
/// the profiles it meant to close open.
pub(crate) fn restrict_areas<F>(lookup: &F) -> Result<BTreeSet<String>, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let configured = lookup(RESTRICT_UNAUTHENTICATED_VAR);
    let text = configured
        .as_deref()
        .unwrap_or(DEFAULT_RESTRICT_UNAUTHENTICATED);
    comma_list(text)
        .into_iter()
        .map(|area| {
            let area = area.to_lowercase();
            if RESTRICT_UNAUTHENTICATED_AREAS.contains(&area.as_str()) {
                Ok(area)
            } else {
                Err(ConfigError::InvalidRestrictUnauthenticated(area))
            }
        })
        .collect()
}

/// The two registration-mode toggles [`registration_modes`] reads, in
/// [`ExposureSettings`] field order; factored out because a pair of
/// same-typed toggles reads better by name than as a tuple.
pub(crate) type RegistrationModes = (Toggle, Toggle);

/// Reads the two settings that decide what a finished registration means:
/// whether the new account waits for staff approval (`approval_required` on
/// the instance endpoints) and whether it has to confirm its email address
/// (`account_activation_required` and `accountActivationRequired`). Grouped
/// because each is a plain default-or-parse and [`Config::from_lookup`] has
/// a size budget (AGENTS.md); the tuple comes back in field order. Both are
/// enforced by [`create_local_user`](crate::models::user::register::create_local_user)
/// and reported by the account-status gate of [`crate::api::oauth`].
///
/// # Errors
///
/// Returns [`ConfigError::InvalidBoolean`] when either variable holds a
/// value that is neither spelling.
pub(crate) fn registration_modes<F>(lookup: &F) -> Result<RegistrationModes, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let approvals = boolean(
        lookup,
        REGISTRATIONS_APPROVAL_REQUIRED_VAR,
        DEFAULT_REGISTRATIONS_APPROVAL_REQUIRED,
    )?;
    let activation = boolean(
        lookup,
        ACCOUNT_ACTIVATION_REQUIRED_VAR,
        DEFAULT_ACCOUNT_ACTIVATION_REQUIRED,
    )?;
    Ok((approvals.into(), activation.into()))
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    use crate::config::{
        test_support::{config_with, try_config_with},
        vars::{ALLOW_ACCOUNT_DELETION_VAR, INVITES_ENABLED_VAR, REGISTRATIONS_OPEN_VAR},
    };

    #[test]
    fn registrations_open_parses_booleans() {
        for (value, want) in [
            ("true", true),
            ("TRUE", true),
            (" 1 ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", false),
        ] {
            let config = config_with(&[(REGISTRATIONS_OPEN_VAR, value)]);
            assert_eq!(
                config.registrations_open.get(),
                want,
                "REGISTRATIONS_OPEN={value:?} means {want} (blank is closed)"
            );
        }
    }

    /// An unset `REGISTRATIONS_OPEN` is closed: opening registration is the
    /// operator's explicit decision.
    #[test]
    fn registrations_are_closed_when_unset() {
        let config = config_with(&[]);
        assert!(
            !config.registrations_open.get(),
            "unset REGISTRATIONS_OPEN means closed"
        );
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

    /// `INVITES_ENABLED` keeps its false default when the variable is
    /// unset: closed registrations refuse every attempt until the
    /// operator turns invites on.
    #[test]
    fn invites_are_off_by_default() {
        let config = config_with(&[]);
        assert!(
            !config.invites_enabled.get(),
            "invites are off by default: closed registrations refuse every attempt"
        );
    }

    #[test]
    fn invites_enabled_parses_booleans() {
        for (value, want) in [
            ("true", true),
            ("TRUE", true),
            (" 1 ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", false),
        ] {
            let config = config_with(&[(INVITES_ENABLED_VAR, value)]);
            assert_eq!(
                config.invites_enabled.get(),
                want,
                "INVITES_ENABLED={value:?} means {want} (unset is off)"
            );
        }
    }

    #[test]
    fn invalid_invites_enabled_is_an_error() {
        let err = try_config_with(&[(INVITES_ENABLED_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(INVITES_ENABLED_VAR),
            "error variant"
        );
        assert!(
            err.to_string()
                .starts_with(&format!("invalid {INVITES_ENABLED_VAR}: ")),
            "message names the variable"
        );
    }

    #[test]
    fn debug_shows_invites_enabled() {
        let config = config_with(&[(INVITES_ENABLED_VAR, "true")]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("invites_enabled: Toggle(true)"),
            "the invite toggle is visible in Debug output: {debug}"
        );
    }

    /// Both registration modes are off until the operator says otherwise:
    /// a plain instance registers and logs in without approval or an email
    /// confirmation.
    #[test]
    fn registration_modes_are_off_by_default() {
        let config = config_with(&[]);
        assert!(
            !config.registrations_approval_required.get(),
            "approval is not required by default"
        );
        assert!(
            !config.account_activation_required.get(),
            "email confirmation is not required by default"
        );
    }

    #[test]
    fn registration_mode_toggles_parse_booleans() {
        for var in [
            REGISTRATIONS_APPROVAL_REQUIRED_VAR,
            ACCOUNT_ACTIVATION_REQUIRED_VAR,
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
                    REGISTRATIONS_APPROVAL_REQUIRED_VAR => config.registrations_approval_required,
                    _ => config.account_activation_required,
                };
                assert_eq!(got.get(), want, "{var}={value:?} means {want}");
            }
        }
    }

    #[test]
    fn invalid_registration_modes_are_errors() {
        for var in [
            REGISTRATIONS_APPROVAL_REQUIRED_VAR,
            ACCOUNT_ACTIVATION_REQUIRED_VAR,
        ] {
            let err = try_config_with(&[(var, "maybe")]).unwrap_err();
            assert_eq!(
                err,
                ConfigError::InvalidBoolean(var),
                "error variant for {var}"
            );
            assert!(
                err.to_string().starts_with(&format!("invalid {var}: ")),
                "message names the variable"
            );
        }
    }

    #[test]
    fn debug_shows_the_registration_modes() {
        let config = config_with(&[
            (REGISTRATIONS_APPROVAL_REQUIRED_VAR, "true"),
            (ACCOUNT_ACTIVATION_REQUIRED_VAR, "true"),
        ]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("registrations_approval_required: Toggle(true)"),
            "the approval toggle is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("account_activation_required: Toggle(true)"),
            "the activation toggle is visible in Debug output: {debug}"
        );
    }

    /// Email confirmation: off by default (this server activates accounts
    /// at registration), every boolean spelling read, junk refused.
    #[test]
    fn account_activation_required_defaults_off_and_parses_booleans() {
        assert!(
            !config_with(&[]).account_activation_required.get(),
            "no account must confirm its email by default"
        );
        for (value, want) in [
            ("true", true),
            ("TRUE", true),
            (" 1 ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", false),
        ] {
            let config = config_with(&[(ACCOUNT_ACTIVATION_REQUIRED_VAR, value)]);
            assert_eq!(
                config.account_activation_required.get(),
                want,
                "ACCOUNT_ACTIVATION_REQUIRED={value:?} means {want}"
            );
        }
        let err = try_config_with(&[(ACCOUNT_ACTIVATION_REQUIRED_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(ACCOUNT_ACTIVATION_REQUIRED_VAR),
            "error variant"
        );
    }

    /// Account deletion: offered by default (Pleroma offers the endpoint),
    /// every boolean spelling read, junk refused.
    #[test]
    fn allow_account_deletion_defaults_on_and_parses_booleans() {
        assert!(
            config_with(&[]).allow_account_deletion.get(),
            "accounts may delete themselves by default"
        );
        for (value, want) in [
            ("true", true),
            ("1", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("", true),
        ] {
            let config = config_with(&[(ALLOW_ACCOUNT_DELETION_VAR, value)]);
            assert_eq!(
                config.allow_account_deletion.get(),
                want,
                "ALLOW_ACCOUNT_DELETION={value:?} means {want} (blank falls back)"
            );
        }
        let err = try_config_with(&[(ALLOW_ACCOUNT_DELETION_VAR, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            ConfigError::InvalidBoolean(ALLOW_ACCOUNT_DELETION_VAR),
            "error variant"
        );
    }

    /// The restricted areas: nothing by default, a comma list read trimmed
    /// and lowercased with repeats folded, an empty value restricting
    /// nothing, and an unknown area refusing startup rather than leaving a
    /// profile silently open.
    #[test]
    fn restrict_unauthenticated_defaults_open_and_reads_its_areas() {
        let open = config_with(&[]);
        assert!(
            open.restrict_unauthenticated.is_empty(),
            "a default instance restricts nothing"
        );
        for area in RESTRICT_UNAUTHENTICATED_AREAS {
            assert!(!open.restricts(area), "`{area}` is open by default");
        }
        let config = config_with(&[(
            RESTRICT_UNAUTHENTICATED_VAR,
            " PROFILES_LOCAL , profiles_remote, timelines_local, profiles_local ",
        )]);
        assert!(config.restricts("profiles_local"), "the half is closed");
        assert!(config.restricts("profiles_remote"), "and so is its twin");
        assert!(
            config.restricts("timelines_local"),
            "as is a timelines half"
        );
        assert!(
            !config.restricts("activities_local"),
            "an area the variable never named stays open"
        );
        assert!(
            !config.restricts("no-such-area"),
            "an unknown name restricts nothing"
        );
        assert_eq!(
            config.restrict_unauthenticated.len(),
            3,
            "the repeat folds and the spacing is trimmed: {:?}",
            config.restrict_unauthenticated
        );
        assert!(
            !config_with(&[(RESTRICT_UNAUTHENTICATED_VAR, "")]).restricts("profiles_local"),
            "an empty value restricts nothing"
        );
    }

    /// An area outside [`RESTRICT_UNAUTHENTICATED_AREAS`] is a startup
    /// failure naming the word it could not place.
    #[test]
    fn an_unknown_restricted_area_is_an_error() {
        let err = try_config_with(&[(RESTRICT_UNAUTHENTICATED_VAR, "profiles_everywhere")])
            .expect_err("an unknown area is refused");
        assert_eq!(
            err,
            ConfigError::InvalidRestrictUnauthenticated("profiles_everywhere".to_owned()),
            "the error carries the offending name"
        );
        assert_eq!(
            err.to_string(),
            "invalid RESTRICT_UNAUTHENTICATED: `profiles_everywhere` is not one of \
             timelines_local, timelines_federated, profiles_local, profiles_remote, \
             activities_local, activities_remote",
            "the wording names the variable and the areas it takes"
        );
        assert!(err.source().is_none(), "there is no underlying error");
    }

    /// `Debug` shows the restricted areas: they name reads, not secrets.
    #[test]
    fn debug_shows_the_restricted_areas() {
        let debug = format!(
            "{:?}",
            config_with(&[(RESTRICT_UNAUTHENTICATED_VAR, "profiles_local")])
        );
        assert!(
            debug.contains("restrict_unauthenticated"),
            "the setting is visible in Debug output: {debug}"
        );
        assert!(
            debug.contains("profiles_local"),
            "and it shows which area is closed: {debug}"
        );
    }
}
