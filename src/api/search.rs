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
//! term, not text, and fills only the hashtag list. `resolve` still rides
//! along to the model, which ignores it until the federation ticket gives
//! it `WebFinger`. The two versions answer the same document until v1's
//! hashtag arm differs: v1 spells hashtag entries as plain name strings
//! and v2 as [`Tag`] entities whose (uncomputed) `history` renders as the
//! empty list.
//!
//! All three routes run in the limiter's `search` bucket (Pleroma
//! throttles the three search actions there; see [`crate::rate_limit`]).

use axum::{Json, Router, extract::State, http::Uri, routing::get};
use serde::Serialize;
use tracing::debug;
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    auth::MaybeAuthUser,
    entities::{account::Account, status::Status as StatusEntity, tag::Tag},
    error::{AppError, AppResult},
    models::{
        hashtag::{self, Hashtag},
        status_search,
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
    /// `resolve`: handed to the model, which ignores it (yet).
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
/// spelling, which Pleroma renders through its `AccountView`).
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
    Ok(Json(search_accounts(&state, &auth, &params).await?))
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
/// [`statuses_are_searchable`] reads the ticket's gate.
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
    let accounts = if params.searches(Resource::Accounts) {
        search_accounts(&state, &auth, &params).await?
    } else {
        Vec::new()
    };
    let statuses = if params.searches(Resource::Statuses) && statuses_are_searchable(&state, viewer)
    {
        find_statuses(&state, viewer, &params).await?
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
/// visible, text-matched page, rendered through the shared batch renderer
/// for the request's viewer (an anonymous render is Pleroma's stranger
/// view, exactly as on the timelines).
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
async fn find_statuses(
    state: &AppState,
    viewer: Option<&User>,
    params: &SearchParams,
) -> AppResult<Vec<StatusEntity>> {
    let rows = status_search::search_statuses(
        &state.db,
        viewer,
        &params.q,
        params.account_id,
        &params.text_page,
    )
    .await?;
    let rendered = render_statuses(state, viewer, &rows).await?;
    debug!(
        count = rendered.len(),
        author_filtered = params.account_id.is_some(),
        "statuses searched"
    );
    Ok(rendered)
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

/// The account entities both search answers run through: the model's
/// ranked, viewer-filtered rows rendered as [`Account`] for the request's
/// viewer (an anonymous request renders Pleroma's stranger view; the
/// listings here are not relationship endpoints, so `pleroma.relationship`
/// stays the empty object).
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
async fn search_accounts(
    state: &AppState,
    auth: &MaybeAuthUser,
    params: &SearchParams,
) -> AppResult<Vec<Account>> {
    let viewer: Option<&User> = auth.0.as_ref();
    let rows = user_search::search_accounts(&state.db, viewer, &params.q, &params.opts()).await?;
    debug!(
        count = rows.len(),
        following = params.following,
        resolve = params.resolve,
        "accounts searched"
    );
    Ok(rows
        .iter()
        .map(|found| Account::from_user_for(viewer, found, &state.config, None))
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::config::TEST_BASE_URL;

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
}
