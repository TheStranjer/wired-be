//! The scrobble documents of the `ActivityPub` router: the `Audio` object
//! at `GET /objects/{uuid}` and the `Listen` at `GET /activities/{uuid}`
//! for an id no status or relation row carries.
//!
//! Only what [`federation_out`](crate::services::federation_out) delivers
//! is served: a scrobble this instance minted an `ap_id` for, listened to
//! by a live local account, at the `public` or `unlisted` visibility. A
//! `private` listen travels by delivery alone and a `direct` one addresses
//! nobody, so both are the same plain 404 as an unknown id.

use axum::response::Response;
use uuid::Uuid;

use super::negotiation::document_response;
use crate::{
    error::{AppError, AppResult},
    federation::activities::{audio_json, listen_json},
    models::{
        scrobble::{self, Scrobble},
        status::Visibility,
        user::{User, get_by_id},
    },
    state::AppState,
};

/// The scrobble `id` names and its listener, when this instance serves it:
/// delivered by us (it has an `ap_id`), openly addressed, and by a live
/// local account; `None` otherwise.
async fn served(state: &AppState, id: Uuid) -> AppResult<Option<(Scrobble, User)>> {
    let Some(row) = scrobble::get_by_id(&state.db, id).await? else {
        return Ok(None);
    };
    if row.ap_id.is_none() || !matches!(row.visibility, Visibility::Public | Visibility::Unlisted) {
        return Ok(None);
    }
    let listener = get_by_id(&state.db, row.user_id)
        .await?
        .filter(User::is_local_active);
    Ok(listener.map(|found| (row, found)))
}

/// The `Audio` document of scrobble `id`, as the 200 `ActivityPub` answer.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] when [`served`] has nothing under `id`; any
/// database failure arrives as [`AppError::Database`].
pub(super) async fn audio_response(state: &AppState, id: Uuid) -> AppResult<Response> {
    let Some((row, listener)) = served(state, id).await? else {
        return Err(AppError::RouteNotFound);
    };
    let document = audio_json(&row, &listener, &state.config);
    Ok(document_response(&document))
}

/// The `Listen` document of scrobble `id`, as the 200 `ActivityPub` answer.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] when [`served`] has nothing under `id`; any
/// database failure arrives as [`AppError::Database`].
pub(super) async fn listen_response(state: &AppState, id: Uuid) -> AppResult<Response> {
    let Some((row, listener)) = served(state, id).await? else {
        return Err(AppError::RouteNotFound);
    };
    let document = listen_json(&row, &listener, &state.config);
    Ok(document_response(&document))
}
