//! Pleroma's account extensions under `/api/v1/pleroma`
//! (`docs/ARCHITECTURE.md` decision 1): the subscription toggles, the
//! caller's own unanswered follow asks, and the confirmation resend.
//!
//! The reference is Pleroma's `PleromaAPI.AccountController`
//! (`lib/pleroma/web/pleroma_api/controllers/account_controller.ex`, the
//! `subscribe`, `unsubscribe` and `confirmation_resend` actions) and
//! `PleromaAPI.FollowRequestController`
//! (`lib/pleroma/web/pleroma_api/controllers/follow_request_controller.ex`,
//! the `outgoing` action). What a subscription *is* on this server stays
//! behind [`crate::models::follow`]: Pleroma's `subscribe` sets the
//! notification flag of the follower's own `follows` row, which is exactly
//! the column Mastodon's `follow?notify` writes and the `notifying` /
//! `subscribing` flags of [`Relationship`] are read from, so
//! [`set_notify`] is the one write all three endpoints share.
//!
//! The three subscription and listing routes require a user token
//! ([`AuthUser`]).
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
//! - `POST /api/v1/pleroma/accounts/confirmation_resend` is the one route
//!   here with no credential at all: an account that cannot log in is the
//!   only one that needs its confirmation letter again. It reads `email`,
//!   re-mails the local account that is still unconfirmed (and nothing for
//!   any other request, including the ones naming no address), and answers
//!   204 with an empty body always — see `confirmation_resend`. Pleroma
//!   throttles this action through its `account_confirmation_resend` bucket;
//!   this server has no rate limiter yet, so the bucket waits for the ticket
//!   that adds one.
//!
//! [`Relationship`]: crate::entities::relationship::Relationship
//! [`set_notify`]: crate::models::follow::set_notify

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::Deserialize;
use tracing::debug;

use crate::{
    api::helpers::account_or_404,
    auth::AuthUser,
    entities::{
        account::{Account, AccountViewer},
        relationship::Relationship,
    },
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        follow,
        user::{User, confirmation},
    },
    oauth::scopes::{FOLLOW, READ_FOLLOWS, WRITE_FOLLOWS},
    pagination::{PageParams, link_header, set_link_header},
    state::AppState,
};

/// The refusal both subscription routes answer when the caller has no
/// accepted follow of the target: the wording this ticket pins for
/// `subscribe`, which `unsubscribe` answers alike — the same missing
/// relationship is what makes either action impossible.
const NOT_FOLLOWING: &str = "Could not subscribe: You are not following this user";

/// The `Link` header's query parameters carry nothing but the cursor: the
/// outgoing list has no filter to repeat on either link (the queue's
/// constant, same rule).
const NO_EXTRA_QUERY: [(String, String); 0] = [];

/// The `/api/v1/pleroma` account router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/pleroma/accounts/{id}/subscribe", post(subscribe))
        .route("/pleroma/accounts/{id}/unsubscribe", post(unsubscribe))
        .route(
            "/pleroma/accounts/confirmation_resend",
            post(confirmation_resend),
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
/// throttle bucket (`config/config.exs`); this server has no rate limiter
/// yet, and the ticket that adds one registers the route under that bucket
/// name.
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
    relationship_for(state, subscriber, &target).await
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
    let link = page_link(&state.config.base_url, uri.path(), &entries);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    // Pleroma's `outgoing` renders through `FollowRequestView`, which
    // delegates to the account view's `index.json` with no relationships
    // preloaded: the entries keep the empty `pleroma.relationship` object.
    let viewer: AccountViewer<'_> = Some(&auth.0);
    let accounts = entries
        .iter()
        .map(|entry| Account::from_user_for(viewer, &entry.user, &state.config, None))
        .collect();
    Ok((headers, Json(accounts)))
}

/// The [`Relationship`] a subscription route answers: [`Relationship::load_many`]
/// from the subscriber's perspective over the single target, so `notifying`
/// and its `subscribing` alias reflect exactly what the write stored.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`]; the
/// [`AppError::Internal`] arm only documents that the loader answers one row
/// per requested id, which a single-target batch always does.
async fn relationship_for(
    state: &AppState,
    subscriber: &User,
    target: &User,
) -> AppResult<Json<Relationship>> {
    let loaded = Relationship::load_many(&state.db, subscriber.id, &[target.id]).await?;
    let relationship = loaded.into_iter().next().ok_or_else(|| {
        AppError::Internal("the relationship loader skipped its only target".to_owned())
    })?;
    Ok(Json(relationship))
}

/// The `Link` header value for a fetched page of outgoing requests: Pleroma's
/// `add_link_headers` over the first (newest) and last (oldest) entry's
/// [`follow::FollowPageEntry::follow_id`] — the cursors key on the `follows`
/// row, not on the accounts — and `None` for an empty page.
fn page_link(base_url: &str, path: &str, entries: &[follow::FollowPageEntry]) -> Option<String> {
    let newest = entries.first()?.follow_id.to_string();
    let oldest = entries.last()?.follow_id.to_string();
    link_header(base_url, path, &NO_EXTRA_QUERY, &newest, &oldest)
}
