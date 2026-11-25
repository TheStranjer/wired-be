//! Departure: deleting and disabling the account.

use axum::{Json, extract::State};
use serde_json::Value;
use tracing::debug;

use super::{
    DELETION_DISABLED, INVALID_PASSWORD, PasswordParams, error_body, password_accepted,
    success_body,
};
use crate::{
    api::helpers::start_deletion,
    auth::AuthUser,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        oauth_token,
        user::{self, User},
    },
    oauth::scopes::WRITE_ACCOUNTS,
    protocols::events::{AccountEvent, dispatch},
    state::AppState,
};

/// `POST /api/pleroma/delete_account` — start deleting the account. The
/// config gate comes first: on an instance that switched deletion off
/// the endpoint answers 403 `{"error":"Account deletion is disabled"}`
/// whatever else the request said. Past it, the password gate, then the
/// start ([`start_deletion`]): the account is deactivated with `deleted_at`
/// stamped and all its tokens — the presenting one included — revoked in
/// one transaction, the [`users.delete`](crate::jobs::deletion::KIND_DELETE)
/// sweep is queued on [`QUEUE_BACKGROUND`](crate::jobs::QUEUE_BACKGROUND).
/// The [`AccountEvent::Deleted`] event is the
/// sweep's own first act, not the endpoint's: a protocol announcing the
/// deletion reads the relationship rows the sweep is about to clear, and a
/// client that gets this 200 must not wait on either.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`],
/// the `write:accounts` check and the config gate. A wrong password is
/// the 200 error body. Any database failure arrives through `?` as
/// [`AppError::Database`].
pub(super) async fn delete_account(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<PasswordParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    if !state.config.allow_account_deletion.get() {
        return Err(AppError::Forbidden(DELETION_DISABLED.to_owned()));
    }
    if !password_accepted(&auth.0, &params.password) {
        return Ok(error_body(INVALID_PASSWORD));
    }
    start_deletion(&state, &auth.0).await?;
    Ok(success_body())
}

/// `POST /api/pleroma/disable_account` — deactivate the account and keep
/// its data (Pleroma's `User.set_activation(user, false)`): the same
/// write as [`delete_account`]'s mark without the `deleted_at` stamp and
/// without the sweep, and its tokens are revoked whole because a
/// disabled account must not still be acting.
///
/// After the write it fires [`AccountEvent::Deactivated`]. `ActivityPub`
/// publishes nothing for it: a deactivation leaves the actor document
/// resolvable so the posts and follows peers already hold keep rendering.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check. A wrong password is the 200 error
/// body. Any database failure arrives through `?` as [`AppError::Database`].
pub(super) async fn disable_account(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<PasswordParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    if !password_accepted(&auth.0, &params.password) {
        return Ok(error_body(INVALID_PASSWORD));
    }
    let revoked = deactivate_and_revoke_all(&state, &auth.0).await?;
    dispatch(&state, &auth.0, AccountEvent::Deactivated).await;
    debug!(account = %auth.0.nickname, revoked, "account disabled");
    Ok(success_body())
}

/// The `disable_account` write: `is_active = false` and every token
/// gone, as one transaction.
async fn deactivate_and_revoke_all(state: &AppState, account: &User) -> AppResult<u64> {
    let mut tx = state.db.begin().await?;
    user::set_activation(&mut *tx, account.id, false).await?;
    let revoked = oauth_token::delete_all_for_user(&mut *tx, account.id).await?;
    tx.commit().await?;
    Ok(revoked)
}
