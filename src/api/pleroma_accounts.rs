//! Pleroma's account extensions under `/api/v1/pleroma`
//! (`docs/ARCHITECTURE.md` decision 1): the subscription toggles, the
//! caller's follow asks, one account's favourites, and the confirmation
//! resend.
//!
//! The reference is Pleroma's `PleromaAPI.AccountController`
//! (`lib/pleroma/web/pleroma_api/controllers/account_controller.ex`, the
//! `subscribe`, `unsubscribe`, `favourites` and `confirmation_resend`
//! actions) and
//! `PleromaAPI.FollowRequestController`
//! (`lib/pleroma/web/pleroma_api/controllers/follow_request_controller.ex`,
//! the `outgoing` action). What a subscription *is* on this server stays
//! behind [`crate::models::follow`]: Pleroma's `subscribe` sets the
//! notification flag of the follower's own `follows` row, which is exactly
//! the column Mastodon's `follow?notify` writes and the `notifying` /
//! `subscribing` flags of [`Relationship`] are read from, so
//! [`set_notify`] is the one write all three endpoints share.
//!
//! The subscription routes and the outgoing listing require a user token
//! ([`AuthUser`]); the favourites listing authenticates optionally
//! ([`MaybeAuthUser`]); the confirmation resend needs no credential at all.
//!
//! - `POST /api/v1/pleroma/accounts/{id}/subscribe` and `…/unsubscribe`
//!   need either the `follow` root or `write:follows` (Pleroma's scopes plug
//!   for these two actions). They resolve `{id}` to the followed account
//!   ([`account_or_404`]: a malformed, unknown or deactivated id is the 404
//!   Pleroma's `assign_account_by_id` answers) and then flip the `notify`
//!   toggle of the caller's *accepted* follow of it. Without such a
//!   relationship nothing is written and the answer is 422 with the refusal
//!   wording this ticket pins; on success the answer is the caller's
//!   [`Relationship`] to the account, so `subscribing` and `notifying` carry
//!   the stored value (`true` after subscribing, `false` after
//!   unsubscribing).
//! - `GET /api/v1/pleroma/outgoing_follow_requests` needs `read:follows` and
//!   answers the [`Account`] entities of the users the caller has asked to
//!   follow and who have not answered yet — newest request first. It pages
//!   like the incoming queue does: [`PageParams`] keyset pagination over the
//!   *`follows` row id* (the [`Link`](crate::pagination::link_header)
//!   cursors are [`Follow::id`](crate::models::follow::Follow::id), as
//!   Pleroma's are), with Pleroma's global page sizes (default 20, at most
//!   40). The queue is the caller's own, so no privacy check is needed.
//! - `GET /api/v1/pleroma/accounts/{id}/favourites` answers the statuses
//!   account `id` favourited — newest favourite first, and only those the
//!   viewer may still see ([`filter_visible`], the reach of Pleroma's
//!   `blocking_user` narrowing). Auth is optional: a presented user token
//!   must grant `read:favourites` (Pleroma's scopes plug with
//!   `fallback: :proceed_unauthenticated`) and decides whose eyes narrow
//!   the list. Privacy first: with `hide_favorites = true` on the target —
//!   the default for every account — the answer is Pleroma's 403
//!   `"Can't get favorites"` to everybody but the target themselves. It
//!   pages exactly like the caller's own `GET /api/v1/favourites`:
//!   [`PageParams`] keyset pagination over the *favourite row ids*
//!   (Pleroma's global page sizes, default 20, at most 40), `Link` cursors
//!   on those same like ids, and the pairs [`page_for_user`] already
//!   dropped with their statuses.
//! - `POST /api/v1/pleroma/accounts/confirmation_resend` is the one route
//!   here with no credential at all: an account that cannot log in is the
//!   only one that needs its confirmation letter again. It reads `email`,
//!   re-mails the local account that is still unconfirmed (and nothing for
//!   any other request, including the ones naming no address), and answers
//!   204 with an empty body always — see `confirmation_resend`. Pleroma
//!   throttles this action through its `account_confirmation_resend`
//!   bucket, and [`router`] mounts the route on that bucket.
//!
//! [`Relationship`]: crate::entities::relationship::Relationship
//! [`set_notify`]: crate::models::follow::set_notify
//! [`filter_visible`]: crate::models::status_visibility::filter_visible
//! [`page_for_user`]: crate::models::favourite::page_for_user

use std::collections::HashSet;

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::Deserialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::account_or_404,
    auth::{AuthUser, MaybeAuthUser},
    entities::{
        account::{Account, AccountViewer},
        relationship::Relationship,
        status::Status,
    },
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        favourite, follow,
        status::Status as StatusRow,
        status_visibility,
        user::{User, confirmation},
    },
    oauth::scopes::{FOLLOW, READ_FAVOURITES, READ_FOLLOWS, WRITE_FOLLOWS},
    pagination::{NO_EXTRA_QUERY, PageParams, keyset_link, set_link_header},
    services::{
        account_emojis::AccountEmojis,
        render_status::{ViewerMode, render_statuses},
    },
    state::AppState,
};

/// The refusal both subscription routes answer when the caller has no
/// accepted follow of the target: the wording this ticket pins for
/// `subscribe`, which `unsubscribe` answers alike — the same missing
/// relationship is what makes either action impossible.
const NOT_FOLLOWING: &str = "Could not subscribe: You are not following this user";

/// The refusal the favourites route answers when the target hides their
/// favourites from the viewer: Pleroma's `AccountController.favourites`
/// `hide_favorites` arm, in its exact (American-spelled) wording.
const CANT_GET_FAVOURITES: &str = "Can't get favorites";

/// The `/api/v1/pleroma` account router, merged into [`crate::api::v1`].
///
/// `confirmation_resend` runs in `account_confirmation_resend`, the bucket
/// Pleroma throttles that action through and [`crate::rate_limit`] already
/// counts by name.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/pleroma/accounts/{id}/favourites", get(favourites))
        .route("/pleroma/accounts/{id}/subscribe", post(subscribe))
        .route("/pleroma/accounts/{id}/unsubscribe", post(unsubscribe))
        .route(
            "/pleroma/accounts/confirmation_resend",
            post(confirmation_resend).layer(crate::rate_limit::rate_limit(
                state,
                "account_confirmation_resend",
            )),
        )
        .route("/pleroma/outgoing_follow_requests", get(outgoing))
}

/// The request parameters of `POST /api/v1/pleroma/accounts/confirmation_resend`:
/// Pleroma's `User.get_by_nickname_or_email` accepts either spelling, and
/// this ticket's parameter list names the address, so `email` is the only
/// key read — and it is optional, because its absence is the same 204 every
/// other refusal answers.
#[derive(Debug, Deserialize)]
struct ConfirmationResendParams {
    /// The address to re-mail, `None` when the body carried none.
    #[serde(default)]
    email: Option<String>,
}

/// `POST /api/v1/pleroma/accounts/confirmation_resend` — send the
/// confirmation letter again to the account whose `email` is still
/// unconfirmed (Pleroma's `PleromaAPI.AccountController.confirmation_resend`,
/// `lib/pleroma/web/pleroma_api/controllers/account_controller.ex`).
///
/// No credential is asked for — the caller cannot log in yet, which is the
/// whole reason the letter is owed — and the answer is 204 with an empty
/// body whether or not anything was sent, so the endpoint cannot be used to
/// probe which addresses are registered here. When an unconfirmed account
/// does own the address, [`send`] replaces its token with a freshly minted
/// one — the old link stops working — and queues the letter carrying it.
///
/// Pleroma runs this action through its `account_confirmation_resend`
/// throttle bucket (`config/config.exs`); [`router`] mounts the route on
/// that bucket, which [`crate::rate_limit`] has counted by name since it
/// first configured the Pleroma set.
///
/// [`send`]: crate::models::user::confirmation::send
///
/// # Errors
///
/// Any database or job-queue failure arrives through `?` as
/// [`AppError::Database`]; the body shape [`ApiBody`] accepts is the only
/// other refusal, and it leaks nothing about the address.
async fn confirmation_resend(
    State(state): State<AppState>,
    ApiBody(params): ApiBody<ConfirmationResendParams>,
) -> AppResult<StatusCode> {
    let Some(email) = params
        .email
        .as_deref()
        .map(str::trim)
        .filter(|email| !email.is_empty())
    else {
        return Ok(StatusCode::NO_CONTENT);
    };
    let Some(user) = confirmation::unconfirmed_by_email(&state.db, email).await? else {
        return Ok(StatusCode::NO_CONTENT);
    };
    confirmation::send(&state, &user).await?;
    debug!(account = %user.id, "resent the confirmation letter");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/pleroma/accounts/{id}/subscribe` — turn notifications on for
/// the caller's accepted follow of account `id` and answer their
/// [`Relationship`] to it (`subscribing` and `notifying` both `true`).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `follow | write:follows` check; [`AppError::NotFound`] when `id`
/// is malformed, names no user or names a deactivated one;
/// [`AppError::Unprocessable`] when the caller has no accepted follow of it.
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn subscribe(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Relationship>> {
    auth.require_any(&[FOLLOW, WRITE_FOLLOWS])?;
    subscription(&state, &auth.0, &id, true).await
}

/// `POST /api/v1/pleroma/accounts/{id}/unsubscribe` — turn the caller's
/// notifications for account `id` off again and answer their
/// [`Relationship`] to it (`subscribing` and `notifying` both `false`).
///
/// # Errors
///
/// Exactly those of [`subscribe`].
async fn unsubscribe(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Relationship>> {
    auth.require_any(&[FOLLOW, WRITE_FOLLOWS])?;
    subscription(&state, &auth.0, &id, false).await
}

/// The body of both subscription routes: resolve the target the route names,
/// write `notify` onto the caller's accepted follow of it and answer the
/// reloaded [`Relationship`].
///
/// The write is the check: [`follow::set_notify`] matches an accepted row
/// only, so a caller who never followed `target`, or whose request nobody
/// answered, writes nothing and is refused here — the same fact in one
/// statement rather than a read that a concurrent unfollow could race.
///
/// # Errors
///
/// [`AppError::NotFound`] from [`account_or_404`] and
/// [`AppError::Unprocessable`] with [`NOT_FOLLOWING`] as described; any
/// database failure arrives through `?` as [`AppError::Database`].
async fn subscription(
    state: &AppState,
    subscriber: &User,
    id_str: &str,
    notify: bool,
) -> AppResult<Json<Relationship>> {
    let target = account_or_404(state, id_str).await?;
    let updated = follow::set_notify(&state.db, subscriber.id, target.id, notify).await?;
    if updated.is_none() {
        debug!(
            user = %subscriber.id,
            target = %target.id,
            notify,
            "subscription refused: no accepted follow"
        );
        return Err(AppError::Unprocessable(NOT_FOLLOWING.to_owned()));
    }
    debug!(
        user = %subscriber.id,
        target = %target.id,
        notify,
        "subscription updated"
    );
    Ok(Json(
        Relationship::load_one(&state.db, subscriber.id, target.id).await?,
    ))
}

/// `GET /api/v1/pleroma/outgoing_follow_requests` — the accounts the caller
/// has asked to follow and who have not answered, newest request first, one
/// [`PageParams`] page at a time (default 20, at most 40) with the `Link`
/// header over the request row ids.
///
/// The request rows are the caller's own, so the list needs no privacy
/// check; a `rejected` row is not a request, and an answered one is a
/// follow, so neither reaches the page (see
/// [`outgoing_requests_from`](crate::models::follow::outgoing_requests_from)).
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`axum::http::Uri`] the router hands a nested handler: axum strips the
/// `/api/v1` prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user token;
/// [`AppError::Forbidden`] when the token does not grant `read:follows`;
/// [`AppError::BadRequest`] from the page parameters for a malformed
/// cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn outgoing(
    State(state): State<AppState>,
    auth: AuthUser,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require(READ_FOLLOWS)?;
    let entries = follow::outgoing_requests_from(&state.db, auth.0.id, page).await?;
    debug!(
        count = entries.len(),
        user = %auth.0.id,
        "outgoing follow requests listed"
    );
    // The cursors key on the `follows` row
    // ([`follow::FollowPageEntry::follow_id`]), not on the accounts.
    let link = keyset_link(
        &state.config.base_url,
        uri.path(),
        &NO_EXTRA_QUERY,
        &entries,
        |e| e.follow_id,
    );
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    // Pleroma's `outgoing` renders through `FollowRequestView`, which
    // delegates to the account view's `index.json` with no relationships
    // preloaded: the entries keep the empty `pleroma.relationship` object.
    let viewer: AccountViewer<'_> = Some(&auth.0);
    let users: Vec<_> = entries.iter().map(|entry| &entry.user).collect();
    let emojis = AccountEmojis::load(&state.db, &users).await?;
    let accounts = entries
        .iter()
        .map(|entry| {
            let account = Account::from_user_for(viewer, &entry.user, &state.config, None);
            emojis.fill(account, &entry.user, &state.config)
        })
        .collect();
    Ok((headers, Json(accounts)))
}

/// `GET /api/v1/pleroma/accounts/{id}/favourites` — the statuses account
/// `id` favourited, newest favourite first, one [`PageParams`] page at a
/// time (default 20, at most 40) with the `Link` header over the *favourite
/// row ids*, narrowed to what the viewer may see.
///
/// Auth is optional ([`MaybeAuthUser`], Pleroma's `read:favourites` plug
/// with `fallback: :proceed_unauthenticated`): a presented user token must
/// grant the scope, an anonymous request and an app-only token proceed as
/// unauthenticated viewers. The target resolves through [`account_or_404`]
/// — the malformed, unknown or deactivated id is Pleroma's shared 404 —
/// and the target's `hide_favorites` flag then gates the read: hidden
/// favourites are [`CANT_GET_FAVOURITES`] for everybody but the target
/// reading their own list. Behind the gate the page is the caller's own
/// favourites list read the other way round: [`page_for_user`] walks the
/// target's like rows, [`filter_visible`] drops the statuses this viewer
/// may not see (Pleroma's `blocking_user` recipients filter, restated over
/// the relational model), and only the favourites that come back visible
/// name the `Link` edges — the cursors key on the like, like the own list
/// does. The entities render for the viewer, so `favourited` and friends
/// carry *their* state, not the target's.
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`axum::http::Uri`] the router hands a nested handler: axum strips the
/// `/api/v1` prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`MaybeAuthUser`] for a presented token
/// that does not resolve; [`AppError::Forbidden`] when a presented user
/// token does not grant `read:favourites` or when the target hides their
/// favourites from this viewer; [`AppError::NotFound`] when `id` is
/// malformed, names no user or names a deactivated one;
/// [`AppError::BadRequest`] from the page parameters for a malformed
/// cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
///
/// [`page_for_user`]: crate::models::favourite::page_for_user
/// [`filter_visible`]: crate::models::status_visibility::filter_visible
async fn favourites(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Status>>)> {
    auth.require_user_scope(READ_FAVOURITES)?;
    let target = account_or_404(&state, &id).await?;
    let viewer = auth.0.as_ref();
    let reading_own = viewer.is_some_and(|viewer| viewer.id == target.id);
    if target.hide_favorites.get() && !reading_own {
        return Err(AppError::Forbidden(CANT_GET_FAVOURITES.to_owned()));
    }
    let pairs = favourite::page_for_user(&state.db, target.id, page).await?;
    let rows: Vec<StatusRow> = pairs.iter().map(|(_, row)| row.clone()).collect();
    let visible = status_visibility::filter_visible(&state.db, rows, viewer).await?;
    let visible_ids: HashSet<Uuid> = visible.iter().map(|row| row.id).collect();
    let cursors: Vec<Uuid> = pairs
        .iter()
        .filter(|(favourite, _)| visible_ids.contains(&favourite.status_id))
        .map(|(favourite, _)| favourite.id)
        .collect();
    debug!(
        target = %target.id,
        favourites = visible.len(),
        "account favourites listed"
    );
    let mut headers = HeaderMap::new();
    // The cursors are the *favourite row ids* of the visible page — the shape
    // `GET /api/v1/favourites` links in, keyed on the like rows.
    let link = keyset_link(
        &state.config.base_url,
        uri.path(),
        &NO_EXTRA_QUERY,
        &cursors,
        |cursor| *cursor,
    );
    set_link_header(&mut headers, link.as_deref())?;
    Ok((
        headers,
        Json(render_statuses(&state, ViewerMode::User(viewer), &visible).await?),
    ))
}
