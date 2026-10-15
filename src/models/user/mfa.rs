//! The `users.mfa_settings` column: multi-factor authentication state
//! (WBE-131).
//!
//! One jsonb value holds the whole per-account MFA state, shaped like
//! Pleroma's `multi_factor_authentication_settings` embedded schema
//! (`lib/pleroma/mfa/settings.ex`): the instance-level `enabled` flag, the
//! TOTP method ([`TotpSettings`]: a base32 shared secret and whether the
//! user proved possession of it), and the SHA-256 hex hashes of the issued
//! backup codes. The migration default and [`MfaSettings::default_value`]
//! agree on one spelling of "MFA off", and [`MfaSettings::from_value`]
//! parses leniently (any missing or malformed piece reads as its default),
//! so a row written before a later field existed still decodes.
//!
//! The TOTP secret is secret material that must survive a round trip
//! (codes are verified against it), so it is stored as-is — but it never
//! renders in `Debug` output ([`TotpSettings`]'s manual impl redacts it),
//! and nothing here logs it. Backup codes are stored only as hashes
//! ([`hash_backup_code`](crate::util::totp::hash_backup_code)); the
//! plaintext is shown exactly once, at generation.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::User;
use crate::error::AppResult;

/// The `mfa_settings` jsonb column: everything the MFA endpoints
/// ([`crate::api::mfa`]) read and change about an account's second factor.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MfaSettings {
    /// Whether any MFA method is enabled: set when TOTP is confirmed,
    /// cleared when it is disabled.
    pub enabled: bool,
    /// The TOTP method.
    pub totp: TotpSettings,
    /// SHA-256 hex hashes of the backup codes currently issued (their
    /// plaintexts are never stored).
    pub backup_codes: Vec<String>,
}

/// The `totp` piece of [`MfaSettings`].
///
/// The manual [`Debug`] impl redacts [`secret`](Self::secret): the base32
/// seed is what an attacker with log access would need to mint codes.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TotpSettings {
    /// The base32 shared secret, stored as-is so codes verify against it;
    /// `None` until setup runs. Never in `Debug` output.
    pub secret: Option<String>,
    /// Whether the user entered a code from their authenticator, proving
    /// possession of the secret.
    pub confirmed: bool,
}

/// `secret` renders as `<redacted>` when set; `confirmed` shows.
impl fmt::Debug for TotpSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TotpSettings")
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .field("confirmed", &self.confirmed)
            .finish()
    }
}

impl MfaSettings {
    /// The column value of an account that never touched MFA: exactly the
    /// migration default.
    #[must_use]
    pub fn default_value() -> Value {
        Self::default().to_value()
    }

    /// Reads `settings` out of a stored column [`Value`], falling back to
    /// the all-off default for anything unrecognised: a missing piece
    /// decodes as its default, and a value of the wrong shape never fails
    /// a user read.
    #[must_use]
    pub fn from_value(value: &Value) -> Self {
        serde_json::from_value(value.clone()).unwrap_or_default()
    }

    /// The parsed `mfa_settings` column of `user`.
    #[must_use]
    pub fn for_user(user: &User) -> Self {
        Self::from_value(&user.mfa_settings)
    }

    /// The canonical column [`Value`] of these settings: always the full
    /// migration shape, so a stored row never drifts.
    #[must_use]
    pub fn to_value(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "totp": {
                "secret": self.totp.secret,
                "confirmed": self.totp.confirmed,
            },
            "backup_codes": self.backup_codes,
        })
    }

    /// Whether the TOTP method is enabled, which is what `GET
    /// /api/pleroma/accounts/mfa` renders as `settings.totp` (Pleroma's
    /// `method_enabled?(:totp, …)`).
    #[must_use]
    pub const fn totp_enabled(&self) -> bool {
        self.totp.confirmed
    }

    /// Installs a fresh unconfirmed `secret`, replacing any earlier
    /// unconfirmed one (Pleroma's `MFA.setup_totp`); nothing else changes.
    pub fn setup_totp(&mut self, secret: &str) {
        self.totp = TotpSettings {
            secret: Some(secret.to_owned()),
            confirmed: false,
        };
    }

    /// Marks TOTP confirmed and enables MFA (Pleroma's
    /// `Changeset.confirm_totp`); the secret stays for future logins.
    pub const fn confirm_totp(&mut self) {
        self.totp.confirmed = true;
        self.enabled = true;
    }

    /// Replaces the stored backup-code hashes with `code_hashes` (the
    /// old codes stop working, Pleroma's `cast_backup_codes`).
    pub fn rotate_backup_codes(&mut self, code_hashes: Vec<String>) {
        self.backup_codes = code_hashes;
    }

    /// Disables TOTP and with it MFA, clearing the secret and the backup
    /// codes (Pleroma's `MFA.disable_totp` plus `Changeset.disable`).
    pub fn disable_totp(&mut self) {
        self.totp = TotpSettings::default();
        self.backup_codes.clear();
        self.enabled = false;
    }
}

/// The `UPDATE` behind [`save`]: writes the whole column in one statement
/// and refreshes `updated_at`.
const UPDATE_SQL: &str = "UPDATE users SET mfa_settings = $2, updated_at = now() WHERE id = $1";

/// Stores `settings` as user `id`'s `mfa_settings` column.
///
/// Unknown ids match no row and change nothing; the endpoints run on an
/// authenticated user, so the row exists.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn save(pool: &PgPool, id: Uuid, settings: &MfaSettings) -> AppResult<()> {
    sqlx::query(UPDATE_SQL)
        .bind(id)
        .bind(settings.to_value())
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::Row as _;

    use super::*;
    use crate::{
        models::user::{NewUser, insert, local_ap_id},
        util::totp::hash_backup_code,
    };

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = "https://test.example";
    /// A base32 secret that must never surface in `Debug` output.
    const SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    /// The migration's `DEFAULT` spelling, verbatim.
    fn migration_default() -> Value {
        serde_json::from_str(
            r#"{"enabled": false, "totp": {"secret": null, "confirmed": false}, "backup_codes": []}"#,
        )
        .unwrap()
    }

    #[test]
    fn default_value_is_the_migration_default_shape() {
        assert_eq!(
            MfaSettings::default_value(),
            migration_default(),
            "Rust and SQL agree on \"MFA off\""
        );
    }

    #[test]
    fn settings_round_trip_through_their_value() {
        let settings = MfaSettings {
            enabled: true,
            totp: TotpSettings {
                secret: Some(SECRET.to_owned()),
                confirmed: true,
            },
            backup_codes: vec![hash_backup_code("abcd1234")],
        };
        let value = settings.to_value();
        assert_eq!(
            MfaSettings::from_value(&value),
            settings,
            "to_value/from_value is the identity"
        );
        assert_eq!(
            value["totp"]["secret"].as_str(),
            Some(SECRET),
            "the secret is stored as-is so codes can verify against it"
        );
    }

    #[test]
    fn parsing_is_lenient_about_missing_and_broken_pieces() {
        assert_eq!(
            MfaSettings::from_value(&json!({"enabled": true})),
            MfaSettings {
                enabled: true,
                totp: TotpSettings::default(),
                backup_codes: Vec::new(),
            },
            "absent pieces read as their defaults"
        );
        for junk in [json!(null), json!("nope"), json!({"totp": 4})] {
            assert_eq!(
                MfaSettings::from_value(&junk),
                MfaSettings::default(),
                "`{junk}` is not settings: it decodes as all-off"
            );
        }
    }

    #[test]
    fn extra_keys_are_tolerated() {
        // Pleroma's schema also carries `delivery_type`; a row imported
        // from there must still parse.
        let settings = MfaSettings::from_value(&json!({
            "enabled": false,
            "totp": {"secret": SECRET, "delivery_type": "app", "confirmed": false},
            "backup_codes": [],
        }));
        assert_eq!(settings.totp.secret.as_deref(), Some(SECRET));
        assert!(!settings.totp.confirmed, "the known pieces parsed");
    }

    #[test]
    fn setup_replaces_the_secret_and_keeps_the_rest() {
        let mut settings = MfaSettings {
            enabled: false,
            totp: TotpSettings {
                secret: Some("OLDOLDOLD".to_owned()),
                confirmed: false,
            },
            backup_codes: vec!["hash".to_owned()],
        };
        settings.setup_totp(SECRET);
        assert_eq!(settings.totp.secret.as_deref(), Some(SECRET));
        assert!(
            !settings.totp.confirmed,
            "a fresh secret starts unconfirmed"
        );
        assert!(!settings.enabled, "setup alone never enables MFA");
        assert_eq!(
            settings.backup_codes,
            ["hash"],
            "Pleroma's setup leaves the backup codes alone"
        );
    }

    #[test]
    fn confirm_enables_mfa_and_keeps_the_secret() {
        let mut settings = MfaSettings::default();
        settings.setup_totp(SECRET);
        settings.confirm_totp();
        assert!(settings.enabled, "confirming enables MFA");
        assert!(settings.totp_enabled(), "and the method shows as enabled");
        assert_eq!(
            settings.totp.secret.as_deref(),
            Some(SECRET),
            "the secret survives: logins verify against it"
        );
    }

    #[test]
    fn disable_clears_everything() {
        let mut settings = MfaSettings {
            enabled: true,
            totp: TotpSettings {
                secret: Some(SECRET.to_owned()),
                confirmed: true,
            },
            backup_codes: vec!["hash".to_owned()],
        };
        settings.disable_totp();
        assert_eq!(settings, MfaSettings::default(), "back to all-off");
    }

    #[test]
    fn backup_code_rotation_replaces_the_hashes() {
        let mut settings = MfaSettings::default();
        settings.rotate_backup_codes(vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(settings.backup_codes, ["a", "b"]);
        settings.rotate_backup_codes(vec!["c".to_owned()]);
        assert_eq!(settings.backup_codes, ["c"], "the old hashes are gone");
    }

    #[test]
    fn debug_never_shows_the_secret() {
        let settings = MfaSettings {
            enabled: true,
            totp: TotpSettings {
                secret: Some(SECRET.to_owned()),
                confirmed: true,
            },
            backup_codes: Vec::new(),
        };
        let debug = format!("{settings:?}");
        assert!(!debug.contains(SECRET), "the secret must not leak: {debug}");
        assert!(
            debug.contains(r#"secret: Some("<redacted>")"#),
            "the redaction marker names the field: {debug}"
        );
        let unset = format!("{:?}", TotpSettings::default());
        assert!(
            unset.contains("secret: None"),
            "an unset secret reads as None: {unset}"
        );
    }

    #[tokio::test]
    async fn new_rows_carry_the_migration_default() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = insert(&pool, &NewUser::local("erin", &local_ap_id(BASE, "erin")))
            .await
            .unwrap();
        assert_eq!(
            user.mfa_settings,
            migration_default(),
            "the column default survives the round trip"
        );
        assert_eq!(
            MfaSettings::for_user(&user),
            MfaSettings::default(),
            "and parses as all-off"
        );
    }

    #[tokio::test]
    async fn save_round_trips_through_the_column() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = insert(&pool, &NewUser::local("fern", &local_ap_id(BASE, "fern")))
            .await
            .unwrap();
        let mut settings = MfaSettings::for_user(&user);
        settings.setup_totp(SECRET);
        settings.rotate_backup_codes(vec![hash_backup_code("abcd1234")]);
        save(&pool, user.id, &settings).await.unwrap();

        let stored: Value = sqlx::query("SELECT mfa_settings FROM users WHERE id = $1")
            .bind(user.id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("mfa_settings");
        assert_eq!(
            MfaSettings::from_value(&stored),
            settings,
            "what was saved is what comes back"
        );
        assert_eq!(
            stored["totp"]["secret"].as_str(),
            Some(SECRET),
            "the secret is recoverable from the column"
        );

        // An unknown id matches no row and is not an error.
        save(&pool, Uuid::now_v7(), &MfaSettings::default())
            .await
            .unwrap();
    }
}
