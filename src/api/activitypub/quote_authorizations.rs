//! The `QuoteAuthorization` `GET` of the `ActivityPub` router: `GET
//! /quote_authorizations/{uuid}`, the document the `result` of our `Accept`
//! names and the quoter's instance fetches before it shows a quote as
//! accepted (FEP-044f).
//!
//! Negotiation and gates are the object reads': an `Accept` naming no
//! `ActivityPub` media type is the plain 404, and `AUTHORIZED_FETCH`
//! demands the signature before anything resolves. An id that names no
//! grant is the 404; a grant that was revoked, or whose quoted status has
//! since been deleted, is the 410 — the quoter's instance reads that as
//! the quote no longer being authorized.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, Method, Uri},
    response::Response,
};

use crate::{
    error::{AppError, AppResult},
    federation::{authorized_fetch::fetch_allowed, quotes::quote_authorization_json},
    models::{quote_authorization, status, user},
    state::AppState,
    util::id::parse_id,
};

use super::negotiation::{document_response, wants_activity_json, with_vary};

/// `GET /quote_authorizations/{uuid}` — the `QuoteAuthorization` document
/// of one grant, `application/activity+json`.
pub(super) async fn get_quote_authorization(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(raw_id): Path<String>,
) -> Response {
    let outcome = authorization_core(&state, &raw_id, &headers, &uri).await;
    with_vary(outcome)
}

/// The route's answer: the id must parse, the client must speak
/// `ActivityPub`, the fetch gate must pass, and the grant must stand with
/// its quoted status live and its author still there.
async fn authorization_core(
    state: &AppState,
    raw_id: &str,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<Response> {
    let Some(id) = parse_id(raw_id) else {
        return Err(AppError::RouteNotFound);
    };
    if !wants_activity_json(headers) {
        return Err(AppError::RouteNotFound);
    }
    fetch_allowed(state, &Method::GET, uri, headers).await?;
    let grant = quote_authorization::get(&state.db, id)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    let quoted = status::get_by_id_including_deleted(&state.db, grant.quoted_status_id)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    if grant.revoked_at.is_some() || quoted.deleted_at.is_some() {
        return Err(AppError::Gone);
    }
    let author = user::get_by_id(&state.db, quoted.user_id)
        .await?
        .filter(user::User::is_local_active)
        .ok_or(AppError::RouteNotFound)?;
    let document =
        quote_authorization_json(&state.config.base_url, &grant, &author.ap_id, &quoted.uri);
    Ok(document_response(&document))
}
