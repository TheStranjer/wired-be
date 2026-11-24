//! Two-factor authentication endpoints (`/api/pleroma/accounts/mfa`,
//! WBE-131): TOTP setup, confirmation, backup codes and disabling.
//!
//! The reference is Pleroma's `TwoFactorAuthenticationController`
//! (`lib/pleroma/web/pleroma_api/controllers/two_factor_authentication_controller.ex`)
//! and `Pleroma.MFA` (`lib/pleroma/mfa.ex`); the ticket's shapes are
//! authoritative. State lives in the `users.mfa_settings` column, parsed
//! and stored through [`MfaSettings`](mfa::MfaSettings); the TOTP primitives (secret, code
//! verification, provisioning URI, backup codes and their hashes) come
//! from [`crate::util::totp`].
//!
//! Every route requires a user token ([`AuthUser`]) and, per Pleroma's
//! `OAuthScopesPlug` pipeline, the `read:security` scope for reading
//! settings and the `write:security` scope for everything that changes
//! state — including generating backup codes.
//!
//! The endpoints, with the state machine they drive:
//!
//! - `GET  /api/pleroma/accounts/mfa` — `{settings: {enabled, totp}}`,
//!   where `totp` reports the *confirmed* flag, not just the secret.
//! - `GET  /api/pleroma/accounts/mfa/setup/totp` — mint a fresh
//!   unconfirmed secret (replacing an earlier unconfirmed one) and return
//!   `{key, provisioning_uri}`; 422 "TOTP already enabled" once confirmed.
//! - `POST /api/pleroma/accounts/mfa/confirm/totp` — `password` plus
//!   `code`; confirms the secret, flips `enabled`, answers `{}`. Wrong
//!   password 422 "Invalid password.", wrong code 422
//!   "`invalid_token`", no prior setup 422 "Setup required".
//! - `GET  /api/pleroma/accounts/mfa/backup_codes` — issue five new codes
//!   (old ones stop working), store their hashes, return the plaintexts
//!   once; requires MFA enabled.
//! - `DELETE /api/pleroma/accounts/mfa/totp` — `password`; clears the
//!   secret, the backup codes and `enabled`, answers `{}`.
//!
//! The password is checked with [`User::password_matches`] against the
//! stored hash; accounts without one (remote mirrors, bot-created rows) simply
//! fail the check. Secrets and codes never reach a log line or an error
//! message.

use std::fmt;

use axum::{
    Json, Router,
    extract::State,
    routing::{delete, get, post},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    auth::AuthUser,
    error::{AppError, AppResult},
    extract::ApiParams,
    models::user::{User, mfa},
    oauth::scopes::{READ_SECURITY, WRITE_SECURITY},
    state::AppState,
    util::totp,
};

/// 422 body when TOTP is already confirmed and setup is requested again
/// (the ticket's wording for Pleroma's "already enabled").
const ALREADY_ENABLED: &str = "TOTP already enabled";
/// 422 body when the submitted password does not match the stored hash
/// (Pleroma's `confirm_current_password` wording, period included).
const INVALID_PASSWORD: &str = "Invalid password.";
/// 422 body when the submitted TOTP code does not verify (Pleroma renders
/// the `:invalid_token` atom's spelling).
const INVALID_TOKEN: &str = "invalid_token";
/// 422 body when confirmation runs before any setup produced a secret.
const SETUP_REQUIRED: &str = "Setup required";
/// 422 body when backup codes are requested for an account with MFA off.
const MFA_NOT_ENABLED: &str = "MFA not enabled";
/// Backup codes issued per generation (Pleroma's
/// `[:instance, :multi_factor_authentication, :backup_codes, :number]`
/// default of 5).
const BACKUP_CODE_COUNT: usize = 5;

/// The `/api/pleroma/accounts/mfa` router, merged into
/// [`crate::api::pleroma`].
///
/// Do not call [`Router::fallback`] here (or on anything merged into this
/// tree): the app-wide JSON fallback in [`crate::app`] must answer the
/// unmatched paths.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(get_settings))
        .route("/setup/totp", get(setup_totp))
        .route("/confirm/totp", post(confirm_totp))
        .route("/backup_codes", get(get_backup_codes))
        .route("/totp", delete(disable_totp))
}

/// The `{settings: {enabled, totp}}` body: the MFA switch and whether the
/// TOTP method is confirmed (Pleroma's `MFA.mfa_settings/1`).
#[derive(Debug, Serialize)]
struct SettingsBody {
    /// The settings object Pleroma nests under `settings`.
    settings: SettingsFlags,
}

/// The flags inside [`SettingsBody::settings`].
#[derive(Debug, Serialize)]
struct SettingsFlags {
    /// Whether any MFA method is enabled for the account.
    enabled: bool,
    /// Whether the TOTP method is confirmed (`Pleroma.MFA.method_enabled?`).
    totp: bool,
}

/// `GET /api/pleroma/accounts/mfa` — the account's MFA settings.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read:security` check.
async fn get_settings(auth: AuthUser) -> AppResult<Json<SettingsBody>> {
    auth.require(READ_SECURITY)?;
    let settings = mfa_of(&auth.0);
    Ok(Json(SettingsBody {
        settings: SettingsFlags {
            enabled: settings.enabled,
            totp: settings.totp_enabled(),
        },
    }))
}

/// The parsed `mfa_settings` of `user`, read through the model's
/// [`MfaSettings`](mfa::MfaSettings) so the shape lives in one place.
fn mfa_of(user: &User) -> mfa::MfaSettings {
    mfa::MfaSettings::for_user(user)
}

/// `GET /api/pleroma/accounts/mfa/setup/totp` — mint a fresh unconfirmed
/// secret and hand back `{key, provisioning_uri}` (the authenticator
/// app's QR payload, issuer = `INSTANCE_NAME`, account = the nickname).
/// Replacing an *unconfirmed* secret is allowed; a confirmed one is not.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`ALREADY_ENABLED`] when TOTP is
/// confirmed; auth and scope errors as in [`get_settings`]. Any database
/// failure arrives through `?` as [`AppError::Database`].
async fn setup_totp(State(state): State<AppState>, auth: AuthUser) -> AppResult<Json<Value>> {
    auth.require(WRITE_SECURITY)?;
    let mut settings = mfa_of(&auth.0);
    if settings.totp_enabled() {
        return Err(AppError::Unprocessable(ALREADY_ENABLED.to_owned()));
    }
    let secret = totp::generate_secret();
    settings.setup_totp(&secret);
    mfa::save(&state.db, auth.0.id, &settings).await?;
    let uri = totp::provisioning_uri(&secret, &auth.0.nickname, &state.config.instance_name);
    Ok(Json(json!({"key": secret, "provisioning_uri": uri})))
}

/// The parameters of `POST …/confirm/totp`: both required, so a request
/// without them is the extractor's 422, and neither renders in `Debug`.
#[derive(Deserialize)]
struct ConfirmParams {
    /// The account's current password; secret material.
    password: String,
    /// The 6-digit TOTP code from the authenticator; secret material.
    code: String,
}

/// `Debug` redacts both fields: they are the password and a live code.
impl fmt::Debug for ConfirmParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfirmParams")
            .field("password", &"<redacted>")
            .field("code", &"<redacted>")
            .finish()
    }
}

/// `POST /api/pleroma/accounts/mfa/confirm/totp` — verify the password and
/// the code, then mark TOTP confirmed and enable MFA. Answers `{}`.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_PASSWORD`] for a wrong (or
/// absent-hash) password, [`SETUP_REQUIRED`] when no secret was set up,
/// [`INVALID_TOKEN`] when the code does not verify. Auth and scope errors
/// as in [`get_settings`]. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn confirm_totp(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiParams(params): ApiParams<ConfirmParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_SECURITY)?;
    require_password(&auth.0, &params.password)?;
    let mut settings = mfa_of(&auth.0);
    let Some(secret) = settings.totp.secret.as_deref() else {
        return Err(AppError::Unprocessable(SETUP_REQUIRED.to_owned()));
    };
    if !totp::verify(secret, &params.code, Utc::now().timestamp()) {
        return Err(AppError::Unprocessable(INVALID_TOKEN.to_owned()));
    }
    settings.confirm_totp();
    mfa::save(&state.db, auth.0.id, &settings).await?;
    Ok(empty_body())
}

/// The parameters of `DELETE …/totp`.
#[derive(Deserialize)]
struct PasswordParams {
    /// The account's current password; secret material.
    password: String,
}

/// `Debug` redacts the password.
impl fmt::Debug for PasswordParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasswordParams")
            .field("password", &"<redacted>")
            .finish()
    }
}

/// `GET /api/pleroma/accounts/mfa/backup_codes` — replace the account's
/// backup codes with five freshly generated ones and return the plaintexts
/// once; only the hashes are stored.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`MFA_NOT_ENABLED`] when MFA is off.
/// Auth and scope errors as in [`get_settings`] (this is a `write:security`
/// action in Pleroma, despite being a `GET`). Any database failure arrives
/// through `?` as [`AppError::Database`].
async fn get_backup_codes(State(state): State<AppState>, auth: AuthUser) -> AppResult<Json<Value>> {
    auth.require(WRITE_SECURITY)?;
    let mut settings = mfa_of(&auth.0);
    if !settings.enabled {
        return Err(AppError::Unprocessable(MFA_NOT_ENABLED.to_owned()));
    }
    let codes = totp::generate_backup_codes(BACKUP_CODE_COUNT);
    let hashes = codes
        .iter()
        .map(|code| totp::hash_backup_code(code))
        .collect();
    settings.rotate_backup_codes(hashes);
    mfa::save(&state.db, auth.0.id, &settings).await?;
    Ok(Json(json!({"backup_codes": codes})))
}

/// `DELETE /api/pleroma/accounts/mfa/totp` — disable TOTP and MFA,
/// clearing the secret and the backup codes. Answers `{}`.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_PASSWORD`] for a wrong (or
/// absent-hash) password. Auth and scope errors as in [`get_settings`].
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn disable_totp(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiParams(params): ApiParams<PasswordParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_SECURITY)?;
    require_password(&auth.0, &params.password)?;
    let mut settings = mfa_of(&auth.0);
    settings.disable_totp();
    mfa::save(&state.db, auth.0.id, &settings).await?;
    Ok(empty_body())
}

/// Checks `password` against the user's stored hash. A remote mirror or
/// any row without a hash fails exactly like a wrong password (Pleroma's
/// `confirm_current_password` answers both with `"Invalid password."`).
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_PASSWORD`].
fn require_password(user: &User, password: &str) -> AppResult<()> {
    if user.password_matches(password) {
        Ok(())
    } else {
        Err(AppError::Unprocessable(INVALID_PASSWORD.to_owned()))
    }
}

/// The `{}` body Pleroma's confirm and disable actions answer with.
fn empty_body() -> Json<Value> {
    Json(Value::Object(Map::new()))
}

#[cfg(test)]
mod tests {
    use serde_json::from_value;

    use super::*;

    /// The settings body `get_settings` would render for the column value
    /// `value` (the pure half of the handler, testable without a router).
    fn body_of(value: &Value) -> Value {
        let settings = mfa::MfaSettings::from_value(value);
        serde_json::to_value(SettingsBody {
            settings: SettingsFlags {
                enabled: settings.enabled,
                totp: settings.totp_enabled(),
            },
        })
        .unwrap()
    }

    #[test]
    fn settings_body_reports_enabled_and_confirmed_totp() {
        assert_eq!(
            body_of(&mfa::MfaSettings::default_value()),
            json!({"settings": {"enabled": false, "totp": false}}),
            "a fresh account is all-off"
        );
        assert_eq!(
            body_of(&json!({"enabled": true, "totp": {"secret": "X", "confirmed": true}})),
            json!({"settings": {"enabled": true, "totp": true}}),
            "confirmed TOTP shows as enabled"
        );
        assert_eq!(
            body_of(&json!({"enabled": false, "totp": {"secret": "X", "confirmed": false}})),
            json!({"settings": {"enabled": false, "totp": false}}),
            "an unconfirmed secret is not yet a method"
        );
        assert_eq!(
            body_of(&json!({"enabled": false})),
            json!({"settings": {"enabled": false, "totp": false}}),
            "a missing totp piece defaults"
        );
    }

    #[test]
    fn confirm_params_redact_their_secrets_and_password_params_the_password() {
        let confirm: ConfirmParams = from_value(json!({"password": "hunter2pw", "code": "123456"}))
            .expect("both fields present");
        let debug = format!("{confirm:?}");
        assert!(
            !debug.contains("hunter2pw") && !debug.contains("123456"),
            "password and code never appear in Debug: {debug}"
        );
        let password: PasswordParams = from_value(json!({"password": "hunter2pw"})).unwrap();
        assert!(
            !format!("{password:?}").contains("hunter2pw"),
            "the disable password is redacted too"
        );
    }

    #[test]
    fn empty_body_serializes_as_an_empty_object() {
        let Json(value) = empty_body();
        assert_eq!(
            value,
            json!({}),
            "Pleroma's confirm/disable answer is the empty object"
        );
    }

    #[test]
    fn backup_code_count_is_pleromas_default() {
        assert_eq!(BACKUP_CODE_COUNT, 5, "Pleroma issues five codes");
    }
}
