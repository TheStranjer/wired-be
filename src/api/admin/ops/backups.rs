//! `POST /api/v1/pleroma/admin/backups`: queueing a backup for a named
//! account.

use axum::{Json, extract::State};
use serde::Deserialize;

use tracing::debug;

use super::{AREA_BACKUPS, CLASS_WRITE, MISSING_NICKNAME, frontends::first_of};
use crate::{
    auth::admin::AdminUser,
    entities::backup::Backup,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{backup, moderation_log, user::get_by_nickname},
    state::AppState,
};

/// The body of `POST /backups`: the account to request the export for.
#[derive(Debug, Deserialize)]
pub(super) struct BackupBody {
    /// The target account's nickname; required.
    #[serde(default)]
    nickname: Option<String>,
}

/// `POST /api/v1/pleroma/admin/backups` — Pleroma's `create_backup`:
/// record a backup for the named account and queue its job through the
/// user endpoint's own service, skipping the `BACKUP_LIMIT_DAYS` gate
/// that stands between one account and its own next export, and answer
/// the account's backup list in the user endpoint's shape.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] with
/// [`MISSING_NICKNAME`]; [`AppError::NotFound`] for a nickname no account
/// carries; [`AppError::Database`] from the record, the queue write and
/// the list read.
pub(super) async fn create_backup(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiBody(body): ApiBody<BackupBody>,
) -> AppResult<Json<Vec<Backup>>> {
    admin.require(CLASS_WRITE, AREA_BACKUPS)?;
    let Some(nickname) = first_of(body.nickname.as_deref(), None) else {
        return Err(AppError::BadRequest(MISSING_NICKNAME.to_owned()));
    };
    let target = get_by_nickname(&state.db, nickname)
        .await?
        .ok_or(AppError::NotFound)?;
    let row = backup::create(&state.db, &target).await?;
    crate::api::backups::enqueue_backup_job(&state.db, row.id).await?;
    let logged = moderation_log::create_backup(&state.db, &admin.0, &target).await?;
    debug!(
        log = %logged.id,
        backup = %row.id,
        user = %target.id,
        admin = %admin.0.id,
        "admin backup requested"
    );
    let rows = backup::list_for_user(&state.db, target.id).await?;
    Ok(Json(crate::api::backups::render_entities(
        &state.uploader,
        &rows,
    )))
}
