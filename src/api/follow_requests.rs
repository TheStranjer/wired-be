//! The follow-request queue over HTTP: `GET /api/v1/follow_requests` and
//! the two answers `POST /api/v1/follow_requests/{id}/authorize` and
//! `…/reject`.
//!
//! Pleroma's `FollowRequestController`
//! (`lib/pleroma/web/mastodon_api/controllers/follow_request_controller.ex`)
//! is the reference. What a request *is* — a `follows` row in
//! [`FollowState::Pending`] — and how answering it moves the denormalised
//! counters stays behind [`crate::models::follow`]; this module is the
//! three routes, their scopes and their entities in front of it.
//!
//! All three require a user token ([`AuthUser`]); `GET` needs the
//! `read:follows` scope, the two POSTs either `write:follows` or the
//! `follow` root (Pleroma's scopes plug for these actions).
//!
//! - `GET /api/v1/follow_requests` answers the accounts whose follow of the
//!   caller still waits — [`Account`] entities, newest request first — and
//!   pages like the relationship listings do: [`PageParams`] keyset
//!   pagination over the *`follows` row id* (the `Link` header's cursors are
//!   [`Follow::id`](crate::models::follow::Follow::id), as Pleroma's are),
//!   with Mastodon's account-list page sizes: 40 rows by default, at most
//!   80. The queue is the caller's own, so no other endpoint here needs a
//!   privacy check.
//! - `POST …/{id}/authorize` and `POST …/{id}/reject` take the *requester's*
//!   account id as `{id}`, answer 404 unless that account has a pending
//!   request of the caller's (unknown id, malformed id, already answered),
//!   and otherwise run [`follow::accept`] / [`follow::reject`] and answer
//!   the caller's [`Relationship`] to the requester — `followed_by: true`
//!   after an authorization, `false` after a rejection.
//!
//! Answering is also the moment the resolver's own inbox is updated — an
//! `authorize` replaces the request's `follow_request` notification with a
//! `follow` one through this module's `after_follow_request_resolved` hook
//! (see [`crate::services::notify`]) — and the moment the requester is
//! answered: the same hook hands the `Accept` (or `Reject`) to
//! [`crate::services::federation_out`], which queues its delivery to a
//! REMOTE follower's inbox and says nothing to a local one.
//!
//! [`follow::accept`]: crate::models::follow::accept
//! [`follow::reject`]: crate::models::follow::reject

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use tracing::debug;

use crate::{
    auth::AuthUser,
    entities::{
        account::{Account, AccountViewer},
        relationship::Relationship,
    },
    error::{AppError, AppResult},
    models::{
        follow::{self, Follow, FollowState},
        user::User,
    },
    oauth::scopes::{FOLLOW, READ_FOLLOWS, WRITE_FOLLOWS},
    pagination::{NO_EXTRA_QUERY, PageParams, keyset_link, set_link_header},
    services::{federation_out, notify},
    state::AppState,
    util::id::parse_id,
};

/// Default page size of the queue: Mastodon's account-list default (40),
/// where Pleroma's global [`PageParams`] default is 20 — the ticket pins
/// this endpoint to the account-list pair.
const DEFAULT_LIMIT: i64 = 40;

/// Page size cap of the queue: Mastodon's account-list limit (80), above
/// [`crate::pagination::MAX_LIMIT`].
const MAX_LIMIT: i64 = 80;

/// The `/api/v1/follow_requests` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/follow_requests", get(list_follow_requests))
        .route("/follow_requests/{id}/authorize", post(authorize))
        .route("/follow_requests/{id}/reject", post(reject))
}

/// `GET /api/v1/follow_requests` — the accounts with a pending follow of
/// the caller, newest request first, one [`PageParams`] page at a time
/// (default 40, at most 80) with the `Link` header over the request row ids.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user token;
/// [`AppError::Forbidden`] when the token does not grant `read:follows`;
/// [`AppError::BadRequest`] from the page parameters for a malformed
/// cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn list_follow_requests(
    State(state): State<AppState>,
    auth: AuthUser,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require(READ_FOLLOWS)?;
    let page = PageParams::from_query_with_default(uri.query(), DEFAULT_LIMIT, MAX_LIMIT)?;
    let entries = follow::pending_requests_for(&state.db, auth.0.id, page).await?;
    debug!(
        count = entries.len(),
        user = %auth.0.id,
        "follow requests listed"
    );
    // The cursors key on the `follows` row ([`FollowPageEntry::follow_id`]),
    // not on the requesters.
    let link = keyset_link(
        &state.config.base_url,
        uri.path(),
        &NO_EXTRA_QUERY,
        &entries,
        |e| e.follow_id,
    );
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    // The queue is answered for its owner; Pleroma's index renders no
    // embedded relationship here (`embed_relationships` is not set), so the
    // entries keep the empty `pleroma.relationship` object.
    let viewer: AccountViewer<'_> = Some(&auth.0);
    let accounts = entries
        .iter()
        .map(|entry| Account::from_user_for(viewer, &entry.user, &state.config, None))
        .collect();
    Ok((headers, Json(accounts)))
}

/// `POST /api/v1/follow_requests/{id}/authorize` — grant the pending
/// request whose *requester* has account id `id` and answer the caller's
/// [`Relationship`] to them, now `followed_by: true`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:follows | follow` check; [`AppError::NotFound`] when `id`
/// is malformed, names no user, or that user has no pending request of the
/// caller's (an answered request is a 404, not an idempotent repeat). Any
/// database failure arrives through `?` as [`AppError::Database`].
async fn authorize(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Relationship>> {
    auth.require_any(&[WRITE_FOLLOWS, FOLLOW])?;
    let request = pending_request_of(&state, &auth.0, &id).await?;
    let granted = follow::accept(&state.db, request.id).await?;
    debug!(
        follow = %granted.id,
        user = %auth.0.id,
        "follow request authorized"
    );
    after_follow_request_resolved(&state, &auth.0, &granted, true).await;
    Ok(Json(
        Relationship::load_one(&state.db, auth.0.id, granted.follower_id).await?,
    ))
}

/// `POST /api/v1/follow_requests/{id}/reject` — withdraw the pending
/// request whose *requester* has account id `id` (the row is deleted, no
/// counters moved) and answer the caller's [`Relationship`] to them,
/// `followed_by: false`.
///
/// # Errors
///
/// Exactly those of [`authorize`].
async fn reject(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Relationship>> {
    auth.require_any(&[WRITE_FOLLOWS, FOLLOW])?;
    let request = pending_request_of(&state, &auth.0, &id).await?;
    let removed = follow::reject(&state.db, request.id).await?;
    debug!(
        follow = %removed.id,
        user = %auth.0.id,
        "follow request rejected"
    );
    after_follow_request_resolved(&state, &auth.0, &removed, false).await;
    Ok(Json(
        Relationship::load_one(&state.db, auth.0.id, removed.follower_id).await?,
    ))
}

/// The caller's pending request from the account `id_str` names, or the
/// 404 this endpoint owes for every miss: `id_str` is the *requester's*
/// account id (Pleroma's `assign_follower`), so a malformed id, an unknown
/// one, and one whose relationship with `resolver` is not pending all
/// answer the same [`AppError::NotFound`].
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives
/// through `?` as [`AppError::Database`].
async fn pending_request_of(state: &AppState, resolver: &User, id_str: &str) -> AppResult<Follow> {
    let Some(requester_id) = parse_id(id_str) else {
        return Err(AppError::NotFound);
    };
    follow::get(&state.db, requester_id, resolver.id)
        .await?
        .filter(|request| request.state == FollowState::Pending)
        .ok_or(AppError::NotFound)
}

/// Post-resolution hook of both answer routes: the notification telling the
/// [`notify`] module owns runs through here — an
/// accepted request replaces its `follow_request` telling with a `follow`
/// one, a rejection says nothing new — and so does the federation of the
/// answer itself: [`crate::services::federation_out`] sends the `Accept` of
/// an authorization and the `Reject` of a refusal through the delivery
/// queue (decision 11). The `Follow` activity answered is
/// [`Follow::ap_id`], its sender [`Follow::follower_id`]'s account, and a
/// LOCAL requester's answer stays in the database: only a REMOTE follower
/// is ever delivered to. The hook never fails the request.
async fn after_follow_request_resolved(
    state: &AppState,
    resolver: &User,
    request: &Follow,
    granted: bool,
) {
    notify::on_follow_request_resolved(state, resolver, request, granted).await;
    federation_out::after_follow_request_resolved(state, resolver, request, granted).await;
}
