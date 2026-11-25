//! The keyword routes of the v2 filter API.
//!
//! `GET`/`POST /api/v2/filters/{id}/keywords` list and add the keywords of
//! one filter; `GET`/`PUT`/`DELETE /api/v2/filters/keywords/{id}` read,
//! change and remove one keyword by its own id. The scopes, the 404 for
//! another account's ids and the `filters_changed` event are the filter
//! routes' (see the parent module).

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::{owned_filter, params::validated_keyword};
use crate::{
    auth::AuthUser,
    entities::filter::v2::FilterKeyword,
    error::{AppError, AppResult},
    extract::{ApiBody, flex},
    models::filter::keyword::{self, KeywordPatch, NewKeyword},
    oauth::scopes::{READ_FILTERS, WRITE_FILTERS},
    services::streaming,
    state::AppState,
    util::id::parse_id,
};

/// The parameters of `POST` and `PUT` on a keyword.
#[derive(Debug, Default, Deserialize)]
pub(super) struct KeywordParams {
    /// The phrase; required on create.
    #[serde(default)]
    keyword: Option<String>,
    /// Whole-word matching; on when absent for a new keyword.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    whole_word: Option<bool>,
}

/// `GET /api/v2/filters/{id}/keywords` — the keywords of one owned filter.
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
) -> AppResult<Json<Vec<FilterKeyword>>> {
    auth.require(READ_FILTERS)?;
    let row = owned_filter(&state, &auth, &id).await?;
    Ok(Json(
        row.keywords.iter().map(FilterKeyword::from_row).collect(),
    ))
}

/// `POST /api/v2/filters/{id}/keywords` — add a keyword to one owned filter.
///
/// # Errors
///
/// As [`list`] with `write:filters`, plus [`AppError::Unprocessable`] for a
/// blank keyword.
pub(super) async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<KeywordParams>,
) -> AppResult<Json<FilterKeyword>> {
    auth.require(WRITE_FILTERS)?;
    let new = NewKeyword {
        keyword: validated_keyword(params.keyword)?,
        whole_word: params.whole_word.unwrap_or(true),
    };
    let row = owned_filter(&state, &auth, &id).await?;
    let mut conn = state.db.acquire().await?;
    let stored = keyword::insert(&mut conn, row.id, &new).await?;
    streaming::filters_changed(&state, auth.0.id);
    Ok(Json(FilterKeyword::from_row(&stored)))
}

/// `GET /api/v2/filters/keywords/{id}` — one keyword of the caller's.
///
/// # Errors
///
/// As [`list`].
pub(super) async fn show(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<FilterKeyword>> {
    auth.require(READ_FILTERS)?;
    let found = owned_keyword(&state, &auth, &id).await?;
    Ok(Json(FilterKeyword::from_row(&found)))
}

/// `PUT /api/v2/filters/keywords/{id}` — change the phrase or the flag of
/// one keyword of the caller's; what the request leaves out stays.
///
/// # Errors
///
/// As [`create`].
pub(super) async fn update(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<KeywordParams>,
) -> AppResult<Json<FilterKeyword>> {
    auth.require(WRITE_FILTERS)?;
    let patch = KeywordPatch {
        keyword: params
            .keyword
            .map(|text| validated_keyword(Some(text)))
            .transpose()?,
        whole_word: params.whole_word,
    };
    let Some(keyword_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    let mut conn = state.db.acquire().await?;
    let Some(changed) = keyword::update_owned(&mut conn, keyword_id, auth.0.id, &patch).await?
    else {
        return Err(AppError::NotFound);
    };
    streaming::filters_changed(&state, auth.0.id);
    Ok(Json(FilterKeyword::from_row(&changed)))
}

/// `DELETE /api/v2/filters/keywords/{id}` — remove one keyword of the
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
    let Some(keyword_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    let mut conn = state.db.acquire().await?;
    if !keyword::delete_owned(&mut conn, keyword_id, auth.0.id).await? {
        return Err(AppError::NotFound);
    }
    streaming::filters_changed(&state, auth.0.id);
    Ok(Json(Value::Object(Map::new())))
}

/// The keyword `id` of the caller's.
///
/// # Errors
///
/// [`AppError::NotFound`] when the id is malformed, unknown or foreign.
async fn owned_keyword(state: &AppState, auth: &AuthUser, id: &str) -> AppResult<keyword::Keyword> {
    let Some(keyword_id) = parse_id(id) else {
        return Err(AppError::NotFound);
    };
    keyword::get_owned(&state.db, keyword_id, auth.0.id)
        .await?
        .ok_or(AppError::NotFound)
}
