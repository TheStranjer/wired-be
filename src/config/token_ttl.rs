//! The per-role access-token lifetimes: the tiers, the three seconds
//! values, how the environment sets them and which values are acceptable.

use chrono::{TimeDelta, Utc};

use super::{
    defaults::{DEFAULT_OAUTH_TOKEN_TTL_SECS, DEFAULT_OAUTH_TOKEN_TTL_SECS_BOT},
    error::ConfigError,
    parse::number,
    vars::{
        OAUTH_TOKEN_TTL_SECS_ADMIN_VAR, OAUTH_TOKEN_TTL_SECS_BOT_VAR,
        OAUTH_TOKEN_TTL_SECS_USER_VAR, OAUTH_TOKEN_TTL_SECS_VAR,
    },
};
use crate::models::oauth_token::REFRESH_GRACE;

/// Which lifetime an issued access token gets, chosen from its user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenTier {
    /// An admin account, whatever its actor type.
    Admin,
    /// A non-admin account whose actor type is `Service`.
    Bot,
    /// Every other account, and the app-only `client_credentials` token.
    User,
}

impl TokenTier {
    /// The three tiers, in precedence order (admin, then bot, then user).
    pub const ALL: [Self; 3] = [Self::Admin, Self::Bot, Self::User];

    /// The tier's name: its admin-config subkey and its environment suffix.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Bot => "bot",
            Self::User => "user",
        }
    }
}

/// The access-token lifetime of each [`TokenTier`], in seconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokenTtls {
    /// Lifetime of an admin's token (`OAUTH_TOKEN_TTL_SECS_ADMIN`).
    pub admin: usize,
    /// Lifetime of a bot's token (`OAUTH_TOKEN_TTL_SECS_BOT`).
    pub bot: usize,
    /// Lifetime of any other token (`OAUTH_TOKEN_TTL_SECS_USER`).
    pub user: usize,
}

impl Default for TokenTtls {
    fn default() -> Self {
        Self {
            admin: DEFAULT_OAUTH_TOKEN_TTL_SECS,
            bot: DEFAULT_OAUTH_TOKEN_TTL_SECS_BOT,
            user: DEFAULT_OAUTH_TOKEN_TTL_SECS,
        }
    }
}

impl TokenTtls {
    /// The seconds `tier`'s tokens live.
    #[must_use]
    pub const fn secs(&self, tier: TokenTier) -> usize {
        match tier {
            TokenTier::Admin => self.admin,
            TokenTier::Bot => self.bot,
            TokenTier::User => self.user,
        }
    }
}

/// Whether a token lifetime of `secs` seconds is acceptable: positive, and
/// short enough that the expiry, and the end of the refresh grace after it,
/// are still timestamps the database and chrono can hold.
#[must_use]
pub fn ttl_secs_valid(secs: u64) -> bool {
    let Ok(grace) = TimeDelta::from_std(REFRESH_GRACE) else {
        return false;
    };
    secs > 0
        && i64::try_from(secs)
            .ok()
            .and_then(TimeDelta::try_seconds)
            .and_then(|ttl| Utc::now().checked_add_signed(ttl))
            .and_then(|valid_until| valid_until.checked_add_signed(grace))
            .is_some()
}

/// Reads one lifetime variable, `default` when it is unset or empty.
///
/// # Errors
///
/// [`ConfigError::InvalidNumber`] for a non-number and
/// [`ConfigError::InvalidTokenTtl`] for one [`ttl_secs_valid`] refuses.
fn ttl_secs<F>(lookup: &F, var: &'static str, default: usize) -> Result<usize, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let secs = number(lookup, var, default)?;
    if u64::try_from(secs).is_ok_and(ttl_secs_valid) {
        Ok(secs)
    } else {
        Err(ConfigError::InvalidTokenTtl(var))
    }
}

/// Reads the three tier lifetimes. The legacy `OAUTH_TOKEN_TTL_SECS` is the
/// default of the admin and user tiers when their own variable is unset;
/// the bot tier reads only its own variable.
///
/// # Errors
///
/// As [`ttl_secs`], for whichever variable is wrong.
pub(crate) fn token_ttls<F>(lookup: &F) -> Result<TokenTtls, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let legacy = ttl_secs(
        lookup,
        OAUTH_TOKEN_TTL_SECS_VAR,
        DEFAULT_OAUTH_TOKEN_TTL_SECS,
    )?;
    Ok(TokenTtls {
        admin: ttl_secs(lookup, OAUTH_TOKEN_TTL_SECS_ADMIN_VAR, legacy)?,
        bot: ttl_secs(
            lookup,
            OAUTH_TOKEN_TTL_SECS_BOT_VAR,
            DEFAULT_OAUTH_TOKEN_TTL_SECS_BOT,
        )?,
        user: ttl_secs(lookup, OAUTH_TOKEN_TTL_SECS_USER_VAR, legacy)?,
    })
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;
    use crate::config::test_support::{config_with, try_config_with};

    #[test]
    fn the_defaults_are_thirty_days_and_a_hundred_years() {
        let ttls = config_with(&[]).oauth_token_ttls;
        assert_eq!(ttls.admin, 2_592_000, "admin: 30 days");
        assert_eq!(ttls.user, 2_592_000, "user: 30 days");
        assert_eq!(ttls.bot, 3_153_600_000, "bot: 100 years");
        assert_eq!(ttls, TokenTtls::default(), "the default is the unset env");
    }

    #[test]
    fn each_tier_reads_its_own_variable() {
        let ttls = config_with(&[
            (OAUTH_TOKEN_TTL_SECS_ADMIN_VAR, "60"),
            (OAUTH_TOKEN_TTL_SECS_BOT_VAR, "120"),
            (OAUTH_TOKEN_TTL_SECS_USER_VAR, "180"),
        ])
        .oauth_token_ttls;
        assert_eq!(ttls.secs(TokenTier::Admin), 60, "admin");
        assert_eq!(ttls.secs(TokenTier::Bot), 120, "bot");
        assert_eq!(ttls.secs(TokenTier::User), 180, "user");
    }

    #[test]
    fn the_legacy_variable_defaults_admin_and_user_but_not_bot() {
        let ttls = config_with(&[(OAUTH_TOKEN_TTL_SECS_VAR, "600")]).oauth_token_ttls;
        assert_eq!(ttls.admin, 600, "admin falls back to the legacy value");
        assert_eq!(ttls.user, 600, "user falls back to the legacy value");
        assert_eq!(ttls.bot, 3_153_600_000, "bot ignores the legacy value");
    }

    #[test]
    fn a_tier_variable_beats_the_legacy_one() {
        let ttls = config_with(&[
            (OAUTH_TOKEN_TTL_SECS_VAR, "600"),
            (OAUTH_TOKEN_TTL_SECS_USER_VAR, "900"),
        ])
        .oauth_token_ttls;
        assert_eq!(ttls.user, 900, "the tier variable wins");
        assert_eq!(ttls.admin, 600, "the other tier keeps the legacy value");
    }

    #[test]
    fn a_hundred_years_is_accepted_for_every_tier() {
        let config = config_with(&[
            (OAUTH_TOKEN_TTL_SECS_ADMIN_VAR, "3153600000"),
            (OAUTH_TOKEN_TTL_SECS_USER_VAR, "3153600000"),
        ]);
        assert_eq!(config.oauth_token_ttls.admin, 3_153_600_000, "admin");
        assert_eq!(config.oauth_token_ttls.user, 3_153_600_000, "user");
    }

    #[test]
    fn zero_and_unrepresentable_lifetimes_are_startup_errors() {
        for var in [
            OAUTH_TOKEN_TTL_SECS_VAR,
            OAUTH_TOKEN_TTL_SECS_ADMIN_VAR,
            OAUTH_TOKEN_TTL_SECS_BOT_VAR,
            OAUTH_TOKEN_TTL_SECS_USER_VAR,
        ] {
            for value in ["0", "99999999999999", "18446744073709551615"] {
                let err = try_config_with(&[(var, value)]).unwrap_err();
                assert_eq!(err, ConfigError::InvalidTokenTtl(var), "{var}={value}");
                assert!(err.source().is_none(), "no source for {var}");
                assert!(
                    err.to_string().starts_with(&format!("invalid {var}: ")),
                    "the message names {var}"
                );
            }
            let err = try_config_with(&[(var, "many")]).unwrap_err();
            assert_eq!(err, ConfigError::InvalidNumber(var), "{var} is no number");
        }
    }

    #[test]
    fn ttl_secs_valid_bounds() {
        assert!(!ttl_secs_valid(0), "zero is no lifetime");
        assert!(ttl_secs_valid(1), "one second is");
        assert!(ttl_secs_valid(3_153_600_000), "a hundred years is");
        assert!(!ttl_secs_valid(u64::MAX), "too long for i64");
        assert!(
            !ttl_secs_valid(i64::MAX.unsigned_abs()),
            "too long for a date"
        );
    }

    #[test]
    fn tiers_name_themselves() {
        let names: Vec<&str> = TokenTier::ALL
            .iter()
            .copied()
            .map(TokenTier::as_str)
            .collect();
        assert_eq!(names, ["admin", "bot", "user"], "precedence order");
    }
}
