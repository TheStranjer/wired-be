//! The account-search endpoints: `GET /api/v1/accounts/search` and the
//! `q`-keyed `GET /api/v1/search` / `GET /api/v2/search`
//! (`docs/ARCHITECTURE.md` decision 1).
//!
//! Reference: Pleroma's `SearchController.account_search`, `.search` and
//! `.search2` (`lib/pleroma/web/mastodon_api/controllers/search_controller.ex`)
//! over `User.Search.search` (`lib/pleroma/user/search.ex`). All three
//! actions are this module; the query itself — normalisation, the `ILIKE`
//! prefix and `pg_trgm` trigram matching, the exact `nick@domain` handle,
//! the ranking and the per-viewer exclusions — is the model's, in
//! [`crate::models::user_search`].
//!
//! Authentication is [`MaybeAuthUser`] with the `read:accounts` scope
//! enforced on a presented user token
//! ([`require_user_scope`](MaybeAuthUser::require_user_scope)), exactly
//! Pleroma's `OAuthScopesPlug … fallback: :proceed_unauthenticated`: an
//! anonymous request searches the stored, active accounts, a token without
//! the scope is the 403, and an invalid token is still the extractor's
//! 401. A viewer is not decoration either: they name the account the
//! [`Account`] entities render for, drive the block/mute exclusions and
//! the "followed by the viewer" rank class, and are the only way the
//! `following` restriction means anything — `following=true` without a
//! viewer searches an empty following list and answers nothing.
//!
//! Pagination is `offset`-based ([`PageParams`] with the account
//! listings' default 40 and cap 80): a ranking over a changing corpus has
//! no stable id cursor, which is why Pleroma pages search with
//! `fetch_paginated(…, :offset)` too. An absent or empty `q` answers no
//! accounts (the empty array, or the empty `accounts` list of the search
//! document) — never the whole table.
//!
//! `GET /api/v1/search` and `GET /api/v2/search` share one handler: the
//! ticket fixes both versions' accounts-only answer to the same document,
//! `{"accounts": […], "statuses": [], "hashtags": []}` (Mastodon's v1
//! spells `hashtags` as plain strings and v2 as tag entities; both are
//! empty until a later ticket searches anything but accounts). The `type`
//! parameter picks the resource: absent or `accounts` answers the account
//! list, `statuses`, `hashtags` or anything else answers the empty
//! document, because neither other resource is searched yet. `account_id`
//! (search one account's statuses) and `exclude_unreviewed` (filter
//! hashtag review states) belong to those other resources and are
//! accepted and ignored with them; `resolve` rides along to the model,
//! which ignores it until the federation ticket gives it `WebFinger`.
//!
//! All three routes run in the limiter's `search` bucket (Pleroma
//! throttles the three search actions there; see [`crate::rate_limit`]).

use axum::{Json, Router, extract::State, http::Uri, routing::get};
use serde::Serialize;
use serde_json::Value;
use tracing::debug;
use url::form_urlencoded;

use crate::{
    auth::MaybeAuthUser,
    entities::account::Account,
    error::AppResult,
    models::{
        user::User,
        user_search::{self, SearchOpts},
    },
    oauth::scopes::READ_ACCOUNTS,
    pagination::PageParams,
    state::AppState,
};

/// Default page size of the search endpoints (`40`, what the ticket pins;
/// Pleroma's `@search_limit`).
const LIST_LIMIT: i64 = 40;

/// Cap of [`LIST_LIMIT`]: the most any of the three routes answers at once
/// (Pleroma's account-listing `min(80)`).
const LIST_CAP: i64 = 80;

/// The row to start at when the request carries no `offset`: the first
/// page ([`PageParams::offset`] reads an absent or unusable offset as
/// `None`).
const NO_OFFSET: i64 = 0;

/// The only `type` whose answer is not the empty document in this ticket:
/// accounts, searched by [`crate::models::user_search`].
const ACCOUNTS_TYPE: &str = "accounts";

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
/// Mastodon's v2 combined search, the very handler the v1 route serves
/// and the same `search` rate-limit bucket as its v1 twin.
pub fn v2_router(state: &AppState) -> Router<AppState> {
    Router::new().route(
        "/search",
        get(search).layer(crate::rate_limit::rate_limit(state, "search")),
    )
}

/// Which resource of the combined search answer a `type` parameter asks
/// for. This ticket searches accounts; everything else is the empty list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Resource {
    /// No `type`, or `type=accounts`: the account search.
    #[default]
    Accounts,
    /// `statuses`, `hashtags`, or any other spelling: nothing is searched
    /// for it yet, so its list stays empty.
    Other,
}

impl Resource {
    /// Reads a `type` parameter value: [`ACCOUNTS_TYPE`] (case-insensitive,
    /// as Pleroma's cast downcases it) is [`Resource::Accounts`], anything
    /// else [`Resource::Other`].
    const fn of(text: &str) -> Self {
        if text.eq_ignore_ascii_case(ACCOUNTS_TYPE) {
            Self::Accounts
        } else {
            Self::Other
        }
    }
}

/// The request parameters of the three search routes, read from the raw
/// query string the way [`PageParams`] reads its own keys (duplicates
/// fold last-wins, unrelated keys are ignored): `q`, `limit`, `offset`,
/// `following`, `resolve`, and — on the combined search — `type`.
/// `account_id` and `exclude_unreviewed` belong to the resources that
/// are not searchable yet, so they are never read at all.
#[derive(Debug)]
struct SearchParams {
    /// The raw `q` parameter; the model's own normalisation (strip the
    /// leading `@`, lowercase) does the folding, and an absent parameter
    /// is the empty string, which answers nothing.
    q: String,
    /// `limit`/`offset` of the page: default [`LIST_LIMIT`], cap
    /// [`LIST_CAP`].
    page: PageParams,
    /// `following`: restrict the search to the viewer's follows.
    following: bool,
    /// `resolve`: handed to the model, which ignores it (yet).
    resolve: bool,
    /// `type`: which resource lists the combined search fills.
    resource: Resource,
}

impl SearchParams {
    /// Reads the parameters out of a raw query string.
    ///
    /// # Errors
    ///
    /// [`AppError::BadRequest`](crate::error::AppError) from
    /// [`PageParams`] for a malformed pagination cursor — search pages by
    /// `offset`, but the shared parser still reads the keyset ids and
    /// refuses an unreadable one, as every other listing here does.
    fn parse(query: Option<&str>) -> AppResult<Self> {
        let mut found = Self {
            q: String::new(),
            page: PageParams::from_query_with_default(query, LIST_LIMIT, LIST_CAP)?,
            following: false,
            resolve: false,
            resource: Resource::default(),
        };
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            match key.as_ref() {
                "q" => found.q = value.into_owned(),
                "following" => found.following = is_true(&value),
                "resolve" => found.resolve = is_true(&value),
                "type" => found.resource = Resource::of(&value),
                _ => {}
            }
        }
        Ok(found)
    }

    /// The [`SearchOpts`] these parameters claim: the model's page is the
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
}

/// Whether a query parameter's text spells a true boolean: `true` or `1`,
/// trimmed and matched case-insensitively — the same reading the directory
/// gives its `local` flag; anything else (absent, empty, `false`, junk)
/// is `false`.
fn is_true(text: &str) -> bool {
    let text = text.trim();
    text.eq_ignore_ascii_case("true") || text == "1"
}

/// The whole combined-search answer, all three resource lists present in
/// every reply (Mastodon's shape, which Pleroma's `do_search` renders
/// even when only one resource was searched): the accounts this ticket
/// finds, and the two lists that stay empty because statuses and hashtags
/// are not searched yet. v1 spells hashtag entries as plain strings and
/// v2 as tag entities; with both lists empty the two versions' documents
/// are the same JSON, which is why one handler answers both.
#[derive(Debug, Serialize)]
struct SearchResults {
    /// The matching [`Account`] entities, in the model's ranking.
    accounts: Vec<Account>,
    /// Statuses matching the query — always empty in this ticket.
    statuses: Vec<Value>,
    /// Hashtags matching the query — always empty in this ticket.
    hashtags: Vec<Value>,
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
/// [`PageParams`] for a malformed cursor. Any database failure arrives
/// through `?` as [`AppError::Database`](crate::error::AppError).
async fn account_search(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    uri: Uri,
) -> AppResult<Json<Vec<Account>>> {
    auth.require_user_scope(READ_ACCOUNTS)?;
    let params = SearchParams::parse(uri.query())?;
    Ok(Json(search_accounts(&state, &auth, &params).await?))
}

/// `GET /api/v1/search` and `GET /api/v2/search` — the combined search
/// document. With no `type` (or `type=accounts`) it carries the account
/// search; with any other `type` every list is empty, because statuses
/// and hashtags are searched by later tickets.
///
/// # Errors
///
/// Exactly those of [`account_search`].
async fn search(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    uri: Uri,
) -> AppResult<Json<SearchResults>> {
    auth.require_user_scope(READ_ACCOUNTS)?;
    let params = SearchParams::parse(uri.query())?;
    let accounts = if params.resource == Resource::Accounts {
        search_accounts(&state, &auth, &params).await?
    } else {
        Vec::new()
    };
    Ok(Json(SearchResults {
        accounts,
        statuses: Vec::new(),
        hashtags: Vec::new(),
    }))
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
    use super::*;

    /// The parsed parameters of `query`.
    ///
    /// # Panics
    ///
    /// If the fixed test query fails to parse (a malformed cursor a test
    /// names as such never reaches here).
    fn parse(query: &str) -> SearchParams {
        SearchParams::parse(Some(query)).expect("the fixed test query parses")
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
        assert_eq!(bare.page.limit, LIST_LIMIT, "and the endpoint's own page");
        assert_eq!(bare.page.offset, None, "no offset means the first page");

        let junk = parse("q=bob&following=yes&resolve=0");
        assert!(!junk.following, "junk is not a true");
        assert!(!junk.resolve);

        let clamped = parse("q=bob&limit=900&offset=40&following=true");
        assert_eq!(clamped.page.limit, LIST_CAP, "the cap clamps the page");
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
            assert_eq!(sized.page.limit, want, "`{query:?}` pages at {want}");
        }
        assert_eq!(
            parse("offset=3").page.offset,
            Some(3),
            "a real offset reads"
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
    fn only_accounts_is_the_resource_that_searches() {
        assert_eq!(Resource::of("accounts"), Resource::Accounts);
        assert_eq!(
            Resource::of("ACCOUNTS"),
            Resource::Accounts,
            "the spelling is folded, as Pleroma's cast folds it"
        );
        for other in ["statuses", "hashtags", "bonked", ""] {
            assert_eq!(
                Resource::of(other),
                Resource::Other,
                "`{other}` is a resource this ticket does not search"
            );
        }
        assert_eq!(
            Resource::default(),
            Resource::Accounts,
            "no type at all searches accounts, like Mastodon's default"
        );

        let untyped = parse("q=bob");
        assert_eq!(untyped.resource, Resource::Accounts, "absent type");
        let other_type = parse("q=bob&type=statuses");
        assert_eq!(other_type.resource, Resource::Other, "another resource");
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
}
