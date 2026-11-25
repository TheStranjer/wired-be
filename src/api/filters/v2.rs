//! The Mastodon v2 filter API: grouped keywords, actions, contexts, expiry
//! and single-status filters, over the same storage the v1 routes use.
//!
//! The reference is Mastodon's `Api::V2::FiltersController` and its
//! `Filters::KeywordsController` / `Filters::StatusesController`:
//!
//! - `GET /api/v2/filters` lists the caller's filters — expired ones too,
//!   as Mastodon does — newest first; `POST` creates one (a title, its
//!   contexts, a `filter_action`, `expires_in` and `keywords_attributes`);
//!   `GET`, `PUT` and `DELETE /api/v2/filters/{id}` read, change and remove
//!   one. A `PUT` changes only what it sends.
//! - `/api/v2/filters/{id}/keywords` lists and adds keywords, and
//!   `/api/v2/filters/keywords/{id}` reads, changes and removes one.
//! - `/api/v2/filters/{id}/statuses` lists and adds single statuses, and
//!   `/api/v2/filters/statuses/{id}` reads and removes one.
//!
//! Every route needs a user token with `read:filters` (reads) or
//! `write:filters` (writes). An id that is malformed, unknown or another
//! account's is the same 404. Each successful write publishes Mastodon's
//! `filters_changed` on the caller's own user stream.

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde_json::{Map, Value};

use self::params::FilterParams;
use crate::{
    auth::AuthUser,
    entities::filter::v2::Filter as FilterEntity,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::filter,
    oauth::scopes::{READ_FILTERS, WRITE_FILTERS},
    services::streaming,
    state::AppState,
    util::id::parse_id,
};

mod keywords;
mod params;
mod statuses;

/// The v2 filter routes, merged into [`crate::api::v2`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/filters", get(list_filters).post(create_filter))
        .route(
            "/filters/{id}",
            get(show_filter).put(update_filter).delete(delete_filter),
        )
        .route(
            "/filters/{id}/keywords",
            get(keywords::list).post(keywords::create),
        )
        .route(
            "/filters/keywords/{id}",
            get(keywords::show)
                .put(keywords::update)
                .delete(keywords::delete),
        )
        .route(
            "/filters/{id}/statuses",
            get(statuses::list).post(statuses::create),
        )
        .route(
            "/filters/statuses/{id}",
            get(statuses::show).delete(statuses::delete),
        )
}

/// `GET /api/v2/filters` — every filter of the caller, newest first.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:filters` check; a database failure as [`AppError::Database`].
async fn list_filters(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Vec<FilterEntity>>> {
    auth.require(READ_FILTERS)?;
    let rows = filter::list_all(&state.db, auth.0.id).await?;
    Ok(Json(rows.iter().map(FilterEntity::from_row).collect()))
}

/// `GET /api/v2/filters/{id}` — one filter of the caller, expired or not.
///
/// # Errors
///
/// As [`list_filters`], plus [`AppError::NotFound`] for an id that is
/// malformed, unknown or foreign.
async fn show_filter(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<FilterEntity>> {
    auth.require(READ_FILTERS)?;
    let row = owned_filter(&state, &auth, &id).await?;
    Ok(Json(FilterEntity::from_row(&row)))
}

/// `POST /api/v2/filters` — create a filter with its keywords and answer it.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:filters` check; [`AppError::Unprocessable`] for a blank title
/// or keyword, a bad context list or an unknown action.
async fn create_filter(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<FilterParams>,
) -> AppResult<Json<FilterEntity>> {
    auth.require(WRITE_FILTERS)?;
    let (new, keywords) = params.into_new()?;
    let row = filter::create(&state.db, auth.0.id, &new, &keywords).await?;
    streaming::filters_changed(&state, auth.0.id);
    Ok(Json(FilterEntity::from_row(&row)))
}

/// `PUT /api/v2/filters/{id}` — change what the request sends of one owned
/// filter, apply its `keywords_attributes`, and answer the result.
///
/// # Errors
///
/// As [`create_filter`], plus [`AppError::NotFound`] for an id that is
/// malformed, unknown or foreign, or a keyword id the filter does not hold.
async fn update_filter(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<FilterParams>,
) -> AppResult<Json<FilterEntity>> {
    auth.require(WRITE_FILTERS)?;
    let (patch, changes) = params.into_patch()?;
    let Some(filter_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    let Some(row) = filter::update(&state.db, filter_id, auth.0.id, &patch, &changes).await? else {
        return Err(AppError::NotFound);
    };
    streaming::filters_changed(&state, auth.0.id);
    Ok(Json(FilterEntity::from_row(&row)))
}

/// `DELETE /api/v2/filters/{id}` — remove one owned filter with its keywords
/// and statuses, and answer `{}`.
///
/// # Errors
///
/// As [`update_filter`], without the parameter errors.
async fn delete_filter(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_FILTERS)?;
    let Some(filter_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    if !filter::delete(&state.db, filter_id, auth.0.id).await? {
        return Err(AppError::NotFound);
    }
    streaming::filters_changed(&state, auth.0.id);
    Ok(Json(Value::Object(Map::new())))
}

/// The filter `id` of the caller.
///
/// # Errors
///
/// [`AppError::NotFound`] when the id is malformed, unknown or foreign; a
/// database failure as [`AppError::Database`].
async fn owned_filter(state: &AppState, auth: &AuthUser, id: &str) -> AppResult<filter::Filter> {
    let Some(filter_id) = parse_id(id) else {
        return Err(AppError::NotFound);
    };
    filter::get_owned(&state.db, filter_id, auth.0.id)
        .await?
        .ok_or(AppError::NotFound)
}
