//! The search endpoints: `GET /api/v1/accounts/search`, and the `q`-keyed
//! combined search `GET /api/v1/search` / `GET /api/v2/search`
//! (`docs/ARCHITECTURE.md` decision 1).
//!
//! Reference: Pleroma's `SearchController.account_search`, `.search` and
//! `.search2` (`lib/pleroma/web/mastodon_api/controllers/search_controller.ex`)
//! over `User.Search.search` (`lib/pleroma/user/search.ex`),
//! `Activity.Search.search` (`lib/pleroma/activity/search.ex`) and
//! `Hashtag.search` (`lib/pleroma/hashtag.ex`). The three searches are this
//! module; each query itself — normalisation, matching, ranking and the
//! per-viewer exclusions — is the model's, in [`crate::models::user_search`],
//! [`crate::models::status_search`] and [`crate::models::hashtag`].
//!
//! Authentication is [`MaybeAuthUser`] with the `read:accounts` scope
//! (the combined `/api/v1/search` and `/api/v2/search` documents demand
//! `read:search` instead) enforced on a presented user token
//! ([`require_user_scope`](MaybeAuthUser::require_user_scope)), exactly
//! Pleroma's `OAuthScopesPlug … fallback: :proceed_unauthenticated`: an
//! anonymous request searches the stored, active accounts and the hashtag
//! names, a token without the scope is the 403, and an invalid token is
//! still the extractor's 401. A viewer is not decoration either: they name
//! the account the [`Account`] and status entities render for, drive every
//! search's block/mute exclusions and the visibility clause the status
//! search runs under, and are the only way the `following` restriction
//! means anything — `following=true` without a viewer searches an empty
//! following list and answers nothing.
//!
//! Statuses keep one gate of their own: the `statuses` list is only ever
//! filled for a request that carries a viewer, unless the operator turns
//! on [`search_statuses_unauthenticated`](crate::config::Config::search_statuses_unauthenticated)
//! (`SEARCH_STATUSES_UNAUTHENTICATED`); an anonymous request then reads the
//! stored text too, with the visibility clause still holding it to
//! `public` and `unlisted` rows.
//!
//! `offset` paging is for a viewer only: an anonymous request that carries a
//! non-blank `offset` is the 401 `OFFSET_UNAUTHENTICATED`, as Mastodon
//! refuses it, since a deep offset makes the database rank every match
//! before skipping it.
//!
//! The accounts list pages by `offset` ([`PageParams`] with the account
//! listings' default 40 and cap 80): a ranking over a changing corpus has
//! no stable id cursor, which is why Pleroma pages search with
//! `fetch_paginated(…, :offset)` too. The statuses list pages by the
//! keyset ids and the `offset` alike, on its own default 20 and cap 40;
//! the hashtag list takes the same page size and walks names with
//! `offset` only. An absent or empty `q` answers nothing anywhere (the
//! empty array, or all three lists of the search document empty) — never
//! the whole table.
//!
//! `GET /api/v1/search` and `GET /api/v2/search` share one code path.
//! `type` picks the resource whose list the document fills: absent, every
//! searchable list answers at once; `accounts`, `statuses` or `hashtags`
//! (case-insensitive) fill exactly that one and leave the other two empty;
//! any other spelling fills nothing, as before this ticket. One exception
//! rides on the untyped search: a query beginning with `#` is a hashtag
//! term, not text, and fills only the hashtag list. The two versions answer
//! the same document until v1's hashtag arm differs: v1 spells hashtag
//! entries as plain name strings and v2 as [`Tag`] entities whose
//! (uncomputed) `history` renders as the empty list.
//!
//! `resolve=true` is the federation arm of the same three routes, and it
//! runs only for an authenticated viewer: reaching past the stored rows is
//! a lookup on somebody's behalf, so an anonymous request gets exactly what
//! the ordinary search answers. The query's shape picks the lookup. A
//! `nick@domain` handle the stored search matched no row for exactly goes
//! through [`get_or_fetch_by_acct`] and its account rides first in
//! `accounts`. An `http(s)` URL is read as an actor id first —
//! [`get_or_fetch_by_ap_id`] answers a stored or local actor from the
//! database and refuses an unknown local id without touching the network —
//! and, when that reading finds no actor, as a status through
//! [`resolve_status`]. A resolved status rides first in `statuses`, one the
//! viewer may not see is left out altogether, and its author is never
//! added to `accounts` as a second copy of itself. [`resolve_status`] also
//! matches this instance's own `/objects/{uuid}` and `/notice/{uuid}` rows
//! from the database, and the one local form it does not name,
//! `/users/{nickname}/statuses/{id}`, is read by id through
//! [`status::get_by_id`]: a URL on this instance's own origin is never
//! dereferenced over HTTP. Every lookup runs under the five-second
//! `RESOLVE_BUDGET`, the
//! request spends at most one of them (an attempt that fails falls through
//! to the next reading, only a success counts), every failure is silent,
//! and `type` narrows the readings to the resource it names: `type=accounts`
//! fetches no post, `type=statuses` fetches no account.
//!
//! All three routes run in the limiter's `search` bucket (Pleroma
//! throttles the three search actions there; see [`crate::rate_limit`]),
//! which is the bucket a resolving request spends too — the lookup rides
//! inside the same routed handler, so no `resolve=true` query escapes it.

//! This module is a tree. `params` parses the request into its search
//! parameters and reads the flags, `find` runs the stored searches and
//! renders their entities, `resolve` holds the resolver, the resolution
//! and the query-shape routing of one request's lookup, `fetch` performs
//! that lookup's remote reads, and `test_support` holds the unit-test
//! fixtures the children share.
//!
//! [`PageParams`]: crate::pagination::PageParams
//! [`Tag`]: crate::entities::tag::Tag
//! [`get_or_fetch_by_acct`]: crate::federation::fetch_actor::get_or_fetch_by_acct
//! [`get_or_fetch_by_ap_id`]: crate::federation::fetch_actor::get_or_fetch_by_ap_id
//! [`resolve_status`]: crate::federation::fetch_object::resolve_status
//! [`status::get_by_id`]: crate::models::status::get_by_id

use std::time::Duration;

use axum::{Json, Router, extract::State, http::Uri, routing::get};
use serde::Serialize;

use crate::{
    auth::MaybeAuthUser,
    entities::{account::Account, status::Status as StatusEntity},
    error::{AppError, AppResult},
    models::user::User,
    oauth::scopes::{READ_ACCOUNTS, READ_SEARCH},
    state::AppState,
};

use self::{
    find::{account_entities, account_rows, find_hashtags, find_statuses, statuses_are_searchable},
    params::{HashtagList, Resource, SearchParams},
    resolve::{Arms, Resolver, resolve_query},
};

mod fetch;
mod find;
mod params;
mod resolve;

#[cfg(test)]
mod test_support;

/// Default page size of the account search (`40`, what the account ticket
/// pins; Pleroma's `@search_limit`).
const LIST_LIMIT: i64 = 40;

/// Cap of [`LIST_LIMIT`]: the most the account search answers at once
/// (Pleroma's account-listing `min(80)`).
const LIST_CAP: i64 = 80;

/// Default page size of the statuses and hashtag searches (`20`, the
/// ticket's pin — Pleroma's `Hashtag.search` default, and the usual
/// timeline page for statuses).
const TEXT_LIST_LIMIT: i64 = 20;

/// Cap of [`TEXT_LIST_LIMIT`]: the most either of them answers at once
/// (the ticket's status-search `max 40`, which Pleroma applies to the
/// same arm with `Enum.min([limit, 40])`).
const TEXT_LIST_CAP: i64 = 40;

/// The row to start at when the request carries no `offset`: the first
/// page ([`PageParams::offset`] reads an absent or unusable offset as
/// `None`).
///
/// [`PageParams::offset`]: crate::pagination::PageParams::offset
const NO_OFFSET: i64 = 0;

/// 400 body for an `account_id` that is not a valid id — the shared
/// pagination wording.
const INVALID_ID: &str = "Invalid id";

/// 401 body for an anonymous request that pages by `offset`.
const OFFSET_UNAUTHENTICATED: &str =
    "Search queries pagination is not supported without authentication";

/// `type` spelling that searches accounts.
const ACCOUNTS_TYPE: &str = "accounts";

/// `type` spelling that searches statuses.
const STATUSES_TYPE: &str = "statuses";

/// `type` spelling that searches hashtags.
const HASHTAGS_TYPE: &str = "hashtags";

/// The whole budget of one resolving search's remote lookup: five seconds
/// for the round the lookup runs — `WebFinger` and the actor dereference, or
/// the object dereference and its author's — past which the arm answers as
/// if nothing had resolved. The same five seconds a remote mention lookup
/// gives itself (see [`crate::services::mentions::REMOTE_LOOKUP_BUDGET`]).
const RESOLVE_BUDGET: Duration = Duration::from_secs(5);

/// The path segment a local actor URL opens with, and the one a local
/// perma-link opens with too (see [`local_permalink_id`]).
///
/// [`local_permalink_id`]: crate::api::search::fetch::local_permalink_id
const USERS_SEGMENT: &str = "users";

/// The segment only a local perma-link adds after the nickname:
/// `{BASE_URL}/users/{nickname}/statuses/{id}`.
const STATUSES_SEGMENT: &str = "statuses";

/// Which combined-search route answered: the two versions render the
/// hashtag list differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Version {
    /// `GET /api/v1/search`: hashtag entries are plain name strings.
    V1,
    /// `GET /api/v2/search`: hashtag entries are [`Tag`] entities.
    ///
    /// [`Tag`]: crate::entities::tag::Tag
    V2,
}

/// The `/api/v1` search router, merged into [`crate::api::v1`].
///
/// Pleroma's account search and the v1 combined search, both counted into
/// the limiter's `search` bucket (Pleroma's
/// `plug RateLimiter, [name: :search] when action in [:search, :search2,
/// :account_search]`), so `state` rides along to build the layer.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/accounts/search",
            get(account_search).layer(crate::rate_limit::rate_limit(state, "search")),
        )
        .route(
            "/search",
            get(search).layer(crate::rate_limit::rate_limit(state, "search")),
        )
}

/// The `/api/v2` side of the search router, merged into [`crate::api::v2`]:
/// Mastodon's v2 combined search — the sibling of the v1 route with the
/// v2 hashtag render — and the same `search` rate-limit bucket.
pub fn v2_router(state: &AppState) -> Router<AppState> {
    Router::new().route(
        "/search",
        get(search_v2).layer(crate::rate_limit::rate_limit(state, "search")),
    )
}

/// The whole combined-search answer, all three resource lists present in
/// every reply (Mastodon's shape, which Pleroma's `do_search` renders
/// even when only one resource was searched): the accounts, the statuses
/// and the hashtags the request's parameters asked for — each arm that
/// was not searched (or, for statuses, not permitted) an empty list.
#[derive(Debug, Serialize)]
struct SearchResults {
    /// The matching [`Account`] entities, in the model's ranking.
    accounts: Vec<Account>,
    /// The matching [`StatusEntity`] rows, newest id first.
    statuses: Vec<StatusEntity>,
    /// The matching hashtags, in the answering version's spelling.
    hashtags: HashtagList,
}

/// Refuses offset paging for a request without a viewer: a deep `OFFSET`
/// forces a full ranked scan of every match before skipping it, a cost an
/// anonymous caller must not be able to demand (Mastodon's
/// `Search queries pagination is not supported without authentication`).
///
/// # Errors
///
/// [`AppError::Unauthorized`] when `auth` carries no user and `params` has
/// an `offset`.
fn refuse_anonymous_offset(auth: &MaybeAuthUser, params: &SearchParams) -> AppResult<()> {
    if auth.0.is_none() && params.has_offset() {
        return Err(AppError::Unauthorized(OFFSET_UNAUTHENTICATED.to_owned()));
    }
    Ok(())
}

/// `GET /api/v1/accounts/search` — the account search on its own, answered
/// as a flat array of [`Account`] entities (Mastodon's account-search
/// spelling, which Pleroma renders through its `AccountView`). A resolving
/// request reads the same array, with the account it fetched first; it runs
/// no status lookup, because this route has no status list to put one in.
///
/// # Errors
///
/// [`AppError::Unauthorized`](crate::error::AppError) from
/// [`MaybeAuthUser`] for an invalid, expired or deactivated token;
/// [`AppError::Forbidden`](crate::error::AppError) when a presented user
/// token does not grant `read:accounts`; [`AppError::BadRequest`] from
/// [`PageParams`] or [`account_id_param`] for a malformed cursor or
/// author id. Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
///
/// [`PageParams`]: crate::pagination::PageParams
/// [`account_id_param`]: crate::api::search::params::account_id_param
/// [`AppError::BadRequest`]: crate::error::AppError::BadRequest
async fn account_search(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    uri: Uri,
) -> AppResult<Json<Vec<Account>>> {
    auth.require_user_scope(READ_ACCOUNTS)?;
    let params = SearchParams::parse(uri.query())?;
    refuse_anonymous_offset(&auth, &params)?;
    let viewer: Option<&User> = auth.0.as_ref();
    let rows = account_rows(&state, viewer, &params).await?;
    let request = Resolver::new(&params, Arms::AccountsOnly, viewer, &rows);
    let resolved = resolve_query(&state, &request).await;
    Ok(Json(account_entities(
        &state,
        viewer,
        &rows,
        resolved.account(),
    )))
}

/// `GET /api/v1/search` — the combined search document in the v1
/// spelling of its hashtag arm.
///
/// # Errors
///
/// Exactly those of [`account_search`].
async fn search(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    uri: Uri,
) -> AppResult<Json<SearchResults>> {
    search_document(state, auth, uri, Version::V1).await
}

/// `GET /api/v2/search` — the same document, hashtag arm rendered as
/// [`Tag`] entities.
///
/// # Errors
///
/// Exactly those of [`account_search`].
///
/// [`Tag`]: crate::entities::tag::Tag
async fn search_v2(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    uri: Uri,
) -> AppResult<Json<SearchResults>> {
    search_document(state, auth, uri, Version::V2).await
}

/// The combined search both versions answer: scope-gated, parameters
/// parsed, and every arm [`SearchParams::searches`] selects filled with
/// its own search — statuses only for a permitted request, as
/// [`statuses_are_searchable`] reads the ticket's gate — with the account
/// or the post the request resolved (see [`resolve_query`]) riding first in
/// its own list.
///
/// # Errors
///
/// Exactly those of [`account_search`], except that a presented user token
/// must grant `read:search` rather than `read:accounts`.
async fn search_document(
    state: AppState,
    auth: MaybeAuthUser,
    uri: Uri,
    version: Version,
) -> AppResult<Json<SearchResults>> {
    auth.require_user_scope(READ_SEARCH)?;
    let params = SearchParams::parse(uri.query())?;
    refuse_anonymous_offset(&auth, &params)?;
    let viewer: Option<&User> = auth.0.as_ref();
    let rows = if params.searches(Resource::Accounts) {
        account_rows(&state, viewer, &params).await?
    } else {
        Vec::new()
    };
    let request = Resolver::new(&params, Arms::Combined, viewer, &rows);
    let resolved = resolve_query(&state, &request).await;
    let accounts = account_entities(&state, viewer, &rows, resolved.account());
    let statuses = if params.searches(Resource::Statuses) && statuses_are_searchable(&state, viewer)
    {
        find_statuses(&state, viewer, &params, resolved.status()).await?
    } else {
        Vec::new()
    };
    let hashtags = if params.searches(Resource::Hashtags) {
        find_hashtags(&state, &params, version).await?
    } else {
        HashtagList::empty(version)
    };
    Ok(Json(SearchResults {
        accounts,
        statuses,
        hashtags,
    }))
}
