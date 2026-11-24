//! The single-account reads and the relationship batch:
//! `GET /api/v1/accounts/{id}`, `…/lookup` and `…/relationships`, with
//! the id filtering [`known_ids`] performs for the batch.
//!
//! Authentication on `show` and `lookup` is optional, as in Pleroma's
//! `OAuthScopesPlug … fallback: :proceed_unauthenticated`: the routes
//! answer without a token. A token is not ignored, though: it names the
//! [`AccountViewer`] the entity is
//! rendered for, so the account's own user sees what the row hides from
//! strangers (hidden follow counts, hidden role flags, and the owner-only
//! keys of the `pleroma` block) while anyone else sees the restricted view.
//! An authenticated viewer looking at *another* account also gets their
//! [`Relationship`] embedded in `pleroma.relationship`, preloaded per
//! request; anonymous viewers and self-views keep Pleroma's empty object.
//! The [`MaybeAuthUser`] extractor is attached to both routes, so a request
//! presenting an invalid, expired or deactivated token is answered 401 "The
//! access token is invalid" instead of being silently treated as anonymous.
//!
//! `lookup` resolves its `acct` parameter — or its `q` alias — through
//! [`get_by_acct`](crate::models::user::get_by_acct): a stored nickname,
//! case-insensitively, with this instance's own domain stripped for local
//! accounts. With `resolve=true` a handle (or actor URL) this instance has
//! never seen is resolved through `WebFinger` and fetched — the federation
//! arm `resolve_actor_or_fetch` shares with `POST /api/v1/follows`, run
//! only for an authenticated caller because resolution is a federation
//! action on somebody's behalf (the resolving search's own rule, see
//! [`crate::api::search`]).
//!
//! `relationships` requires auth: [`AuthUser`] plus the `read:follows`
//! scope, exactly Pleroma's plug pipeline for that action
//! (`%{scopes: ["read:follows"]}`). It reads Mastodon's `id[]` array
//! parameter — a single `id=` is accepted too, Pleroma's `List.wrap(id)` —
//! and answers one [`Relationship`] per *known* id in the order requested:
//! values that are not `UUID` text and ids that name no stored user are
//! skipped (as Pleroma's `User.get_all_by_ids` skips them), and an absent,
//! empty or wholly-unknown `id` is the empty array, never an error
//! (Pleroma returns `[]` when the parameter is missing). The flags come
//! from [`Relationship::load_many`], one batched query set per request.
//!

use std::collections::HashSet;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::Uri,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    api::helpers::{is_true, requested_ids, visible_account_or_404, visible_to},
    auth::{AuthUser, MaybeAuthUser},
    entities::{account::Account, relationship::Relationship},
    error::{AppError, AppResult},
    models::user,
    oauth::scopes::READ_FOLLOWS,
    state::AppState,
};

use super::{follow::resolve_actor_or_fetch, follow_lists::require_profile_read, render_account};

/// The 400 body message when the `acct` query parameter is absent or blank
/// (Mastodon's `ParameterMissing` wording).
const MISSING_ACCT: &str = "Missing parameter: acct";

/// `GET /api/v1/accounts/{id}` — the account whose id is `id`, local or
/// remote. An id that is not `UUID` text is a 404, not a 400, like every
/// other miss (see [`visible_account_or_404`]); so is an account the viewer
/// may not see (one awaiting approval or email confirmation, see
/// [`visible_to`]).
///
/// The viewer behind an optional token is the account the entity is
/// rendered for; without one the anonymous view is answered. A viewer who
/// is looking at someone else gets their relationship to the account
/// embedded (see [`render_account`]).
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed, unknown, deactivated or (to this
/// viewer) invisible id;
/// [`AppError::Forbidden`] ("This API requires an authenticated user") for
/// an anonymous request while `RESTRICT_UNAUTHENTICATED` closes the
/// account's profile half (see [`require_profile_read`]; Pleroma's `show`
/// answers that case 404, this API keeps the 403 `statuses` gives);
/// [`AppError::Unauthorized`] from the extractor for an invalid token.
/// Any database failure arrives through `?` as [`AppError::Database`].
pub(super) async fn show_account(
    State(state): State<AppState>,
    MaybeAuthUser(viewer, _): MaybeAuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Account>> {
    let found = visible_account_or_404(&state, &id, viewer.as_ref()).await?;
    require_profile_read(&state.config, viewer.as_ref(), &found)?;
    Ok(Json(render_account(&state, viewer.as_ref(), &found).await?))
}

/// The query parameters of `GET /api/v1/accounts/lookup`. The absent and
/// blank cases are the documented 400 and 404, which the type cannot
/// express, so the fields are optional here (the `webfinger` handler's
/// pattern).
#[derive(Debug, Deserialize)]
pub(super) struct LookupQuery {
    /// The handle to resolve: `nick` or `nick@domain`.
    acct: Option<String>,
    /// The same handle under Mastodon's newer spelling: read only when
    /// `acct` is absent or blank.
    q: Option<String>,
    /// `resolve`: the federation arm's switch — an unseen handle or actor
    /// URL is `WebFinger`ed and fetched instead of answered 404. A string
    /// because a query parameter is one, read by [`is_true`] like the
    /// search's flags.
    ///
    /// [`is_true`]: crate::api::helpers::is_true
    resolve: Option<String>,
}

impl LookupQuery {
    /// The account the request asks about: `acct` when it carries text,
    /// else the `q` alias — a blank is no answer, not a shadowing one.
    /// Neither standing is the documented 400.
    fn asked(&self) -> Option<&str> {
        [self.acct.as_deref(), self.q.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|asked| !asked.is_empty())
    }

    /// Whether the request asks for federation resolution: `resolve=true`.
    fn resolves(&self) -> bool {
        self.resolve.as_deref().is_some_and(is_true)
    }
}

/// `GET /api/v1/accounts/lookup?acct=…` (or `?q=…`) — the account a handle
/// names, resolved by [`get_by_acct`](crate::models::user::get_by_acct).
/// The viewer behind an optional token is the account the entity is
/// rendered for, as in `show`.
///
/// With `resolve=true` a stored miss is not yet an answer: an
/// authenticated caller gets the [`resolve_actor_or_fetch`] round trip —
/// `WebFinger` and the actor fetch for a handle, the dereference for an
/// actor URL, the mirror written on the way out. An anonymous `resolve`
/// request keeps the stored-only answer, because resolution is a
/// federation action somebody has to be acting for — the rule the
/// resolving search runs by.
///
/// # Errors
///
/// [`AppError::BadRequest`] when neither `acct` nor `q` carries a handle;
/// [`AppError::NotFound`] when it names no live account, stored or
/// resolvable (unknown, on a domain whose lookup fails, or deactivated);
/// [`AppError::Forbidden`] for an anonymous request while
/// `RESTRICT_UNAUTHENTICATED` closes the found account's profile half, as
/// in [`show_account`];
/// [`AppError::Unauthorized`] from the extractor for an invalid token.
/// Any database failure arrives through `?` as [`AppError::Database`].
pub(super) async fn lookup_account(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Query(query): Query<LookupQuery>,
) -> AppResult<Json<Account>> {
    let Some(asked) = query.asked().map(str::to_owned) else {
        return Err(AppError::BadRequest(MISSING_ACCT.to_owned()));
    };
    let viewer = auth.0.as_ref();
    let stored = user::get_by_acct(&state.db, &state.config, &asked)
        .await?
        .filter(|found| visible_to(found, viewer));
    let found = match stored {
        Some(found) => found,
        None if query.resolves() && viewer.is_some() => {
            resolve_actor_or_fetch(&state, &asked, viewer).await?
        }
        None => return Err(AppError::NotFound),
    };
    require_profile_read(&state.config, viewer, &found)?;
    Ok(Json(render_account(&state, viewer, &found).await?))
}

/// `GET /api/v1/accounts/relationships?id[]=a&id[]=b` — the caller's
/// relationship to each requested account, as
/// [`Relationship::load_many`] computes it, in the order requested.
///
/// The `id` values are read by [`requested_ids`]; ones that are not `UUID`
/// text and ones that name no stored user are dropped (Pleroma's
/// `User.get_all_by_ids` simply finds nothing for them), so the response
/// holds only the known ids in requested order. No usable `id` at all is
/// the empty array, Pleroma's explicit `json(conn, [])` for a missing
/// parameter, not a `Missing parameter` error.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user
/// token; [`AppError::Forbidden`] when the token does not grant
/// `read:follows`. Any database failure arrives through `?` as
/// [`AppError::Database`].
pub(super) async fn relationships(
    State(state): State<AppState>,
    auth: AuthUser,
    uri: Uri,
) -> AppResult<Json<Vec<Relationship>>> {
    auth.require(READ_FOLLOWS)?;
    let requested = requested_ids(uri.query());
    if requested.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let targets = known_ids(&state, &requested).await?;
    Ok(Json(
        Relationship::load_many(&state.db, auth.0.id, &targets).await?,
    ))
}

/// The subset of `requested` that names a stored user, kept in the
/// requested order ([`user::get_many`] answers in id order, so membership
/// is decided from its rows). One query per request whatever the batch
/// size.
async fn known_ids(state: &AppState, requested: &[Uuid]) -> AppResult<Vec<Uuid>> {
    let known: HashSet<Uuid> = user::get_many(&state.db, requested)
        .await?
        .into_iter()
        .map(|found| found.id)
        .collect();
    Ok(requested
        .iter()
        .copied()
        .filter(|id| known.contains(id))
        .collect())
}
