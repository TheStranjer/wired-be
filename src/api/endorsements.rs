//! The endorsement endpoints: pinning an account's profile to your own
//! (Mastodon's "featured profiles") and reading the pinned lists back.
//!
//! Pleroma's `AccountController` `endorse`, `unendorse`,
//! `.own_endorsements` and `.endorsements`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`,
//! routed for `/pin`/`/unpin` and `/endorse`/`/unendorse` alike,
//! `lib/pleroma/web/router.ex`) over `User.endorse` / `User.unendorse`
//! (`lib/pleroma/user.ex`) are the reference; the storage is
//! [`crate::models::endorsement`] and the fact is this server's
//! `endorsements` table.
//!
//! `POST /api/v1/accounts/{id}/endorse` and its alias `…/pin` require a
//! user token ([`AuthUser`]) carrying `write:accounts`, exactly Pleroma's
//! scopes plug for the action. An id that is malformed, unknown or
//! deactivated is the shared 404 ([`account_or_404`]). The write then
//! demands what Pleroma's `User.endorse` demands: an *accepted* follow of
//! the target — [`is_following`](crate::models::follow::is_following) —
//! and a viewer who does not follow (yet, or at all; a pending request is
//! not a follow) is answered 422 with this endpoint's ticket-pinned
//! wording. Pinning yourself is therefore impossible, the same refusal.
//! An accepted pair stores the endorsement ([`endorse`] is idempotent on
//! the pair, so pinning twice is one row and one answer) and the caller's
//! [`Relationship`] to the account — with `endorsed: true` read back off
//! the stored row — is the answer.
//!
//! `POST /api/v1/accounts/{id}/unendorse` and its alias `…/unpin` run the
//! same auth and 404 pipeline and withdraw the endorsement. Unlike the
//! write side there is no follow to have: [`unendorse`] deletes whatever
//! pair it is given (Pleroma's `User.unendorse` errors on a missing
//! relationship; this endpoint's ticket answers the unchanged
//! [`Relationship`] instead, so clearing twice is 200 twice).
//!
//! `GET /api/v1/endorsements` is Pleroma's `.own_endorsements`: [`AuthUser`]
//! plus `read:accounts`, [`PageParams`] keyset pagination (default limit
//! 20, at most [`MAX_LIMIT`](crate::pagination::MAX_LIMIT)) over the
//! *endorsement row id* — the [`Link`](crate::pagination::link_header)
//! cursors carry [`EndorsementPageEntry::endorsement_id`], newest pin
//! first — and the [`Account`] entities of the pinned accounts, each
//! embedding the caller's [`Relationship`] to it.
//!
//! `GET /api/v1/accounts/{id}/endorsements` and its `/api/v1/pleroma`
//! counterpart are Pleroma's `.endorsements`: authentication is optional
//! ([`MaybeAuthUser`], with `read:accounts` enforced on a presented user
//! token through [`require_user_scope`](MaybeAuthUser::require_user_scope))
//! and the page shape is the own listing's. Privacy per this ticket: an
//! account that hides its follows ([`hide_follows`](crate::models::user::User::hide_follows),
//! the same flag that hides `…/following`) answers the empty array — 200,
//! no `Link` header — to everyone but itself, endorsements being pinned
//! follows; the count-hiding flag alone never hides the list. The account
//! miss is the shared 404.
//!
//! [`endorse`]: crate::models::endorsement::endorse
//! [`unendorse`]: crate::models::endorsement::unendorse
//! [`EndorsementPageEntry::endorsement_id`]: crate::models::endorsement::EndorsementPageEntry::endorsement_id
//! [`hide_follows`]: crate::models::user::User::hide_follows

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::account_or_404,
    auth::{AuthUser, MaybeAuthUser},
    entities::{
        account::{Account, AccountViewer},
        relationship::Relationship,
    },
    error::{AppError, AppResult},
    models::{endorsement, follow},
    oauth::scopes::{READ_ACCOUNTS, WRITE_ACCOUNTS},
    pagination::{NO_EXTRA_QUERY, PageParams, keyset_link, set_link_header},
    state::AppState,
};

/// The 422 body both pin routes answer when the caller has no accepted
/// follow of the target: this endpoint's ticket pins the wording (in place
/// of Pleroma's nickname-carrying refusal).
const NOT_FOLLOWING: &str = "You must follow this user to endorse them";

/// The `/api/v1` endorsements router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/endorsements", get(own_endorsements))
        .route("/accounts/{id}/endorse", post(endorse_account))
        .route("/accounts/{id}/pin", post(endorse_account))
        .route("/accounts/{id}/unendorse", post(unendorse_account))
        .route("/accounts/{id}/unpin", post(unendorse_account))
        .route("/accounts/{id}/endorsements", get(list_endorsements))
}

/// The `/api/v1/pleroma` side of the endorsements router, merged into
/// [`crate::api::v1`].
///
/// Pleroma serves the other account's list under both prefixes
/// (`lib/pleroma/web/router.ex` routes `:endorsements` in the mastodon
/// and the pleroma scopes alike), so this one route is the counterpart
/// of the mastodon path's listing.
pub fn pleroma_router() -> Router<AppState> {
    Router::new().route(
        "/pleroma/accounts/{id}/endorsements",
        get(list_endorsements),
    )
}

/// `POST /api/v1/accounts/{id}/endorse` (and its `/pin` alias) — pin the
/// account to the caller's profile and answer their [`Relationship`] to
/// it, `endorsed: true`.
///
/// The follow requirement is the whole validation: [`is_following`]
/// demands an accepted `caller → target` row (a pending request is not a
/// follow), and the write itself ([`endorse`]) is idempotent on the pair,
/// so re-pinning keeps the stored row — and the listing's cursor — as it
/// is. The [`Relationship::load_many`] after the write is the render of
/// Pleroma's `relationship.json`: the answer states the stored fact
/// rather than trusting the request.
///
/// [`is_following`]: crate::models::follow::is_following
/// [`endorse`]: crate::models::endorsement::endorse
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::NotFound`] for a malformed,
/// unknown or deactivated account id; [`AppError::Unprocessable`] with
/// [`NOT_FOLLOWING`] when the caller does not follow the target (which
/// makes the caller's own id the same 422, nobody follows themselves).
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn endorse_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Relationship>> {
    auth.require(WRITE_ACCOUNTS)?;
    let target = account_or_404(&state, &id).await?;
    if !follow::is_following(&state.db, &auth.0, &target).await? {
        debug!(
            user = %auth.0.id,
            target = %target.id,
            "endorsement refused: not following"
        );
        return Err(AppError::Unprocessable(NOT_FOLLOWING.to_owned()));
    }
    endorsement::endorse(&state.db, auth.0.id, target.id).await?;
    debug!(
        user = %auth.0.id,
        target = %target.id,
        "account endorsed"
    );
    Ok(Json(
        Relationship::load_one(&state.db, auth.0.id, target.id).await?,
    ))
}

/// `POST /api/v1/accounts/{id}/unendorse` (and its `/unpin` alias) —
/// withdraw the caller's pin of the account and answer their
/// [`Relationship`] to it, `endorsed: false`.
///
/// The withdrawal ([`unendorse`]) deletes whatever pair it is given and is
/// content with none: an account that was never pinned (or already
/// unpinned) changes nothing and is answered the unchanged [`Relationship`]
/// with 200, so unpinning is idempotent.
///
/// [`unendorse`]: crate::models::endorsement::unendorse
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::NotFound`] for a malformed,
/// unknown or deactivated account id. Any database failure arrives
/// through `?` as [`AppError::Database`].
async fn unendorse_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Relationship>> {
    auth.require(WRITE_ACCOUNTS)?;
    let target = account_or_404(&state, &id).await?;
    endorsement::unendorse(&state.db, auth.0.id, target.id).await?;
    debug!(
        user = %auth.0.id,
        target = %target.id,
        "endorsement withdrawn"
    );
    Ok(Json(
        Relationship::load_one(&state.db, auth.0.id, target.id).await?,
    ))
}

/// `GET /api/v1/endorsements` — the accounts the caller pins to their own
/// profile, newest endorsement first, one [`PageParams`] page at a time
/// (default limit 20, at most [`MAX_LIMIT`](crate::pagination::MAX_LIMIT))
/// under a `Link` header keyed on the endorsement row ids. The queue is
/// the caller's own, so no privacy check applies.
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`axum::http::Uri`] the router hands a nested handler: axum strips the
/// `/api/v1` prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user
/// token; [`AppError::Forbidden`] when the token does not grant
/// `read:accounts`; [`AppError::BadRequest`] from [`PageParams`] for a
/// malformed cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn own_endorsements(
    State(state): State<AppState>,
    auth: AuthUser,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require(READ_ACCOUNTS)?;
    let entries = endorsement::page_for(&state.db, auth.0.id, page).await?;
    debug!(
        count = entries.len(),
        user = %auth.0.id,
        "own endorsements listed"
    );
    let viewer: AccountViewer<'_> = Some(&auth.0);
    page_response(&state, viewer, &entries, uri.path()).await
}

/// `GET /api/v1/accounts/{id}/endorsements` and
/// `GET /api/v1/pleroma/accounts/{id}/endorsements` — the accounts `id`
/// pins to their profile, same page shape as [`own_endorsements`].
///
/// Auth is optional; a presented user token must grant `read:accounts`,
/// as on the followers/following listings. Pleroma hides a
/// `hidden_follows` account's following list from strangers, and an
/// endorsement is a pinned follow: per this ticket, an account with
/// [`hide_follows`](crate::models::user::User::hide_follows) answers the empty array — 200, no
/// `Link` header — to everyone but itself, and the account itself always
/// sees its own pins.
///
/// # Errors
///
/// Exactly those of [`own_endorsements`] plus [`AppError::NotFound`] for a
/// malformed, unknown or deactivated account id; an invalid token is the
/// extractor's 401, never a silently anonymous request.
async fn list_endorsements(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require_user_scope(READ_ACCOUNTS)?;
    let target = account_or_404(&state, &id).await?;
    let viewer = auth.0.as_ref();
    if target.hide_follows.get() && viewer.is_none_or(|who| who.id != target.id) {
        return Ok(empty_page());
    }
    let entries = endorsement::page_for(&state.db, target.id, page).await?;
    page_response(&state, viewer, &entries, uri.path()).await
}

/// The answer to a hidden endorsement listing: the empty array as a plain
/// 200 with no `Link` header at all — the followers/following listings'
/// hidden branch, where the page never got a cursor to link from.
fn empty_page() -> (HeaderMap, Json<Vec<Account>>) {
    (HeaderMap::new(), Json(Vec::new()))
}

/// Renders one fetched endorsement page for `viewer`: every entry's
/// [`User`] as an [`Account`] with the viewer's relationship embedded
/// where one is preloaded (see [`Relationship::load_map`]), under the
/// `Link` header [`keyset_link`] derives from the endorsement ids.
async fn page_response(
    state: &AppState,
    viewer: AccountViewer<'_>,
    entries: &[endorsement::EndorsementPageEntry],
    path: &str,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    let targets: Vec<Uuid> = entries.iter().map(|entry| entry.user.id).collect();
    let relationships = Relationship::load_map(&state.db, viewer, &targets).await?;
    // The cursors key on the `endorsements` row
    // ([`endorsement::EndorsementPageEntry::endorsement_id`]), not on the
    // accounts.
    let link = keyset_link(
        &state.config.base_url,
        path,
        &NO_EXTRA_QUERY,
        entries,
        |e| e.endorsement_id,
    );
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    let accounts = entries
        .iter()
        .map(|entry| {
            Account::from_user_for(viewer, &entry.user, &state.config, relationships.as_ref())
        })
        .collect();
    Ok((headers, Json(accounts)))
}
