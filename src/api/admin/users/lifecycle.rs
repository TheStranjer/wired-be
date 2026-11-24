//! The account lifecycle: delete, activate, deactivate, toggle and approve.

use std::slice;

use axum::{
    Json,
    extract::{Path, State},
};
use tracing::debug;

use super::maintenance::NicknamesBody;
use super::tags::named_account;
use super::{AREA_ACCOUNTS, CLASS_WRITE};
use crate::{
    api::helpers,
    auth::admin::AdminUser,
    entities::admin_account::{AdminAccount, AdminAccounts},
    error::{AppError, AppResult},
    extract::{ApiBody, ApiParams},
    models::{
        moderation_log,
        user::{User, admin},
    },
    state::AppState,
};

/// The 403 body of a toggle that would deactivate the caller's own account,
/// worded like the Mastodon admin API's refusal of the same kind.
const SELF_DEACTIVATION: &str = "This action is not allowed";

/// `DELETE /api/v1/pleroma/admin/users` — Pleroma's `delete`: every named
/// account that exists starts its deletion, and the answer is the JSON
/// array of the nicknames started. Two divergences from Pleroma's
/// `do_deletes`, both the ticket's: unknown handles are skipped inside the
/// same success (where a Pleroma miss dies on the nil user's `ap_id`), and
/// the caller's own account is ignored — an admin cannot delete themselves
/// out of the instance through their own console.
///
/// The `nicknames[]` arrives in the body or the query string, bracketed or
/// bare, through [`ApiParams`].
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] from the
/// extractor for a request that names nothing; [`AppError::Database`] from
/// the lookups, the writes, the queueing and the log.
pub(super) async fn delete_users(
    State(state): State<AppState>,
    admin: AdminUser,
    params: ApiParams<NicknamesBody>,
) -> AppResult<Json<Vec<String>>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let found = admin::resolve_nicknames(&state.db, &params.0.nicknames).await?;
    let subjects: Vec<User> = found
        .into_iter()
        .filter(|account| account.id != admin.0.id)
        .collect();
    for account in &subjects {
        helpers::start_deletion(&state, account).await?;
    }
    let nicknames: Vec<String> = subjects
        .iter()
        .map(|account| account.nickname.clone())
        .collect();
    let logged = moderation_log::delete(&state.db, &admin.0, &subjects).await?;
    debug!(
        log = %logged.id,
        deleted = nicknames.len(),
        "admin deleted users"
    );
    Ok(Json(nicknames))
}

/// The activation write the activate/deactivate pair shares: `is_active`
/// lands on every named account that answers — the acting admin's own row
/// excepted when the request deactivates, the self-protection the ticket
/// pins (Pleroma protects nobody here) — and the answer is the moved
/// accounts as admin entities, in the order the request stated them.
///
/// A deactivation is the flag alone: no token is revoked (the bearer gate
/// answers a deactivated owner's requests with its own 403 for as long as
/// the rows live), no deletion is stamped and nothing federates, exactly
/// as `POST /api/pleroma/disable_account`'s note about the actor document
/// staying resolvable.
///
/// # Errors
///
/// [`AppError::Database`] from the lookups, the writes and the log.
pub(super) async fn write_activation(
    state: &AppState,
    actor: &User,
    nicknames: &[String],
    active: bool,
) -> AppResult<Json<AdminAccounts>> {
    let found = admin::resolve_nicknames(&state.db, nicknames).await?;
    let mut moved = Vec::with_capacity(found.len());
    for account in &found {
        if !active && account.id == actor.id {
            continue;
        }
        if let Some(updated) = admin::set_active(&state.db, account.id, active).await? {
            moved.push(updated);
        }
    }
    let logged = if active {
        moderation_log::activate(&state.db, actor, &moved).await?
    } else {
        moderation_log::deactivate(&state.db, actor, &moved).await?
    };
    debug!(
        log = %logged.id,
        moved = moved.len(),
        active, "admin changed account activations"
    );
    Ok(Json(AdminAccounts::from_users(&moved, &state.config)))
}

/// `PATCH /api/v1/pleroma/admin/users/activate` — Pleroma's `activate`:
/// [`write_activation`] with the flag set.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] from the
/// extractor for a body with no `nicknames`; [`AppError::Database`] from
/// [`write_activation`].
pub(super) async fn activate_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<NicknamesBody>,
) -> AppResult<Json<AdminAccounts>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    write_activation(&state, &admin.0, &body.0.nicknames, true).await
}

/// `PATCH /api/v1/pleroma/admin/users/deactivate` — Pleroma's
/// `deactivate`: [`write_activation`] with the flag cleared, and the
/// caller's own account skipped rather than locked out.
///
/// # Errors
///
/// As [`activate_users`].
pub(super) async fn deactivate_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<NicknamesBody>,
) -> AppResult<Json<AdminAccounts>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    write_activation(&state, &admin.0, &body.0.nicknames, false).await
}

/// `PATCH /api/v1/pleroma/admin/users/{nickname}/toggle_activation` —
/// Pleroma's `toggle_activation`: flip the flag of the one account the
/// segment names (nickname or id, the show's lookup, mirror included),
/// answer its single admin entity and log the move that was made — an
/// activation logged `activate`, a deactivation `deactivate`, Pleroma's
/// own choice of action for the entry. Like the bulk deactivation, it
/// refuses to lock the caller out: toggling the caller's own active account
/// is a 403 and writes nothing (a deactivated caller cannot reach this
/// route, so only the deactivating flip is ever refused).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate, and [`AppError::Forbidden`] with
/// [`SELF_DEACTIVATION`] for the caller's own active account;
/// [`AppError::RouteNotFound`] from
/// [`named_account`] or from the write matching no row;
/// [`AppError::Database`] from the write and the log.
pub(super) async fn toggle_activation(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminAccount>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let found = named_account(&state, &raw).await?;
    if found.id == admin.0.id && found.is_active.get() {
        return Err(AppError::Forbidden(SELF_DEACTIVATION.to_owned()));
    }
    let activate = !found.is_active.get();
    let updated = admin::set_active(&state.db, found.id, activate)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    let subjects = slice::from_ref(&found);
    let logged = if activate {
        moderation_log::activate(&state.db, &admin.0, subjects).await?
    } else {
        moderation_log::deactivate(&state.db, &admin.0, subjects).await?
    };
    debug!(
        log = %logged.id,
        id = %found.id,
        nickname = %found.nickname,
        activated = activate,
        "admin toggled an account's activation"
    );
    Ok(Json(AdminAccount::from_user(&updated, &state.config)))
}

/// `PATCH /api/v1/pleroma/admin/users/approve` — Pleroma's `approve`:
/// every named account that answers is among the answered `users`, in the
/// order the request stated them, and the ones still awaiting staff get
/// their `approval_pending` cleared and the `account_approved` letter —
/// an already-approved account is content to be handed back unchanged,
/// Pleroma's `approve(%User{is_approved: true})` clause, mailed nothing
/// and written nothing. No token is created: approval opens the door, the
/// account still signs in with its own password. One `approve` entry names
/// every resolved subject, moved or not.
///
/// # Errors
///
/// As [`activate_users`].
pub(super) async fn approve_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<NicknamesBody>,
) -> AppResult<Json<AdminAccounts>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let found = admin::resolve_nicknames(&state.db, &body.0.nicknames).await?;
    let mut answered = Vec::with_capacity(found.len());
    for account in found {
        if account.approval_pending.get() {
            answered.push(helpers::approve_account(&state, account).await?);
        } else {
            answered.push(account);
        }
    }
    let logged = moderation_log::approve(&state.db, &admin.0, &answered).await?;
    debug!(
        log = %logged.id,
        resolved = answered.len(),
        "admin approved users"
    );
    Ok(Json(AdminAccounts::from_users(&answered, &state.config)))
}
