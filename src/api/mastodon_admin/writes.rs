//! The account writes: delete, enable, approve and reject.

use std::slice;

use axum::{
    Json,
    extract::{Path, State},
};
use tracing::debug;

use super::{AREA_ACCOUNTS, CLASS_WRITE, NOT_ALLOWED, reads::named_account};
use crate::{
    api::helpers,
    auth::admin::AdminUser,
    entities::mastodon_admin::AdminAccount,
    error::{AppError, AppResult},
    mailer::{self, templates},
    models::{
        moderation_log,
        user::{User, admin},
    },
    state::AppState,
};

/// `DELETE /api/v1/admin/accounts/{id}` — Pleroma's `delete`: the account's
/// deletion, started the way every other admin deletion here is started, and
/// answered with the entity as the row stood when the request named it.
///
/// The self check is the ticket's: Pleroma's `delete` pipes through
/// `require_privileged_role_users_delete` and then announces whatever
/// account the path named, the caller's own included.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate, [`AppError::Forbidden`] with
/// [`NOT_ALLOWED`] for the admin's own account; [`AppError::NotFound`] from
/// [`named_account`]; [`AppError::Database`] from the writes and the log.
pub(super) async fn delete_account(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminAccount>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let account = named_account(&state, &raw).await?;
    if account.id == admin.0.id {
        return Err(AppError::Forbidden(NOT_ALLOWED.to_owned()));
    }
    helpers::start_deletion(&state, &account).await?;
    let logged = moderation_log::delete(&state.db, &admin.0, slice::from_ref(&account)).await?;
    debug!(
        log = %logged.id,
        account = %account.id,
        "admin deleted an account"
    );
    Ok(Json(AdminAccount::from_user(&account, &state.config)))
}

/// `POST /api/v1/admin/accounts/{id}/enable` — Pleroma's `enable`:
/// [`helpers::set_activation`] with the flag set, the fresh row's entity as the
/// answer and the `activate` entry Pleroma's own action writes.
///
/// The flag is the whole gate: the deactivation revoked no token and stamped
/// no deletion, so the account's own credential works again on this answer
/// and its actor document never stopped resolving (the `Unsuspended` event
/// has `ActivityPub` send the peers a fresh `Update` without the `suspended`
/// flag). Re-enabling an active account writes the same `true` back and
/// answers the same entity.
///
/// `enable` reverses a `disable`, not a suspension (WBE-878): a suspended
/// account is answered as it is, with no write and no entry, and
/// [`unsuspend_account`] is the route that lifts it.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::NotFound`] from
/// [`named_account`] or from the write matching no row;
/// [`AppError::Database`] from the write and the log.
pub(super) async fn enable_account(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminAccount>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let account = named_account(&state, &raw).await?;
    let answered = if account.suspended_at.is_some() {
        account
    } else {
        reactivate(&state, &admin.0, &account).await?
    };
    debug!(account = %answered.id, "admin enabled an account");
    Ok(Json(AdminAccount::from_user(&answered, &state.config)))
}

/// `POST /api/v1/admin/accounts/{id}/unsuspend` — Mastodon's `unsuspend`:
/// the reversal of a `suspend` (WBE-878), through the same activation write
/// and `activate` entry as [`enable_account`].
///
/// Only a suspended account moves. One that is active, or disabled without a
/// suspension, is answered as it is, with no write and no entry — `enable`
/// is what lifts a `disable`.
///
/// # Errors
///
/// As [`enable_account`].
pub(super) async fn unsuspend_account(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminAccount>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let account = named_account(&state, &raw).await?;
    let answered = if account.suspended_at.is_some() {
        reactivate(&state, &admin.0, &account).await?
    } else {
        account
    };
    debug!(account = %answered.id, "admin unsuspended an account");
    Ok(Json(AdminAccount::from_user(&answered, &state.config)))
}

/// Activates `account` through [`helpers::set_activation`] and answers the
/// row the write returned.
///
/// # Errors
///
/// [`AppError::NotFound`] when the write matches no row;
/// [`AppError::Database`] from the write and the log.
async fn reactivate(state: &AppState, actor: &User, account: &User) -> AppResult<User> {
    helpers::set_activation(state, actor, slice::from_ref(account), true)
        .await?
        .pop()
        .ok_or(AppError::NotFound)
}

/// `POST /api/v1/admin/accounts/{id}/approve` — Pleroma's `approve`: the
/// console's batch approval for one account by id, its write and its
/// `account_approved` letter shared through [`helpers::approve_account`].
///
/// An account nobody left pending is Pleroma's
/// `approve(%User{is_approved: true})` arm: handed back unchanged, mailed
/// nothing, written nothing — and still the answer's entity and still a log
/// subject. The approval mints no credential; the account signs in with the
/// password it registered.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::NotFound`] from
/// [`named_account`]; [`AppError::Database`] from the write, the letter's
/// queueing and the log.
pub(super) async fn approve_account(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminAccount>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let account = named_account(&state, &raw).await?;
    let answered = if account.approval_pending.get() {
        helpers::approve_account(&state, account).await?
    } else {
        account
    };
    let logged = moderation_log::approve(&state.db, &admin.0, slice::from_ref(&answered)).await?;
    debug!(
        log = %logged.id,
        account = %answered.id,
        "admin approved an account"
    );
    Ok(Json(AdminAccount::from_user(&answered, &state.config)))
}

/// `POST /api/v1/admin/accounts/{id}/reject` — Pleroma's `reject`: the
/// pending account's row deleted outright (Pleroma's `User.reject` runs
/// `User.delete`, whose `maybe_delete_from_db` arm drops the row of an
/// account that never finished approving rather than only purging it) and
/// its `account_rejected` letter posted to the address on that row.
///
/// The entity answers as the row was read: the deletion took the row with
/// it, and `approved: false` is the state the admin just refused. The log
/// entry, written after the delete, is what survives of the account — its
/// subject columns carry no key, so naming a gone row is what they are for.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate, [`AppError::Forbidden`] with [`NOT_ALLOWED`]
/// for an account that was not pending; [`AppError::NotFound`] from
/// [`named_account`] or from the write matching no row;
/// [`AppError::Database`] from the delete, the letter's queueing and the log.
pub(super) async fn reject_account(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminAccount>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let account = named_account(&state, &raw).await?;
    if !account.approval_pending.get() {
        return Err(AppError::Forbidden(NOT_ALLOWED.to_owned()));
    }
    if !admin::delete_user(&state.db, account.id).await? {
        return Err(AppError::NotFound);
    }
    if let Some(email) = account.email.as_deref() {
        mailer::deliver_later(&state, templates::account_rejected(&state.config, email)).await?;
    }
    let logged = moderation_log::reject(&state.db, &admin.0, slice::from_ref(&account)).await?;
    debug!(
        log = %logged.id,
        account = %account.id,
        nickname = %account.nickname,
        "admin rejected an account"
    );
    Ok(Json(AdminAccount::from_user(&account, &state.config)))
}
