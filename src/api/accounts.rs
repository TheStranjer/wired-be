//! `GET /api/v1/accounts/{id}` and `GET /api/v1/accounts/lookup`: the
//! read endpoints for one account.
//!
//! Pleroma's `AccountController.show` and `.lookup`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`) are
//! the reference. Both answer the core [`Account`] entity (decision 3:
//! rendered from the row by [`Account::from_user`]), both serve local and
//! remote mirrors alike (decision 7), and both treat an account that is
//! unknown, unresolvable or deactivated as a 404 "Record not found" — the
//! answer Pleroma's `visible_for` check renders through its fallback
//! controller.
//!
//! Authentication is optional, as in Pleroma's
//! `OAuthScopesPlug … fallback: :proceed_unauthenticated`: the routes
//! answer without a token, and a valid token changes nothing yet
//! (relationship fields arrive with a later ticket). The [`MaybeAuthUser`]
//! extractor is still attached to both, so a request presenting an
//! invalid, expired or deactivated token is answered 401 "The access
//! token is invalid" instead of being silently treated as anonymous.
//!
//! `lookup` resolves its `acct` parameter through
//! [`get_by_acct`](crate::models::user::get_by_acct): a stored nickname,
//! case-insensitively, with this instance's own domain stripped for local
//! accounts. Resolving a handle this instance has never seen is left to
//! the `WebFinger` federation ticket.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use serde::Deserialize;

use crate::{
    api::helpers::account_or_404,
    auth::MaybeAuthUser,
    entities::account::Account,
    error::{AppError, AppResult},
    models::user,
    state::AppState,
};

/// The 400 body message when the `acct` query parameter is absent or blank
/// (Mastodon's `ParameterMissing` wording).
const MISSING_ACCT: &str = "Missing parameter: acct";

/// The `/api/v1/accounts` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/accounts/lookup", get(lookup_account))
        .route("/accounts/{id}", get(show_account))
}

/// `GET /api/v1/accounts/{id}` — the account whose id is `id`, local or
/// remote. An id that is not `UUID` text is a 404, not a 400, like every
/// other miss (see [`account_or_404`]).
///
/// The viewer behind an optional token is resolved and ignored for now;
/// relationship fields come with a later ticket.
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed, unknown or deactivated id;
/// [`AppError::Unauthorized`] from the extractor for an invalid token.
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn show_account(
    State(state): State<AppState>,
    MaybeAuthUser(_): MaybeAuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Account>> {
    let found = account_or_404(&state, &id).await?;
    Ok(Json(Account::from_user(&found, &state.config)))
}

/// The query parameters of `GET /api/v1/accounts/lookup`. The absent and
/// blank cases are the documented 400 and 404, which the type cannot
/// express, so the field is optional here (the `webfinger` handler's
/// pattern).
#[derive(Debug, Deserialize)]
struct LookupQuery {
    /// The handle to resolve: `nick` or `nick@domain`.
    acct: Option<String>,
}

/// `GET /api/v1/accounts/lookup?acct=…` — the account a handle names,
/// resolved by [`get_by_acct`](crate::models::user::get_by_acct). The
/// viewer behind an optional token is resolved and ignored for now.
///
/// # Errors
///
/// [`AppError::BadRequest`] when `acct` is absent or blank;
/// [`AppError::NotFound`] when it names no live account (unknown, on a
/// foreign domain not mirrored here, or deactivated);
/// [`AppError::Unauthorized`] from the extractor for an invalid token.
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn lookup_account(
    State(state): State<AppState>,
    MaybeAuthUser(_): MaybeAuthUser,
    Query(query): Query<LookupQuery>,
) -> AppResult<Json<Account>> {
    let Some(acct) = query
        .acct
        .as_deref()
        .map(str::trim)
        .filter(|acct| !acct.is_empty())
    else {
        return Err(AppError::BadRequest(MISSING_ACCT.to_owned()));
    };
    let found = user::get_by_acct(&state.db, &state.config, acct)
        .await?
        .filter(|found| found.is_active.get())
        .ok_or(AppError::NotFound)?;
    Ok(Json(Account::from_user(&found, &state.config)))
}
