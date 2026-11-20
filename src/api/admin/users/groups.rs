//! The permission-group reads and writes, single and bulk, and the
//! suggestion pair.

use std::slice;

use axum::{
    Json,
    extract::{Path, State},
};
use serde_json::Value;
use tracing::debug;

use super::maintenance::NicknamesBody;
use super::tags::{group_of, local_only, named_account, write_role, write_suggestions};
use super::{AREA_ACCOUNTS, CLASS_READ, CLASS_WRITE, REMOTE_ROLE, SELF_REVOKE};
use crate::{
    auth::admin::AdminUser,
    entities::admin_account::{AdminAccounts, AdminPermissionGroup},
    error::{AppError, AppResult},
    extract::ApiParams,
    models::user::admin,
    state::AppState,
};

/// `GET /api/v1/pleroma/admin/users/{nickname}/permission_group` — Pleroma's
/// `right_get`: the account's two staff flags, `is_moderator` and `is_admin`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::RouteNotFound`] from
/// [`named_account`]; [`AppError::Database`] from the read.
pub(super) async fn show_permission_group(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminPermissionGroup>> {
    admin.require(CLASS_READ, AREA_ACCOUNTS)?;
    let found = named_account(&state, &raw).await?;
    debug!(
        id = %found.id,
        nickname = %found.nickname,
        "admin permission group read"
    );
    Ok(Json(AdminPermissionGroup::from_user(&found)))
}

/// `GET /api/v1/pleroma/admin/users/{nickname}/permission_group/{group}` —
/// the very same object: Pleroma routes both spellings onto `right_get`,
/// which never reads the group. The segment is validated anyway, as the
/// ticket pins its 404.
///
/// # Errors
///
/// As [`show_permission_group`], plus [`group_of`]'s 404.
pub(super) async fn show_permission_group_of(
    State(state): State<AppState>,
    admin: AdminUser,
    Path((raw, group)): Path<(String, String)>,
) -> AppResult<Json<AdminPermissionGroup>> {
    admin.require(CLASS_READ, AREA_ACCOUNTS)?;
    let role = group_of(&group)?;
    let found = named_account(&state, &raw).await?;
    debug!(
        id = %found.id,
        group = role.as_str(),
        "admin permission group read"
    );
    Ok(Json(AdminPermissionGroup::from_user(&found)))
}

/// `POST /api/v1/pleroma/admin/users/{nickname}/permission_group/{group}` —
/// Pleroma's `right_add`: the account joins the group, and the answer names
/// the one column that moved.
///
/// The write is the ticket's, and narrower than Pleroma's: an account this
/// instance does not own is the 400 [`REMOTE_ROLE`], because a staff role
/// here is a local row's flag and nothing else. The group is checked first,
/// so a bogus group is [`group_of`]'s 404 whatever the handle says — where
/// Pleroma's guard clause fails on the group before it ever looks the
/// account up.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::NotFoundMessage`] for a group off
/// the two, [`AppError::RouteNotFound`] for an account nobody answers to;
/// [`AppError::BadRequest`] for a mirror; [`AppError::Database`] from the
/// read, the write and the log.
pub(super) async fn grant_role(
    State(state): State<AppState>,
    admin: AdminUser,
    Path((raw, group)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let role = group_of(&group)?;
    let found = named_account(&state, &raw).await?;
    if !found.local.get() {
        return Err(AppError::BadRequest(REMOTE_ROLE.to_owned()));
    }
    write_role(&state, &admin.0, slice::from_ref(&found), role, true).await
}

/// `DELETE /api/v1/pleroma/admin/users/{nickname}/permission_group/{group}`
/// — Pleroma's `right_delete`: the account leaves the group.
///
/// The account's own row comes first among the refusals, as it does in
/// Pleroma — and, like its `right_delete` clause that matches the acting
/// admin's nickname without reading the group, for either group: an admin
/// who could revoke their own `moderator` flag could still revoke
/// themselves out of staff. A mirror is the same 400 [`grant_role`] answers
/// it, and the bulk twin excludes the caller rather than refusing.
///
/// # Errors
///
/// As [`grant_role`], with [`AppError::Forbidden`] and [`SELF_REVOKE`] for
/// the admin who names their own account.
pub(super) async fn revoke_role(
    State(state): State<AppState>,
    admin: AdminUser,
    Path((raw, group)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let role = group_of(&group)?;
    let found = named_account(&state, &raw).await?;
    if found.id == admin.0.id {
        return Err(AppError::Forbidden(SELF_REVOKE.to_owned()));
    }
    if !found.local.get() {
        return Err(AppError::BadRequest(REMOTE_ROLE.to_owned()));
    }
    write_role(&state, &admin.0, slice::from_ref(&found), role, false).await
}

/// `POST /api/v1/pleroma/admin/users/permission_group/{group}` — Pleroma's
/// `right_add_multiple`: every named LOCAL account joins the group, unknown
/// handles and mirrors skipped inside the same success, one `grant` entry
/// naming who moved, and the same one-column answer as the singular write.
///
/// # Errors
///
/// As [`grant_role`], plus the extractor's 422 for a request with no
/// `nicknames`.
pub(super) async fn grant_role_bulk(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(group): Path<String>,
    params: ApiParams<NicknamesBody>,
) -> AppResult<Json<Value>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let role = group_of(&group)?;
    let found = admin::resolve_nicknames(&state.db, &params.0.nicknames).await?;
    write_role(&state, &admin.0, &local_only(found), role, true).await
}

/// `DELETE /api/v1/pleroma/admin/users/permission_group/{group}` — Pleroma's
/// `right_delete_multiple`, and the divergence the ticket pins: where
/// Pleroma refuses the whole request with its `admin/moderator` 403 once
/// the acting admin's own handle is on the list, this revokes from
/// everybody ELSE and leaves the caller's own group untouched. The answer is
/// the same one-column map, and the `revoke` entry names the accounts that
/// moved — the caller's row not among them.
///
/// # Errors
///
/// As [`grant_role_bulk`].
pub(super) async fn revoke_role_bulk(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(group): Path<String>,
    params: ApiParams<NicknamesBody>,
) -> AppResult<Json<Value>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let role = group_of(&group)?;
    let found = admin::resolve_nicknames(&state.db, &params.0.nicknames).await?;
    let mut subjects = local_only(found);
    subjects.retain(|account| account.id != admin.0.id);
    write_role(&state, &admin.0, &subjects, role, false).await
}

/// `PATCH /api/v1/pleroma/admin/users/suggest` — Pleroma's
/// `UserController.suggest`: the named accounts join the pool the
/// `GET /api/v{1,2}/suggestions` listings answer from, and every one of
/// them comes back as an admin entity under `users`.
///
/// Pleroma flags whatever the handle names, mirror included; a mirror keeps
/// its flag out of those listings because the pool itself is local accounts
/// only (see [`crate::models::suggestion`]), not because this write refused
/// it.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] from the
/// extractor for a body with no `nicknames`; [`AppError::Database`] from the
/// lookups, the writes and the log.
pub(super) async fn suggest_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiParams<NicknamesBody>,
) -> AppResult<Json<AdminAccounts>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    write_suggestions(&state, &admin.0, &body.0.nicknames, true).await
}

/// `PATCH /api/v1/pleroma/admin/users/unsuggest` — Pleroma's
/// `unsuggest`: [`suggest_users`] with the flag cleared and the entry
/// logged as a `remove_suggestion`.
///
/// # Errors
///
/// As [`suggest_users`].
pub(super) async fn unsuggest_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiParams<NicknamesBody>,
) -> AppResult<Json<AdminAccounts>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    write_suggestions(&state, &admin.0, &body.0.nicknames, false).await
}

#[cfg(test)]
mod tests {
    use super::super::NO_SUCH_GROUP;
    use super::*;

    /// The bulk write's subject filter is exercised end to end (a mirror in
    /// a nickname list is skipped); the group segment is the pure part:
    /// Pleroma's two names, and no other text.
    #[test]
    fn only_the_two_group_names_are_a_group() {
        assert_eq!(group_of("admin").ok(), Some(admin::Role::Admin));
        assert_eq!(group_of("moderator").ok(), Some(admin::Role::Moderator));
        for text in ["Admin", "owner", "", "is_admin"] {
            let err = group_of(text).expect_err("no such group name");
            assert_eq!(err.to_string(), NO_SUCH_GROUP, "Pleroma's wording");
        }
    }
}
