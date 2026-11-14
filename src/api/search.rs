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
//! enforced on a presented user token
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

use std::{future::Future, pin::Pin, time::Duration};

use axum::{Json, Router, extract::State, http::Uri, routing::get};
use serde::Serialize;
use tracing::debug;
use url::{Url, form_urlencoded};
use uuid::Uuid;

use crate::{
    auth::MaybeAuthUser,
    config::Config,
    entities::{account::Account, status::Status as StatusEntity, tag::Tag},
    error::{AppError, AppResult},
    federation::{
        fetch_actor::{FetchError, get_or_fetch_by_acct, get_or_fetch_by_ap_id, is_local_ap_id},
        fetch_object::resolve_status,
        webfinger_client::{resolve_acct_with_base, split_acct, strip_finger_prefixes},
    },
    models::{
        hashtag::{self, Hashtag},
        status::{self, Status as StatusRow},
        status_search, status_visibility,
        user::User,
        user_search::{self, SearchOpts},
    },
    oauth::scopes::READ_ACCOUNTS,
    pagination::PageParams,
    services::render_status::render_statuses,
    state::AppState,
    util::id::parse_id,
};

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
const NO_OFFSET: i64 = 0;

/// 400 body for an `account_id` that is not a valid id — the shared
/// pagination wording.
const INVALID_ID: &str = "Invalid id";

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

/// Which resource of the combined search answer a `type` parameter asks
/// for, and — when it asks for none — whether all of them answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Resource {
    /// No `type`: every searchable resource lists, subject to the
    /// hashtag-only query rule of [`SearchParams::searches`].
    #[default]
    All,
    /// `type=accounts`.
    Accounts,
    /// `type=statuses`.
    Statuses,
    /// `type=hashtags`.
    Hashtags,
    /// Any other spelling: no resource the search knows, so every list
    /// stays empty.
    Other,
}

impl Resource {
    /// Reads a `type` parameter value: the three resource names, each
    /// case-insensitive (as Pleroma's cast downcases it), and anything
    /// else [`Resource::Other`].
    const fn of(text: &str) -> Self {
        if text.eq_ignore_ascii_case(ACCOUNTS_TYPE) {
            Self::Accounts
        } else if text.eq_ignore_ascii_case(STATUSES_TYPE) {
            Self::Statuses
        } else if text.eq_ignore_ascii_case(HASHTAGS_TYPE) {
            Self::Hashtags
        } else {
            Self::Other
        }
    }
}

/// The request parameters of the three search routes, read from the raw
/// query string the way [`PageParams`] reads its own keys (duplicates
/// fold last-wins, unrelated keys are ignored): `q`, `limit`, `offset`,
/// `min_id`, `max_id`, `following`, `resolve`, `account_id` and `type`.
/// The one `limit` sizes two pages: the account listings parse the
/// request's keys with their own default 40 and cap 80, the statuses and
/// hashtags with their own default 20 and cap 40, and each keyset cursor
/// is read into both.
#[derive(Debug)]
struct SearchParams {
    /// The raw `q` parameter; each model's own normalisation does the
    /// folding, and an absent parameter is the empty string, which
    /// answers nothing anywhere.
    q: String,
    /// The accounts arm's page: default [`LIST_LIMIT`], cap [`LIST_CAP`].
    page: PageParams,
    /// The statuses and hashtags arms' page: default [`TEXT_LIST_LIMIT`],
    /// cap [`TEXT_LIST_CAP`].
    text_page: PageParams,
    /// `following`: restrict the account search to the viewer's follows.
    following: bool,
    /// `resolve`: the federation arm's switch, read by [`resolve_query`] —
    /// and still handed to the model, which ignores it.
    resolve: bool,
    /// `type`: which resource lists the combined search fills.
    resource: Resource,
    /// `account_id`: restrict the status search to that one author; an
    /// absent, empty or unusable value narrows nothing.
    account_id: Option<Uuid>,
}

impl SearchParams {
    /// Reads the parameters out of a raw query string.
    ///
    /// # Errors
    ///
    /// [`AppError::BadRequest`](crate::error::AppError) from
    /// [`PageParams`] for a malformed pagination cursor, and the same for
    /// an `account_id` that is present, non-empty, and not a valid id.
    fn parse(query: Option<&str>) -> AppResult<Self> {
        let mut found = Self {
            q: String::new(),
            page: PageParams::from_query_with_default(query, LIST_LIMIT, LIST_CAP)?,
            text_page: PageParams::from_query_with_default(query, TEXT_LIST_LIMIT, TEXT_LIST_CAP)?,
            following: false,
            resolve: false,
            resource: Resource::default(),
            account_id: None,
        };
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            match key.as_ref() {
                "q" => found.q = value.into_owned(),
                "following" => found.following = is_true(&value),
                "resolve" => found.resolve = is_true(&value),
                "type" => found.resource = Resource::of(&value),
                "account_id" => found.account_id = account_id_param(&value)?,
                _ => {}
            }
        }
        Ok(found)
    }

    /// The [`SearchOpts`] the account arm claims: the model's page is the
    /// parsed limit and offset (an absent `offset` is the first page), and
    /// both flags pass through — `resolve` unread until federation.
    fn opts(&self) -> SearchOpts {
        SearchOpts {
            limit: self.page.limit,
            offset: self.page.offset.unwrap_or(NO_OFFSET),
            following_only: self.following,
            resolve: self.resolve,
        }
    }

    /// Whether the query opens with the hashtag sigil: `#tag` (any
    /// leading whitespace included) asks for a tag name, not for text.
    fn is_tag_query(&self) -> bool {
        self.q.trim_start().starts_with('#')
    }

    /// Whether the combined search fills `asked`'s list: an explicit
    /// `type` fills exactly the resource it names (and nothing for an
    /// unknown one); with no `type` every resource lists at once, except
    /// that a `#`-prefixed query is a hashtag term and leaves the other
    /// two arms empty.
    fn searches(&self, asked: Resource) -> bool {
        match self.resource {
            Resource::All => !self.is_tag_query() || asked == Resource::Hashtags,
            Resource::Other => false,
            Resource::Accounts | Resource::Statuses | Resource::Hashtags => self.resource == asked,
        }
    }
}

/// One `account_id` parameter: `None` when absent, empty or blank, the id
/// when it parses, and a 400 [`INVALID_ID`] when it is present and is not
/// a valid id — the shared pagination cursor's rule and wording.
fn account_id_param(text: &str) -> AppResult<Option<Uuid>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    parse_id(text)
        .map(Some)
        .ok_or_else(|| AppError::BadRequest(INVALID_ID.to_owned()))
}

/// Whether a query parameter's text spells a true boolean: `true` or `1`,
/// trimmed and matched case-insensitively — the same reading the directory
/// gives its `local` flag; anything else (absent, empty, `false`, junk)
/// is `false`.
fn is_true(text: &str) -> bool {
    let text = text.trim();
    text.eq_ignore_ascii_case("true") || text == "1"
}

/// The hashtag list of a combined-search answer, in the spelling the
/// answering version renders: v1's plain name strings, v2's [`Tag`]
/// entities with the (search-side uncomputed) `history` key as the empty
/// list.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum HashtagList {
    /// v1: each stored tag by its bare name.
    Names(Vec<String>),
    /// v2: each stored tag as a `name`/`url`/`history` entity.
    Tags(Vec<Tag>),
}

impl HashtagList {
    /// Renders the found `tags` the way `version` spells them, their tag
    /// pages under `base_url`.
    fn render(tags: Vec<Hashtag>, version: Version, base_url: &str) -> Self {
        match version {
            Version::V1 => Self::Names(tags.into_iter().map(|tag| tag.name).collect()),
            Version::V2 => Self::Tags(
                tags.iter()
                    .map(|tag| Tag::with_history(base_url, &tag.name, Vec::new(), None))
                    .collect(),
            ),
        }
    }

    /// The list an arm that was not searched answers: the same empty
    /// array, spelled for `version` so both shapes stay arrays on the
    /// wire.
    fn empty(version: Version) -> Self {
        Self::render(Vec::new(), version, "")
    }
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
async fn account_search(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    uri: Uri,
) -> AppResult<Json<Vec<Account>>> {
    auth.require_user_scope(READ_ACCOUNTS)?;
    let params = SearchParams::parse(uri.query())?;
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
/// Exactly those of [`account_search`].
async fn search_document(
    state: AppState,
    auth: MaybeAuthUser,
    uri: Uri,
    version: Version,
) -> AppResult<Json<SearchResults>> {
    auth.require_user_scope(READ_ACCOUNTS)?;
    let params = SearchParams::parse(uri.query())?;
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

/// Whether a request may search statuses at all: every authenticated
/// viewer's may, an anonymous one only while the operator has turned
/// `SEARCH_STATUSES_UNAUTHENTICATED` on (decision 12's setting). The
/// visibility clause still decides *which* statuses each of them finds.
fn statuses_are_searchable(state: &AppState, viewer: Option<&User>) -> bool {
    viewer.is_some() || state.config.search_statuses_unauthenticated.get()
}

/// The status entities the `statuses` arm answers with: the model's
/// visible, text-matched page — with the post the request resolved, when
/// the viewer may see it, riding first — rendered through the shared batch
/// renderer for the request's viewer (an anonymous render is Pleroma's
/// stranger view, exactly as on the timelines).
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
async fn find_statuses(
    state: &AppState,
    viewer: Option<&User>,
    params: &SearchParams,
    resolved: Option<&StatusRow>,
) -> AppResult<Vec<StatusEntity>> {
    let mut rows = status_search::search_statuses(
        &state.db,
        viewer,
        &params.q,
        params.account_id,
        &params.text_page,
    )
    .await?;
    let mut page: Vec<StatusRow> = Vec::with_capacity(rows.len() + usize::from(resolved.is_some()));
    if let Some(top) = visible_resolution(state, viewer, resolved).await? {
        let id = top.id;
        rows.retain(|row| row.id != id);
        page.push(top);
    }
    page.append(&mut rows);
    let rendered = render_statuses(state, viewer, &page).await?;
    debug!(
        count = rendered.len(),
        author_filtered = params.account_id.is_some(),
        resolved = page.len() > rows.len(),
        "statuses searched"
    );
    Ok(rendered)
}

/// The resolved status this request may answer with: [`can_view`] is the
/// shared gate, so a `private` or `direct` post the viewer has no business
/// reading is left out of the list exactly as the text search would never
/// have found it.
///
/// [`can_view`]: crate::models::status_visibility::can_view
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
async fn visible_resolution(
    state: &AppState,
    viewer: Option<&User>,
    resolved: Option<&StatusRow>,
) -> AppResult<Option<StatusRow>> {
    let Some(row) = resolved else {
        return Ok(None);
    };
    if status_visibility::can_view(&state.db, row, viewer).await? {
        Ok(Some(row.clone()))
    } else {
        debug!("a resolved status the viewer may not see is left out of the answer");
        Ok(None)
    }
}

/// The hashtag arm: names stored with the query as their prefix, in the
/// answering version's spelling.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
async fn find_hashtags(
    state: &AppState,
    params: &SearchParams,
    version: Version,
) -> AppResult<HashtagList> {
    let found = hashtag::search_prefix(&state.db, &params.q, params.text_page.limit).await?;
    debug!(
        count = found.len(),
        version = ?version,
        "hashtags searched"
    );
    Ok(HashtagList::render(found, version, &state.config.base_url))
}

/// The account rows of one search answer, before any resolution and before
/// rendering: the model's ranked, viewer-filtered page for the query.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
async fn account_rows(
    state: &AppState,
    viewer: Option<&User>,
    params: &SearchParams,
) -> AppResult<Vec<User>> {
    let rows = user_search::search_accounts(&state.db, viewer, &params.q, &params.opts()).await?;
    debug!(
        count = rows.len(),
        following = params.following,
        resolve = params.resolve,
        "accounts searched"
    );
    Ok(rows)
}

/// The account entities both search answers run through: `rows` rendered as
/// [`Account`] for the request's viewer (an anonymous request renders
/// Pleroma's stranger view; the listings here are not relationship
/// endpoints, so `pleroma.relationship` stays the empty object), with the
/// account the request resolved riding first — and left out of `rows` when
/// the fetch answered a row that is in the page after all, so one account is
/// never listed twice.
fn account_entities(
    state: &AppState,
    viewer: Option<&User>,
    rows: &[User],
    resolved: Option<&User>,
) -> Vec<Account> {
    let mut accounts: Vec<Account> = rows
        .iter()
        .filter(|row| resolved.is_none_or(|found| row.id != found.id))
        .map(|found| Account::from_user_for(viewer, found, &state.config, None))
        .collect();
    if let Some(found) = resolved {
        accounts.insert(
            0,
            Account::from_user_for(viewer, found, &state.config, None),
        );
    }
    accounts
}

/// Which lists the answering route renders, and so which arm the request's
/// single resolution is allowed to spend itself on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arms {
    /// The combined document: both the `accounts` and the `statuses` lists
    /// answer, so either reading is worth running.
    Combined,
    /// The flat account array: only `accounts` answers, so a status reading
    /// would fetch a post this route has nowhere to put.
    AccountsOnly,
}

impl Arms {
    /// Whether this route renders `asked`'s list.
    fn renders(self, asked: Resource) -> bool {
        asked != Resource::Statuses || matches!(self, Self::Combined)
    }

    /// Whether a request on `params` may spend its one lookup on `asked`:
    /// the route renders that list, and the request's `type` selects it —
    /// `type=accounts` asks for no post, `type=statuses` for no account, and
    /// a `#`-prefixed untyped query asks for neither.
    fn attempts(self, params: &SearchParams, asked: Resource) -> bool {
        self.renders(asked) && params.searches(asked)
    }
}

/// Everything the one resolution of a request decides from: the parsed
/// request, the arms its route renders, the viewer whose credentials make
/// resolution possible at all, and the account rows the stored search has
/// already answered.
#[derive(Debug)]
struct Resolver<'a> {
    /// The parsed request parameters.
    params: &'a SearchParams,
    /// Which lists the answering route renders.
    arms: Arms,
    /// The requesting viewer: `None` anonymously, which resolves nothing.
    viewer: Option<&'a User>,
    /// The rows [`account_rows`] answered for this request, in its ranking:
    /// the exact-match test reads them to keep a handle this instance
    /// already stores off the network.
    rows: &'a [User],
    /// The origin a mock `WebFinger` peer answers on, and `None` everywhere
    /// else: `WebFinger` is HTTPS-only by RFC 7033, so only the in-crate
    /// tests can point it at loopback (see
    /// [`crate::services::mentions::resolve_mentions_with_base`], the same
    /// seam).
    webfinger_base: Option<&'a str>,
}

impl<'a> Resolver<'a> {
    /// The resolver a handler runs, against the real `WebFinger`.
    const fn new(
        params: &'a SearchParams,
        arms: Arms,
        viewer: Option<&'a User>,
        rows: &'a [User],
    ) -> Self {
        Self {
            params,
            arms,
            viewer,
            rows,
            webfinger_base: None,
        }
    }

    /// Whether this request asks for resolution at all: `resolve=true`, from
    /// an authenticated viewer. Resolution is a federation action on
    /// somebody's behalf, so an anonymous caller keeps the ordinary stored
    /// search's answer (Pleroma's `maybe_resolve` resolves for a user, too).
    const fn resolves(&self) -> bool {
        self.params.resolve && self.viewer.is_some()
    }

    /// Whether this request may spend its one lookup on `asked`'s arm.
    fn attempts(&self, asked: Resource) -> bool {
        self.arms.attempts(self.params, asked)
    }
}

/// What the one lookup of a resolving search produced. The payloads are
/// boxed: a [`User`] or a [`StatusRow`] outgrows the nothing-resolved arm by
/// an order of magnitude.
#[derive(Debug)]
enum Resolution {
    /// Nothing resolved: no lookup ran, or every attempt failed quietly.
    Nothing,
    /// The account to answer first in `accounts`.
    Account(Box<User>),
    /// The status row to answer first in `statuses`.
    Status(Box<StatusRow>),
}

impl Resolution {
    /// The resolved account, when that is what the lookup found.
    fn account(&self) -> Option<&User> {
        match self {
            Self::Account(found) => Some(found.as_ref()),
            Self::Nothing | Self::Status(_) => None,
        }
    }

    /// The resolved status row, when that is what the lookup found.
    fn status(&self) -> Option<&StatusRow> {
        match self {
            Self::Status(found) => Some(found.as_ref()),
            Self::Nothing | Self::Account(_) => None,
        }
    }
}

/// The request's one remote lookup, silent on every failure. The query's
/// shape decides: an `http(s)` URL goes to [`resolve_url`], and any other
/// query that spells a `nick@domain` handle goes to [`resolve_handle`].
/// Anything else — a word, a `#tag`, a query in no scheme federation speaks
/// — resolves nothing, and the answer is the stored search's alone.
async fn resolve_query(state: &AppState, request: &Resolver<'_>) -> Resolution {
    if !request.resolves() {
        return Resolution::Nothing;
    }
    let query = request.params.q.trim();
    match web_url(query) {
        Some(url) => resolve_url(state, request, &url).await,
        None => resolve_handle(state, request, query).await,
    }
}

/// The URL arm: the actor reading first, the status reading second, and the
/// request stops at the first thing it finds. A status URL run through the
/// actor reading fails there — a `Note` document is no actor — which is what
/// lets one query name either kind of remote object (the ticket's "try, and
/// fall through on failure"), while `type` still keeps either reading from
/// being attempted at all.
async fn resolve_url(state: &AppState, request: &Resolver<'_>, url: &Url) -> Resolution {
    if request.attempts(Resource::Accounts)
        && let Some(found) = fetch_actor_url(state, url).await
    {
        return Resolution::Account(Box::new(found));
    }
    if request.attempts(Resource::Statuses)
        && let Some(found) = fetch_status_url(state, url).await
    {
        return Resolution::Status(Box::new(found));
    }
    Resolution::Nothing
}

/// The handle arm: a `nick@domain` query (the leading `@` optional) that the
/// stored search matched no row for exactly, looked up through
/// [`get_or_fetch_by_acct`]. A handle this client would refuse to finger is
/// no handle at all for the search either — [`split_acct`] is what says so —
/// and a query the search already matched nickname for nickname needs no
/// lookup, because the row the fetch would return is already in the page.
async fn resolve_handle(state: &AppState, request: &Resolver<'_>, query: &str) -> Resolution {
    if !request.attempts(Resource::Accounts)
        || split_acct(query).is_none()
        || matched_exact(request.rows, query)
    {
        return Resolution::Nothing;
    }
    fetch_handle(state, request.webfinger_base, query)
        .await
        .map_or(Resolution::Nothing, |found| {
            Resolution::Account(Box::new(found))
        })
}

/// One resolution's remote lookup, pinned to a pointer: [`within_budget`]
/// waits on it, and a boxed future keeps the waiting search's own state
/// machine small however much state the federation dereference inside it
/// carries (the shape [`crate::jobs`] hands its job handlers, for the same
/// reason).
type Lookup<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The handle lookup of a resolving search: [`get_or_fetch_by_acct`] —
/// `WebFinger`, then the actor dereference, then the vouch check — under
/// [`RESOLVE_BUDGET`], every failure (and every clock that runs out) answering
/// `None` with a `debug` line, because the ticket's unresolved arm is an
/// empty list, not an error.
async fn fetch_handle(state: &AppState, webfinger_base: Option<&str>, acct: &str) -> Option<User> {
    within_budget(Box::pin(async move {
        match lookup_handle(state, webfinger_base, acct).await {
            Ok(found) => Some(found),
            Err(err) => {
                debug!(%err, %acct, "a search could not resolve the handle");
                None
            }
        }
    }))
    .await
}

/// The one handle lookup behind [`fetch_handle`]. The production arm is
/// [`get_or_fetch_by_acct`] entire; the `Some(base)` arm runs the same
/// `WebFinger`-then-actor round trip against a mock peer at `base` and
/// skips only the vouch check `get_or_fetch_by_acct` closes it with — a mock
/// answering on loopback can never name the dotted domain of a handle
/// `WebFinger` accepts, so the check would reject every mock success. The
/// same seam [`crate::services::mentions::resolve_mentions_with_base`]
/// keeps, for the same reason.
async fn lookup_handle(
    state: &AppState,
    webfinger_base: Option<&str>,
    acct: &str,
) -> Result<User, FetchError> {
    match webfinger_base {
        None => get_or_fetch_by_acct(state, acct).await,
        Some(base) => {
            let ap_id = resolve_acct_with_base(&state.http_client, acct, Some(base)).await?;
            get_or_fetch_by_ap_id(state, &ap_id).await
        }
    }
}

/// The actor reading of a URL query: [`get_or_fetch_by_ap_id`] under
/// [`RESOLVE_BUDGET`]. It answers a stored actor or a local one from the
/// database — an unknown id on this instance's own origin is refused without
/// any network at all — and mirrors a remote one it has never seen.
async fn fetch_actor_url(state: &AppState, url: &Url) -> Option<User> {
    within_budget(Box::pin(async move {
        match get_or_fetch_by_ap_id(state, url.as_str()).await {
            Ok(found) => Some(found),
            Err(err) => {
                debug!(%err, "a search read a URL as an actor id and found no account");
                None
            }
        }
    }))
    .await
}

/// The status reading of a URL query: this instance's own perma-link is a
/// database read, and [`resolve_status`] does the rest — it answers a stored
/// row (its `/objects/{uuid}` shape and its `/notice/{uuid}` alias alike)
/// and nothing at all for an unknown local id, so the only local form left
/// out is the perma-link the resolver stores no uri under.
async fn fetch_status_url(state: &AppState, url: &Url) -> Option<StatusRow> {
    if let Some(id) = local_permalink_id(&state.config, url) {
        let found = status::get_by_id(&state.db, id).await.ok().flatten();
        debug!(
            %id,
            stored = found.is_some(),
            "a local status perma-link resolved from the database"
        );
        return found;
    }
    within_budget(Box::pin(async move {
        match resolve_status(state, url.as_str()).await {
            Ok(found) => found,
            Err(err) => {
                debug!(%err, "a search read a URL as a status and found no post");
                None
            }
        }
    }))
    .await
}

/// The query as a URL federation can act on: `http` or `https` only —
/// anything else (a word, a handle, an `ftp:` or `mailto:` address) names no
/// remote document, and is [`None`].
fn web_url(query: &str) -> Option<Url> {
    let parsed = Url::parse(query).ok()?;
    matches!(parsed.scheme(), "http" | "https").then_some(parsed)
}

/// Whether the stored search already answered with the account `query`
/// names: a row whose nickname *is* the query, case folded and its leading
/// `@` (or `acct:`) folded away. Such a query needs no lookup — the account
/// the fetch would return is already in the page.
fn matched_exact(rows: &[User], query: &str) -> bool {
    let handle = strip_finger_prefixes(query.trim());
    rows.iter()
        .any(|row| row.nickname.eq_ignore_ascii_case(handle))
}

/// The id a local status perma-link names: `{BASE_URL}/users/{nickname}/
/// statuses/{id}` on this instance's own origin, with `id` a stored-shaped
/// id ([`parse_id`]). The nickname segment is the client's slug and decides
/// nothing — the id is the row. Every other URL is [`None`], including a
/// local URL of another shape, which the resolver's own local arms settle.
fn local_permalink_id(config: &Config, url: &Url) -> Option<Uuid> {
    if !is_local_ap_id(config, url.as_str()) {
        return None;
    }
    let mut segments = url.path().trim_start_matches('/').split('/');
    if segments.next() != Some(USERS_SEGMENT) {
        return None;
    }
    if segments.next().is_none_or(str::is_empty) {
        return None;
    }
    if segments.next() != Some(STATUSES_SEGMENT) {
        return None;
    }
    let id = segments.next().filter(|id| !id.is_empty())?;
    if segments.next().is_some() {
        return None;
    }
    parse_id(id)
}

/// Runs one resolution `future` under [`RESOLVE_BUDGET`], answering whatever
/// it answers or [`None`] when the budget runs out first. A lookup cut off
/// mid-flight is simply dropped: the ticket's silent failure, and the same
/// posture a remote mention lookup takes (see [`crate::services::ingest`]).
///
/// The future arrives boxed for the same reason [`crate::jobs`] boxes its
/// handler futures: a federation dereference is kilobytes of state machine,
/// and the search's own future holds a pointer to it instead.
async fn within_budget<T>(future: Lookup<'_, Option<T>>) -> Option<T> {
    tokio::time::timeout(RESOLVE_BUDGET, future)
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use axum::{
        extract::{Path, Query},
        http::header::CONTENT_TYPE,
        response::{IntoResponse as _, Response},
    };
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        http_client::HttpClient,
        models::user::{NewUser, get_by_ap_id, insert, local_ap_id},
        uploads::UploaderBackend,
    };

    /// The parsed parameters of `query`.
    ///
    /// # Panics
    ///
    /// If the fixed test query fails to parse (a malformed cursor a test
    /// names as such never reaches here).
    fn parse(query: &str) -> SearchParams {
        SearchParams::parse(Some(query)).expect("the fixed test query parses")
    }

    /// A stored tag row named `name`, for the render tests (the id and the
    /// creation time are what the entity never reads).
    fn tag(name: &str) -> Hashtag {
        Hashtag {
            id: Uuid::now_v7(),
            name: name.to_owned(),
            inserted_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn the_query_and_flags_are_read_last_wins() {
        let plain = parse("q=ali&following=false&resolve=false");
        assert_eq!(plain.q, "ali", "a plain query");
        assert!(!plain.following, "and its explicit falses");
        assert!(!plain.resolve);

        let repeated = parse("q=first&q=second&following=1&resolve=TRUE");
        assert_eq!(repeated.q, "second", "the last occurrence wins");
        assert!(repeated.following, "`1` is true");
        assert!(repeated.resolve, "`TRUE` is true");

        let decoded = parse("q=%40bob%20x&following=true");
        assert_eq!(decoded.q, "@bob x", "the value is percent-decoded");
        assert!(decoded.following);
    }

    #[test]
    fn everything_else_is_absent_and_false() {
        let bare = parse("other=1");
        assert_eq!(bare.q, "", "no q is the empty query, which answers nothing");
        assert!(!bare.following, "no flag is false");
        assert_eq!(bare.page.limit, LIST_LIMIT, "and the accounts' own page");
        assert_eq!(
            bare.text_page.limit, TEXT_LIST_LIMIT,
            "and the statuses' and hashtags' own page"
        );
        assert_eq!(bare.page.offset, None, "no offset means the first page");
        assert_eq!(bare.account_id, None, "no author narrows the status search");

        let junk = parse("q=bob&following=yes&resolve=0");
        assert!(!junk.following, "junk is not a true");
        assert!(!junk.resolve);

        let clamped = parse("q=bob&limit=900&offset=40&following=true");
        assert_eq!(
            clamped.page.limit, LIST_CAP,
            "the cap clamps the account page"
        );
        assert_eq!(
            clamped.text_page.limit, TEXT_LIST_CAP,
            "and the other page caps at its own 40"
        );
        let built = clamped.opts();
        assert_eq!(built.limit, LIST_CAP, "limit passes through clamped");
        assert_eq!(built.offset, 40, "offset passes through");
        assert!(built.following_only, "`following` becomes the restriction");
        assert!(!built.resolve, "and resolve stays what was not sent");
    }

    #[test]
    fn the_page_is_the_listings_own_default_and_cap() {
        for (query, want) in [
            ("", LIST_LIMIT),
            ("limit=7", 7),
            ("limit=80", LIST_CAP),
            ("limit=200", LIST_CAP),
            ("limit=0", LIST_LIMIT),
            ("limit=nope", LIST_LIMIT),
        ] {
            let sized = parse(query);
            assert_eq!(
                sized.page.limit, want,
                "`{query:?}` pages accounts at {want}"
            );
        }
        for (query, want) in [
            ("", TEXT_LIST_LIMIT),
            ("limit=7", 7),
            ("limit=40", TEXT_LIST_CAP),
            ("limit=200", TEXT_LIST_CAP),
            ("limit=0", TEXT_LIST_LIMIT),
            ("limit=nope", TEXT_LIST_LIMIT),
        ] {
            let sized = parse(query);
            assert_eq!(
                sized.text_page.limit, want,
                "`{query:?}` pages statuses and hashtags at {want}"
            );
        }
        assert_eq!(
            parse("offset=3").page.offset,
            Some(3),
            "a real offset reads"
        );
        assert_eq!(
            parse("offset=3").text_page.offset,
            Some(3),
            "and rides into both pages"
        );
        assert_eq!(
            parse("offset=x").page.offset,
            None,
            "and junk means the first page"
        );
    }

    #[test]
    fn a_malformed_cursor_is_a_bad_request() {
        let err = SearchParams::parse(Some("max_id=nope")).expect_err("a bad cursor is refused");
        assert_eq!(
            err.to_string(),
            "Invalid id",
            "the shared pagination wording"
        );
    }

    #[test]
    fn the_author_id_reads_an_id_or_refuses_junk() {
        let id = Uuid::now_v7();
        assert_eq!(parse(&format!("account_id={id}")).account_id, Some(id));
        assert_eq!(
            parse(&format!("account_id={}", id.simple())).account_id,
            Some(id),
            "the hyphenless spelling parses too"
        );
        assert_eq!(
            parse("account_id=").account_id,
            None,
            "an empty author filter narrows nothing"
        );
        assert_eq!(
            parse("other=1").account_id,
            None,
            "and an absent one is absent"
        );
        let err = SearchParams::parse(Some("account_id=nope")).expect_err("junk is refused");
        assert_eq!(err.to_string(), INVALID_ID, "the shared wording");
    }

    #[test]
    fn the_three_resources_and_anything_else_are_read() {
        assert_eq!(Resource::of("accounts"), Resource::Accounts);
        assert_eq!(Resource::of("STATUSES"), Resource::Statuses, "case folds");
        assert_eq!(Resource::of("Hashtags"), Resource::Hashtags);
        for other in ["bonked", ""] {
            assert_eq!(
                Resource::of(other),
                Resource::Other,
                "`{other}` is no resource the search knows"
            );
        }
        assert_eq!(
            Resource::default(),
            Resource::All,
            "no type at all searches every resource, like Mastodon's default"
        );

        let untyped = parse("q=bob");
        assert_eq!(untyped.resource, Resource::All, "absent type");
        for (query, want) in [
            ("q=bob&type=statuses", Resource::Statuses),
            ("q=bob&type=hashtags", Resource::Hashtags),
            ("q=bob&type=accounts", Resource::Accounts),
            ("q=bob&type=potato", Resource::Other),
        ] {
            assert_eq!(parse(query).resource, want, "`{query}`");
        }
    }

    #[test]
    fn type_selects_one_arm_and_no_type_selects_all() {
        /// The three arms a selection judgement is made about.
        const ALL_ARMS: [Resource; 3] =
            [Resource::Accounts, Resource::Statuses, Resource::Hashtags];

        let untyped = parse("q=bob");
        for asked in ALL_ARMS {
            assert!(untyped.searches(asked), "no type searches {asked:?}");
        }

        for (query, filled) in [
            ("type=accounts", Resource::Accounts),
            ("type=statuses", Resource::Statuses),
            ("type=hashtags", Resource::Hashtags),
        ] {
            let chosen = parse(&format!("q=bob&{query}"));
            assert!(chosen.searches(filled), "`{query}` fills its own arm");
            let others = ALL_ARMS.into_iter().filter(|arm| *arm != filled);
            for other in others {
                assert!(
                    !chosen.searches(other),
                    "`{query}` leaves the {other:?} arm empty"
                );
            }
        }

        let unknown = parse("q=bob&type=potato");
        for asked in ALL_ARMS {
            assert!(
                !unknown.searches(asked),
                "an unknown type fills nothing, {asked:?} included"
            );
        }
    }

    #[test]
    fn a_leading_hash_makes_an_untyped_query_hashtag_only() {
        let tagged = parse("q=%23tag");
        assert!(tagged.is_tag_query(), "`#tag` opens with the sigil");
        assert!(
            tagged.searches(Resource::Hashtags),
            "the hashtag arm answers it"
        );
        assert!(
            !tagged.searches(Resource::Accounts) && !tagged.searches(Resource::Statuses),
            "and it is a hashtag-only search term: no accounts, no statuses"
        );

        let spaced = parse("q=+%23tag");
        assert!(spaced.is_tag_query(), "leading whitespace does not hide it");
        let mid = parse("q=super #tag");
        assert!(
            !mid.is_tag_query(),
            "only a query that opens with `#` is one"
        );
        let forced = parse("q=%23tag&type=statuses");
        assert!(
            forced.searches(Resource::Statuses),
            "an explicit type searches its resource whatever the query's shape"
        );
        assert!(
            !forced.searches(Resource::Hashtags),
            "and still fills exactly that one arm"
        );
    }

    #[test]
    fn only_the_true_spellings_of_a_flag_are_true() {
        for text in ["true", "TRUE", " true ", "1"] {
            assert!(is_true(text), "`{text}` asks for the restriction");
        }
        for text in ["", "false", "0", "yes", "2", " true and then some "] {
            assert!(!is_true(text), "`{text}` is not a true flag");
        }
    }

    #[test]
    fn the_hashtag_list_is_names_on_v1_and_tag_entities_on_v2() {
        let tags = vec![tag("rust"), tag("ruby")];

        let v1: Value = serde_json::to_value(HashtagList::render(
            tags.clone(),
            Version::V1,
            TEST_BASE_URL,
        ))
        .expect("the v1 list serializes");
        assert_eq!(v1, json!(["rust", "ruby"]), "v1 spells plain names");

        let v2: Value = serde_json::to_value(HashtagList::render(tags, Version::V2, TEST_BASE_URL))
            .expect("the v2 list serializes");
        assert_eq!(
            v2,
            json!([
                { "name": "rust", "url": format!("{TEST_BASE_URL}/tags/rust"), "history": [] },
                { "name": "ruby", "url": format!("{TEST_BASE_URL}/tags/ruby"), "history": [] },
            ]),
            "v2 renders tag entities whose uncomputed history is the empty list"
        );

        for version in [Version::V1, Version::V2] {
            let empty: Value = serde_json::to_value(HashtagList::empty(version))
                .expect("the empty list serializes");
            assert_eq!(empty, json!([]), "an unsearched arm is still an array");
        }
    }

    /// `User-Agent` for the mock-reachable client.
    const TEST_UA: &str = "Wired (https://test.example)";

    /// Test state over `pool`: the fixed test config with a client whose SSRF
    /// guard is off, so the handle lookup may reach the loopback mock.
    fn test_state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::new_for_tests(TEST_UA),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// A local user named `nickname`, inserted straight.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the fixture local user inserts")
    }

    /// A stored remote mirror of `nickname` on `host`, the shape decision 7
    /// keeps: the nickname carries the whole handle.
    async fn mirror_user(pool: &PgPool, nickname: &str, host: &str) -> User {
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@{host}"),
                ap_id: format!("https://{host}/users/{nickname}"),
                domain: Some(host.to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the fixture mirror inserts")
    }

    /// The mock remote peer: its `http://` origin beside the per-endpoint
    /// request counts the tests assert on.
    #[derive(Clone, Debug)]
    struct Mock {
        /// The peer's origin.
        base: String,
        /// `WebFinger` requests it received.
        webfinger_hits: Arc<AtomicUsize>,
        /// Actor dereferences it received.
        actor_hits: Arc<AtomicUsize>,
    }

    /// Serves `WebFinger` and one actor document per nickname on a loopback
    /// port; the returned task is aborted by the test once its assertions are
    /// in.
    async fn mock_peer() -> (Mock, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock peer binds a port");
        let mock = Mock {
            base: format!("http://{}", listener.local_addr().expect("a bound address")),
            webfinger_hits: Arc::new(AtomicUsize::new(0)),
            actor_hits: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/.well-known/webfinger", get(mock_webfinger))
            .route("/users/{nick}", get(mock_actor))
            .with_state(mock.clone());
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock peer stopped: {err}");
            }
        });
        (mock, server)
    }

    /// [`mock_peer`]'s `WebFinger` arm: a `JRD` whose self link points at this
    /// peer's own `/users/{nick}`, under a subject that vouches for the
    /// `{nick}@mock.example` handle the resolver was asked about.
    async fn mock_webfinger(
        State(mock): State<Mock>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Response {
        let _hits = mock.webfinger_hits.fetch_add(1, Ordering::SeqCst);
        let nickname = params
            .get("resource")
            .and_then(|resource| resource.strip_prefix("acct:"))
            .and_then(|handle| handle.split_once('@').map(|(nick, _)| nick))
            .unwrap_or_else(|| panic!("the mock was fingered for no acct: {params:?}"));
        let jrd = json!({
            "subject": format!("acct:{nickname}@mock.example"),
            "links": [{
                "rel": "self",
                "type": "application/activity+json",
                "href": format!("{}/users/{nickname}", mock.base),
            }],
        });
        ([(CONTENT_TYPE, "application/jrd+json")], jrd.to_string()).into_response()
    }

    /// [`mock_peer`]'s actor arm: the self-identifying `Person` document
    /// [`parse_actor`] requires.
    ///
    /// [`parse_actor`]: crate::federation::remote_actor::parse_actor
    async fn mock_actor(State(mock): State<Mock>, Path(nick): Path<String>) -> Response {
        let _hits = mock.actor_hits.fetch_add(1, Ordering::SeqCst);
        let id = format!("{}/users/{nick}", mock.base);
        let document = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "type": "Person",
            "id": id,
            "preferredUsername": nick,
            "name": "Mock",
            "inbox": format!("{id}/inbox"),
            "publicKey": {
                "id": format!("{id}#main-key"),
                "owner": id,
                "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
            },
        });
        (
            [(CONTENT_TYPE, "application/activity+json")],
            document.to_string(),
        )
            .into_response()
    }

    /// The origin of a port nothing listens on any more: every request to it
    /// fails to connect, which is how an unreachable peer reads here without
    /// waiting on any network or timer.
    async fn dead_base() -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port is free to bind");
        let base = format!("http://{}", listener.local_addr().expect("a bound address"));
        drop(listener);
        base
    }

    /// The hits one mock counter has recorded.
    fn hits(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::SeqCst)
    }

    /// The one lookup a resolving combined search runs for `query`: `viewer`
    /// asking, `rows` already answered by the stored search, and `base` the
    /// mock `WebFinger` origin to point the handle arm at (`None` asks the
    /// real one, which no loopback mock answers).
    async fn resolution(
        state: &AppState,
        viewer: Option<&User>,
        query: &str,
        rows: &[User],
        base: Option<&str>,
    ) -> Resolution {
        let params = parse(query);
        let request = Resolver {
            webfinger_base: base,
            ..Resolver::new(&params, Arms::Combined, viewer, rows)
        };
        resolve_query(state, &request).await
    }

    /// A handle the stored search answered nothing for is fetched through
    /// `WebFinger`, mirrored into `users`, and rides the account list first;
    /// the next search for the same handle is served by that mirror, so the
    /// actor document is never dereferenced twice.
    #[tokio::test]
    async fn a_resolved_handle_rides_first_and_is_stored_for_later() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let (mock, server) = mock_peer().await;
        let viewer = local_user(&pool, "viewer").await;
        let follower = local_user(&pool, "alfred").await;

        let found = resolution(
            &state,
            Some(&viewer),
            "q=alice@mock.example&resolve=true",
            &[],
            Some(&mock.base),
        )
        .await;
        assert!(
            found.account().is_some() && found.status().is_none(),
            "the account arm is what answered"
        );
        let Resolution::Account(account) = found else {
            panic!("the handle resolved to an account: {found:?}");
        };
        assert_eq!(
            account.ap_id,
            format!("{}/users/alice", mock.base),
            "the actor the mock's WebFinger named"
        );
        assert!(!account.local.get(), "and a remote mirror, not a local row");
        let stored = get_by_ap_id(&pool, &account.ap_id)
            .await
            .expect("the mirror read")
            .expect("the fetched actor is stored for later");
        assert_eq!(stored.id, account.id, "the answer is the stored row");

        let follower_id = follower.id.to_string();
        let listed = account_entities(&state, Some(&viewer), &[follower], Some(&account));
        let ids: Vec<String> = listed.iter().map(|entry| entry.id.clone()).collect();
        assert_eq!(
            ids,
            vec![account.id.to_string(), follower_id],
            "the resolved account comes first, the stored match follows"
        );

        let again = resolution(
            &state,
            Some(&viewer),
            "q=alice@mock.example&resolve=true",
            &[],
            Some(&mock.base),
        )
        .await;
        assert!(
            matches!(again, Resolution::Account(_)),
            "the mirror answers the next search too"
        );
        assert_eq!(
            (hits(&mock.webfinger_hits), hits(&mock.actor_hits)),
            (2, 1),
            "one actor dereference for the two searches: the second read the mirror"
        );
        server.abort();
    }

    /// A handle the stored search matched nickname for nickname runs no
    /// lookup at all: the account the fetch would return is already in the
    /// page, and the peer never hears about the request.
    #[tokio::test]
    async fn an_exact_handle_match_resolves_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let (mock, server) = mock_peer().await;
        let viewer = local_user(&pool, "viewer").await;
        let eve = mirror_user(&pool, "eve", "remote.example").await;
        assert!(
            matched_exact(std::slice::from_ref(&eve), "@Eve@remote.example"),
            "the stored nickname is an exact match for the typed handle"
        );

        let found = resolution(
            &state,
            Some(&viewer),
            "q=%40eve%40remote.example&resolve=true",
            &[eve],
            Some(&mock.base),
        )
        .await;
        assert!(
            matches!(found, Resolution::Nothing),
            "the exact match is the answer already: {found:?}"
        );
        assert_eq!(
            (hits(&mock.webfinger_hits), hits(&mock.actor_hits)),
            (0, 0),
            "and no lookup ran"
        );
        server.abort();
    }

    /// The peer behind a handle cannot be reached: the arm answers nothing,
    /// quietly — no error, no partial row, and the account list stays as the
    /// stored search left it.
    #[tokio::test]
    async fn an_unreachable_handle_resolves_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let viewer = local_user(&pool, "viewer").await;
        let base = dead_base().await;

        let found = resolution(
            &state,
            Some(&viewer),
            "q=alice@mock.example&resolve=true",
            &[],
            Some(&base),
        )
        .await;
        assert!(
            matches!(found, Resolution::Nothing),
            "an unreachable peer is the empty arm: {found:?}"
        );
    }

    /// Resolution needs both the flag and a viewer: `resolve=false` and an
    /// anonymous `resolve=true` are the ordinary stored search, and the mock
    /// peer sees neither request.
    #[tokio::test]
    async fn resolution_needs_the_flag_and_a_viewer() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let (mock, server) = mock_peer().await;
        let viewer = local_user(&pool, "viewer").await;

        for (who, query) in [
            ("no flag", "q=alice@mock.example"),
            ("an anonymous caller", "q=alice@mock.example&resolve=true"),
            ("junk that is not a handle", "q=alice&resolve=true"),
            ("a tag query", "q=%23alice&resolve=true"),
        ] {
            let asked = match who {
                "an anonymous caller" => None,
                _ => Some(&viewer),
            };
            let found = resolution(&state, asked, query, &[], Some(&mock.base)).await;
            assert!(
                matches!(found, Resolution::Nothing),
                "{who} resolves nothing: {found:?}"
            );
        }
        assert_eq!(
            (hits(&mock.webfinger_hits), hits(&mock.actor_hits)),
            (0, 0),
            "and none of them reached the network"
        );
        server.abort();
    }

    /// `type` narrows the readings to the resource it names: a handle is
    /// never fetched for `type=statuses`, and the flat account search never
    /// spends its one lookup on a post it has no list for.
    #[tokio::test]
    async fn the_requested_type_narrows_what_resolves() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let (mock, server) = mock_peer().await;
        let viewer = local_user(&pool, "viewer").await;

        let found = resolution(
            &state,
            Some(&viewer),
            "q=alice@mock.example&resolve=true&type=statuses",
            &[],
            Some(&mock.base),
        )
        .await;
        assert!(
            matches!(found, Resolution::Nothing),
            "`type=statuses` asks for no account: {found:?}"
        );
        assert_eq!(hits(&mock.webfinger_hits), 0, "and no lookup ran");

        let untyped = parse("q=bob");
        assert!(
            Arms::Combined.attempts(&untyped, Resource::Statuses),
            "the combined document can answer a resolved post"
        );
        assert!(
            !Arms::AccountsOnly.attempts(&untyped, Resource::Statuses),
            "the flat account array cannot, so it never fetches one"
        );
        for asked in [Resource::Accounts, Resource::Hashtags] {
            assert!(
                Arms::AccountsOnly.attempts(&untyped, asked),
                "the accounts arm is still there for {asked:?}"
            );
        }
        let only_tags = parse("q=%23bob");
        for asked in [Resource::Accounts, Resource::Statuses] {
            assert!(
                !Arms::Combined.attempts(&only_tags, asked),
                "a `#`-query resolves no {asked:?} either"
            );
        }
        server.abort();
    }

    #[test]
    fn only_an_http_url_is_read_as_an_address() {
        for query in [
            "https://remote.example/users/zoe",
            "http://127.0.0.1:8080/a",
        ] {
            let url = web_url(query).expect("the address parses as a web URL");
            assert!(matches!(url.scheme(), "http" | "https"), "{query} is web");
        }
        for query in [
            "alice@remote.example",
            "@alice",
            "ftp://remote.example/pub",
            "mailto:alice@remote.example",
            "//remote.example/users/zoe",
            "",
        ] {
            assert!(web_url(query).is_none(), "`{query}` names no document");
        }
    }

    #[test]
    fn a_local_permalink_is_read_by_id_and_nothing_else() {
        let config = Config::for_tests();
        let id = Uuid::now_v7();
        for (path, want) in [
            (format!("/users/bob/statuses/{id}"), Some(id)),
            (format!("/users/bob/statuses/{}", id.simple()), Some(id)),
            (format!("/users/bob/statuses/{id}/replies"), None),
            ("/users/bob/statuses/nope".to_owned(), None),
            (format!("/users//statuses/{id}"), None),
            ("/users/bob/statuses".to_owned(), None),
            ("/users/bob".to_owned(), None),
            (format!("/notice/{id}"), None),
            (format!("/objects/{id}"), None),
        ] {
            let url = Url::parse(&format!("{TEST_BASE_URL}{path}")).expect("the fixture URL");
            assert_eq!(
                local_permalink_id(&config, &url),
                want,
                "`{path}` is no perma-link of ours"
            );
        }
        let elsewhere = Url::parse(&format!("http://127.0.0.1:9999/users/bob/statuses/{id}"))
            .expect("the foreign URL");
        assert_eq!(
            local_permalink_id(&config, &elsewhere),
            None,
            "another origin's perma-link is not ours to read from the database"
        );
    }
}
