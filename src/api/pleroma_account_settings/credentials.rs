//! Credentials: changing the password and the email address, and the
//! confirmation mail.

use std::fmt;

use axum::{Json, extract::State};
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use super::{
    CONFIRM_EMAIL_PATH, EMAIL_INVALID_FORMAT, EMAIL_TAKEN, INVALID_PASSWORD, PASSWORD_MISMATCH,
    PASSWORD_TOO_SHORT, error_body, password_accepted, success_body,
};
use crate::{
    api::instance::sha256_hex,
    auth::AuthUser,
    error::AppResult,
    extract::ApiBody,
    mailer::{self, templates},
    models::{
        oauth_token,
        user::{self, User, get_by_email},
    },
    oauth::scopes::WRITE_ACCOUNTS,
    state::AppState,
    util::password::hash_password,
};

/// The parameters of `POST /api/pleroma/change_password`.
#[derive(Deserialize)]
pub(super) struct ChangePasswordParams {
    /// The account's current password; secret material.
    password: String,
    /// The password to store; secret material.
    new_password: String,
    /// The repeated [`new_password`](Self::new_password); secret material.
    new_password_confirmation: String,
}

/// All three fields are secret material and render as `<redacted>`.
impl fmt::Debug for ChangePasswordParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChangePasswordParams")
            .field("password", &"<redacted>")
            .field("new_password", &"<redacted>")
            .field("new_password_confirmation", &"<redacted>")
            .finish()
    }
}

/// The parameters of `POST /api/pleroma/change_email`.
#[derive(Deserialize)]
pub(super) struct ChangeEmailParams {
    /// The account's current password; secret material.
    password: String,
    /// The address to store.
    email: String,
}

/// `Debug` redacts the password.
impl fmt::Debug for ChangeEmailParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChangeEmailParams")
            .field("password", &"<redacted>")
            .field("email", &self.email)
            .finish()
    }
}

/// `POST /api/pleroma/change_password` — set a new password. The gates
/// run in order — the current password, then the confirmation, then the
/// length; every refusal is a 200 error body, as Pleroma renders them —
/// and a pass stores the new Argon2id hash and revokes every one of the
/// account's *other* OAuth tokens in one transaction. The credential the
/// request authenticated with keeps working; the password it was minted
/// for no longer exists, so every other client must log in again.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check. A wrong password, a mismatching
/// confirmation or a too-short new one answer with the 200 error bodies.
/// A hashing failure arrives through `?` as [`AppError::Internal`], any
/// database failure as [`AppError::Database`].
pub(super) async fn change_password(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<ChangePasswordParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    if !password_accepted(&auth.0, &params.password) {
        return Ok(error_body(INVALID_PASSWORD));
    }
    if params.new_password != params.new_password_confirmation {
        return Ok(error_body(PASSWORD_MISMATCH));
    }
    if params.new_password.chars().count() < state.config.min_password_length {
        return Ok(error_body(PASSWORD_TOO_SHORT));
    }
    let hash = hash_password(&params.new_password)?;
    let revoked = write_password_and_revoke(&state, &auth, &hash).await?;
    debug!(account = %auth.0.nickname, revoked, "password changed: other tokens revoked");
    Ok(success_body())
}

/// The `change_password` write: the new hash and the revocation of every
/// token but the request's own, as one transaction — half of it is never
/// a state an account is left in.
async fn write_password_and_revoke(
    state: &AppState,
    auth: &AuthUser,
    hash: &str,
) -> AppResult<u64> {
    let mut tx = state.db.begin().await?;
    user::set_password(&mut *tx, auth.0.id, hash).await?;
    let revoked = oauth_token::delete_all_for_user_except(&mut *tx, auth.0.id, auth.1.id).await?;
    tx.commit().await?;
    Ok(revoked)
}

/// `POST /api/pleroma/change_email` — set a new address. After the
/// password gate the trimmed address must look like `x@y.z` and belong
/// to no other account — both refusals are the endpoint's 200 error
/// bodies. The write stores the address; when the instance requires
/// email confirmation the account becomes unconfirmed and a confirmation
/// letter for the new address is queued (dropped silently while no SMTP
/// relay is configured, see [`crate::mailer`]).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check. The password, format and uniqueness
/// refusals are 200 error bodies. Any database failure arrives through
/// `?` as [`AppError::Database`].
pub(super) async fn change_email(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<ChangeEmailParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    if !password_accepted(&auth.0, &params.password) {
        return Ok(error_body(INVALID_PASSWORD));
    }
    let email = params.email.trim();
    if !user::register::is_plausible_email(email) {
        return Ok(error_body(EMAIL_INVALID_FORMAT));
    }
    if email_is_taken(&state, &auth.0, email).await? {
        return Ok(error_body(EMAIL_TAKEN));
    }
    let confirmed = !state.config.account_activation_required.get();
    user::set_email(&state.db, auth.0.id, email, confirmed).await?;
    if !confirmed {
        send_confirmation_mail(&state, &auth.0, email).await?;
    }
    debug!(account = %auth.0.nickname, confirmed, "email changed");
    Ok(success_body())
}

/// Whether `email` (already format-checked) is carried by an account
/// other than `account`: the citext column folds case, so "taken" means
/// taken in any spelling, while the account's own address is never
/// somebody else's taken.
async fn email_is_taken(state: &AppState, account: &User, email: &str) -> AppResult<bool> {
    Ok(get_by_email(&state.db, email)
        .await?
        .is_some_and(|holder| holder.id != account.id))
}

/// Queues the letter [`change_email`] sends a confirmation-requiring
/// instance: the registration-confirmation template addressed to the new
/// email, linking to [`CONFIRM_EMAIL_PATH`] with the account id and its
/// [`confirmation_token`].
async fn send_confirmation_mail(state: &AppState, account: &User, email: &str) -> AppResult<()> {
    let link = format!(
        "{}/{}/{}",
        CONFIRM_EMAIL_PATH,
        account.id,
        confirmation_token(account, email)
    );
    mailer::deliver_later(
        state,
        templates::registration_confirmation(&state.config, email, &link),
    )
    .await
}

/// The email-confirmation token for `email`: the lowercase hex `SHA-256`
/// of the account id, the address and the account's creation time. It is
/// derived rather than stored — the confirmation ticket that serves the
/// link can re-derive it from the row it reads, and because the address
/// is part of the digest, changing the email again invalidates the
/// letters already sent.
#[must_use]
fn confirmation_token(account: &User, email: &str) -> String {
    sha256_hex(&format!(
        "{}|{email}|{}",
        account.id,
        account.inserted_at.to_rfc3339()
    ))
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use serde_json::json;

    use super::super::{PasswordParams, test_support::template_user};
    use super::*;

    #[test]
    fn change_password_params_debug_redacts_all_three_secrets() {
        let params: ChangePasswordParams = serde_json::from_value(json!({
            "password": "hunter2",
            "new_password": "s3cret-new",
            "new_password_confirmation": "s3cret-new",
        }))
        .unwrap();
        let shown = format!("{params:?}");
        assert!(
            !shown.contains("hunter2") && !shown.contains("s3cret-new"),
            "no password material shows in Debug: {shown}"
        );
        assert!(
            shown.contains(r#"new_password_confirmation: "<redacted>""#),
            "the markers name the fields: {shown}"
        );
    }

    #[test]
    fn change_email_and_password_params_debug_redact_the_password() {
        let email: ChangeEmailParams = serde_json::from_value(json!({
            "password": "hunter2",
            "email": "new@example.org",
        }))
        .unwrap();
        let shown = format!("{email:?}");
        assert!(
            !shown.contains("hunter2") && shown.contains("new@example.org"),
            "the credential hides, the address shows: {shown}"
        );
        let bare: PasswordParams = serde_json::from_value(json!({"password": "hunter2"})).unwrap();
        let bare_shown = format!("{bare:?}");
        assert!(
            !bare_shown.contains("hunter2"),
            "the lone-password params hide their secret: {bare_shown}"
        );
    }

    #[test]
    fn the_confirmation_token_is_stable_and_address_bound() {
        let account = template_user("alice");
        let first = confirmation_token(&account, "new@example.org");
        assert_eq!(first.len(), 64, "a SHA-256 digest in lowercase hex");
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex, nothing else: {first}"
        );
        assert_eq!(
            first,
            confirmation_token(&account, "new@example.org"),
            "the same account and address derive the same token"
        );
        assert_ne!(
            first,
            confirmation_token(&account, "other@example.org"),
            "a different address invalidates the letter already sent"
        );
        let mut renamed = template_user("alice");
        renamed.inserted_at = account.inserted_at + Duration::seconds(1);
        assert_ne!(
            first,
            confirmation_token(&renamed, "new@example.org"),
            "the creation time salts the digest"
        );
    }
}
