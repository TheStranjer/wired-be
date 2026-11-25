//! Credentials: changing the password and the email address, and the
//! confirmation mail.

use std::fmt;

use axum::{Json, extract::State};
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use super::{
    EMAIL_INVALID_FORMAT, EMAIL_TAKEN, INVALID_PASSWORD, PASSWORD_MISMATCH, PASSWORD_TOO_SHORT,
    error_body, password_accepted, success_body,
};
use crate::{
    auth::AuthUser,
    error::AppResult,
    extract::ApiBody,
    models::{
        oauth_token,
        user::{self, User, confirmation, get_by_email},
    },
    oauth::scopes::WRITE_ACCOUNTS,
    protocols::events::{self, AccountEvent},
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
    let revoked =
        write_password_and_revoke(&state, &auth.0, auth.1.id, &params.new_password, &hash).await?;
    debug!(account = %auth.0.nickname, revoked, "password changed: other tokens revoked");
    Ok(success_body())
}

/// The `change_password` write: the new hash and the revocation of every
/// token but `keep_token`, as one transaction — half of it is never
/// a state an account is left in.
///
/// Right after the hash is set, every enabled protocol is handed the
/// plaintext `new_password` inside the transaction
/// ([`events::password_changed_in_tx`]); a protocol that refuses rolls the
/// change back. Once the transaction commits they all hear
/// [`AccountEvent::PasswordChanged`].
async fn write_password_and_revoke(
    state: &AppState,
    account: &User,
    keep_token: Uuid,
    new_password: &str,
    hash: &str,
) -> AppResult<u64> {
    let mut tx = state.db.begin().await?;
    user::set_password(&mut *tx, account.id, hash).await?;
    events::password_changed_in_tx(&mut tx, state, account, new_password).await?;
    let revoked = oauth_token::delete_all_for_user_except(&mut *tx, account.id, keep_token).await?;
    tx.commit().await?;
    events::dispatch(state, account, AccountEvent::PasswordChanged).await;
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
    if !confirmed && let Some(updated) = user::get_by_id(&state.db, auth.0.id).await? {
        confirmation::send(&state, &updated).await?;
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::PasswordParams;
    use super::*;
    use crate::{
        protocols::fixture::{Recorded, recorded_for},
        test_support::app_state_with_fixture,
        util::password::verify_password,
    };

    /// The password an account has before the change.
    const OLD_PASSWORD: &str = "old-password-1";
    /// The password the change sets.
    const NEW_PASSWORD: &str = "new-password-2";

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

    /// Inserts the local account `nickname` whose password is [`OLD_PASSWORD`].
    async fn account_with_old_password(state: &AppState, nickname: &str) -> User {
        let ap_id = user::local_ap_id(&state.config.base_url, nickname);
        let new = user::NewUser {
            password_hash: Some(hash_password(OLD_PASSWORD).expect("the password hashes")),
            ..user::NewUser::local(nickname, &ap_id)
        };
        user::insert(&state.db, &new)
            .await
            .expect("the account fixture inserts")
    }

    /// The stored password hash of `account`.
    async fn stored_hash(state: &AppState, account: &User) -> String {
        user::get_by_id(&state.db, account.id)
            .await
            .expect("the account reads")
            .expect("the account exists")
            .password_hash
            .expect("the account has a password")
    }

    #[tokio::test]
    async fn a_password_change_runs_the_in_transaction_hook_then_announces_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state_with_fixture(db.pool());
        let account = account_with_old_password(&state, "change_events").await;

        let hash = hash_password(NEW_PASSWORD).expect("the password hashes");
        let revoked =
            write_password_and_revoke(&state, &account, Uuid::now_v7(), NEW_PASSWORD, &hash)
                .await
                .expect("the change lands");

        assert_eq!(revoked, 0, "the account held no tokens to revoke");
        assert_eq!(
            recorded_for(account.id),
            vec![
                Recorded::PasswordChangedInTx,
                Recorded::Event(AccountEvent::PasswordChanged),
            ],
            "the in-transaction call came first, the event after the commit"
        );
        assert!(
            verify_password(NEW_PASSWORD, &stored_hash(&state, &account).await),
            "the new password"
        );
    }

    #[tokio::test]
    async fn a_failing_in_transaction_hook_rolls_the_password_change_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state_with_fixture(db.pool());
        let account = account_with_old_password(&state, "fail-hook-change").await;

        let hash = hash_password(NEW_PASSWORD).expect("the password hashes");
        let outcome =
            write_password_and_revoke(&state, &account, Uuid::now_v7(), NEW_PASSWORD, &hash).await;

        assert!(outcome.is_err(), "the hook's failure fails the change");
        assert!(
            verify_password(OLD_PASSWORD, &stored_hash(&state, &account).await),
            "the old password still works"
        );
        assert_eq!(
            recorded_for(account.id),
            vec![Recorded::PasswordChangedInTx],
            "no event for a change that never committed"
        );
    }
}
