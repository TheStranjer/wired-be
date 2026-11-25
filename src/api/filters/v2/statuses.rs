//! The status routes of the v2 filter API.
//!
//! `GET`/`POST /api/v2/filters/{id}/statuses` list and add the single
//! statuses one filter matches whole (the `status_id` parameter);
//! `GET`/`DELETE /api/v2/filters/statuses/{id}` read and remove one entry by
//! its own id. A `status_id` that names no status, or a status the filter
//! already holds, is a 422. The scopes, the 404 for another account's ids
//! and the `filters_changed` event are the filter routes' (see the parent
//! module).

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::owned_filter;
use crate::{
    auth::AuthUser,
    entities::filter::v2::FilterStatus as StatusEntity,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::filter::status,
    oauth::scopes::{READ_FILTERS, WRITE_FILTERS},
    services::streaming,
    state::AppState,
    util::id::parse_id,
};

/// 422 body for a `status_id` that is absent or names no status.
const STATUS_MISSING: &str = "Validation failed: Status must exist";
/// 422 body for a status the filter already holds.
const STATUS_TAKEN: &str = "Validation failed: Status has already been taken";

/// The parameters of `POST /api/v2/filters/{id}/statuses`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct StatusParams {
    /// The status to add.
    #[serde(default)]
    status_id: Option<String>,
}

/// `GET /api/v2/filters/{id}/statuses` — the status entries of one owned
/// filter.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:filters` check; [`AppError::NotFound`] for an id that is
/// malformed, unknown or foreign.
pub(super) async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<StatusEntity>>> {
    auth.require(READ_FILTERS)?;
    let row = owned_filter(&state, &auth, &id).await?;
    Ok(Json(
        row.statuses.iter().map(StatusEntity::from_row).collect(),
    ))
}

/// `POST /api/v2/filters/{id}/statuses` — add a status to one owned filter.
///
/// # Errors
///
/// As [`list`] with `write:filters`, plus [`AppError::Unprocessable`] for a
/// `status_id` that names no status or one the filter already holds.
pub(super) async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<StatusParams>,
) -> AppResult<Json<StatusEntity>> {
    auth.require(WRITE_FILTERS)?;
    let row = owned_filter(&state, &auth, &id).await?;
    let missing = || AppError::Unprocessable(STATUS_MISSING.to_owned());
    let status_id = params
        .status_id
        .as_deref()
        .and_then(parse_id)
        .ok_or_else(missing)?;
    if !status::status_exists(&state.db, status_id).await? {
        return Err(missing());
    }
    let Some(entry) = status::insert(&state.db, row.id, status_id).await? else {
        return Err(AppError::Unprocessable(STATUS_TAKEN.to_owned()));
    };
    streaming::filters_changed(&state, auth.0.id);
    Ok(Json(StatusEntity::from_row(&entry)))
}

/// `GET /api/v2/filters/statuses/{id}` — one status entry of the caller's.
///
/// # Errors
///
/// As [`list`].
pub(super) async fn show(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<StatusEntity>> {
    auth.require(READ_FILTERS)?;
    let Some(entry_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    let Some(entry) = status::get_owned(&state.db, entry_id, auth.0.id).await? else {
        return Err(AppError::NotFound);
    };
    Ok(Json(StatusEntity::from_row(&entry)))
}

/// `DELETE /api/v2/filters/statuses/{id}` — remove one status entry of the
/// caller's and answer `{}`.
///
/// # Errors
///
/// As [`list`] with `write:filters`.
pub(super) async fn delete(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_FILTERS)?;
    let Some(entry_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    if !status::delete_owned(&state.db, entry_id, auth.0.id).await? {
        return Err(AppError::NotFound);
    }
    streaming::filters_changed(&state, auth.0.id);
    Ok(Json(Value::Object(Map::new())))
}
