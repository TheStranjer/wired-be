//! The admin user endpoints: the accounts listing and the one-account show
//! under `/api/v1/pleroma/admin/users` (WBE-195).
//!
//! The reference is Pleroma's `AdminAPI.UserController`
//! (`lib/pleroma/web/admin_api/controllers/user_controller.ex`), `index`
//! and `show`, rendering through `AdminAPI.AccountView`
//! (`lib/pleroma/web/admin_api/views/account_view.ex`). Pleroma gates both
//! reads with `admin:read:accounts`; the same gate is the
//! [`AdminUser::require`] call each handler makes first, on the area
//! `accounts`. Both answers are Pleroma's admin account entity, [`AdminAccount`].
//!
//! - `GET /users` (`index`) — [`Search.user`](crate::models::user::admin)
//!   over the request's criteria: `query` (a case-insensitive substring of
//!   nickname, display name or email), `filters` (the comma list of `local`,
//!   `external`, `active`, `deactivated`, `need_approval`, `unconfirmed`,
//!   `is_admin`, `is_moderator` — a name off that list is ignored),
//!   `tags[]`, `name`, `email` and `actor_types[]`, paged by `page` (from 1)
//!   and `page_size` (Pleroma's 50, capped at 200). The answer is
//!   `{users, count, page_size}`: the nickname-ordered page, the total the
//!   criteria select across every page, and the size the page was cut to.
//!   The instance actor is never among them.
//! - `GET /users/{nickname}` (`show`) — the account the segment names, by
//!   nickname (a remote `nick@domain` handle included, axum delivering the
//!   percent-decoded segment) or, when no nickname matches, by id. Anything
//!   else is the 404 `{"error":"Not found"}` the rest of this API answers
//!   with; unlike the listing, the show finds a deactivated account and the
//!   instance actor, because naming one is not listing them.
//!
//! Two of Pleroma's parameters are read wider here than its
//! `CastAndValidate` declares them: an array key arrives in both the
//! bracketed spelling `pleroma-fe` sends (`tags[]`) and the bare one
//! (`tags`), the way the timeline routes read their hashtag lists.
//!
//! [`AdminUser::require`]: crate::auth::admin::AdminUser::require
//! [`AdminAccount`]: crate::entities::admin_account::AdminAccount

use axum::{
    Json, Router,
    extract::{Path, State},
    http::Uri,
    routing::get,
};
use tracing::debug;
use url::form_urlencoded;

use crate::{
    auth::admin::AdminUser,
    entities::admin_account::{AdminAccount, AdminAccountList},
    error::{AppError, AppResult},
    models::user::admin,
    state::AppState,
};

/// The read endpoint class of [`AdminUser::require`].
const CLASS_READ: &str = "read";
/// The admin scope area these endpoints live in
/// (`admin:read:accounts`, Pleroma's `OAuthScopesPlug` pair for `index` and
/// `show`).
const AREA_ACCOUNTS: &str = "accounts";

/// The user routes of the admin API, merged into [`super`].
///
/// `/users/{nickname}` is the show route of the same prefix the invite
/// actions (`/users/invites` and friends, [`super::invites`]) live under;
/// the router prefers a static segment over a parameter, so both are
/// reachable.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/users", get(list_users))
        .route("/users/{nickname}", get(show_user))
}

/// `GET /api/v1/pleroma/admin/users` — Pleroma's `index`: the criteria of
/// the query string, one page of entities and the whole total.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::Database`] from the listing read.
async fn list_users(
    State(state): State<AppState>,
    admin: AdminUser,
    uri: Uri,
) -> AppResult<Json<AdminAccountList>> {
    admin.require(CLASS_READ, AREA_ACCOUNTS)?;
    let params = list_params(uri.query());
    let page = admin::list(&state.db, &params).await?;
    debug!(
        page = params.page,
        page_size = params.page_size,
        count = page.count,
        listed = page.users.len(),
        "admin users listed"
    );
    Ok(Json(AdminAccountList::from_page(
        &page,
        params.page_size,
        &state.config,
    )))
}

/// `GET /api/v1/pleroma/admin/users/{nickname}` — Pleroma's `show`: the
/// entity behind the handle or id the path segment names.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::RouteNotFound`] for text that is
/// no stored nickname and no id; [`AppError::Database`] from the read.
async fn show_user(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminAccount>> {
    admin.require(CLASS_READ, AREA_ACCOUNTS)?;
    let found = admin::get_by_nickname_or_id(&state.db, &raw)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    debug!(
        id = %found.id,
        nickname = %found.nickname,
        "admin user shown"
    );
    Ok(Json(AdminAccount::from_user(&found, &state.config)))
}

/// The listing criteria one request states: the five named criteria, the
/// comma list of `filters` and the page window, read off the raw query
/// string `Uri::query` returns (percent-decoding and all).
///
/// Every parameter is optional; the page window keeps the admin listings'
/// defaults (page 1, [`super::DEFAULT_PAGE_SIZE`] rows, the
/// [`super::MAX_PAGE_SIZE`] cap), so an absent or unusable number never
/// refuses the request.
fn list_params(query: Option<&str>) -> admin::ListParams {
    let pairs = QueryPairs::parse(query);
    admin::ListParams {
        query: pairs.value("query"),
        name: pairs.value("name"),
        email: pairs.value("email"),
        tags: pairs.values("tags"),
        actor_types: pairs.values("actor_types"),
        filters: admin::ListFilters::parse(pairs.value("filters").as_deref()),
        page: super::page_number_of(pairs.value("page").as_deref(), super::DEFAULT_PAGE),
        page_size: super::page_size_of(pairs.value("page_size").as_deref()),
    }
}

/// The decoded pairs of one query string, kept in order so a repeated array
/// key can be read as the list it is while a repeated scalar folds to its
/// last value — how a query-parameter map reads either way, and what
/// Pleroma's `conn.query_params` hands its controller.
#[derive(Debug)]
struct QueryPairs(Vec<(String, String)>);

impl QueryPairs {
    /// Decodes `query` (an absent query string is no pairs at all).
    fn parse(query: Option<&str>) -> Self {
        let pairs = query.map_or_else(Vec::default, |raw| {
            form_urlencoded::parse(raw.as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        });
        Self(pairs)
    }

    /// The value of the last occurrence of `key`, `None` when it is absent
    /// or empty — an empty parameter is nothing to filter on (Pleroma's
    /// `not_empty_string` guard).
    fn value(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .rev()
            .find(|(found, _)| found == key)
            .map(|(_, value)| value.clone())
            .filter(|value| !value.is_empty())
    }

    /// Every value of `key` in occurrence order, in both the bracketed
    /// spelling a client sends (`key[]`) and the bare one, empty values
    /// dropped; neither spelling present is the empty list.
    fn values(&self, key: &str) -> Vec<String> {
        let bracketed = format!("{key}[]");
        self.0
            .iter()
            .filter(|(found, value)| !value.is_empty() && (found == key || found == &bracketed))
            .map(|(_, value)| value.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No query string at all is the criterion-free first page at the
    /// default size.
    #[test]
    fn an_absent_query_is_the_default_page() {
        for raw in [None, Some("")] {
            let params = list_params(raw);
            assert_eq!(params.page, super::super::DEFAULT_PAGE, "the first page");
            assert_eq!(
                params.page_size,
                super::super::DEFAULT_PAGE_SIZE,
                "and Pleroma's size"
            );
            assert!(
                params.query.is_none()
                    && params.name.is_none()
                    && params.email.is_none()
                    && params.actor_types.is_empty()
                    && params.filters == admin::ListFilters::default(),
                "nothing to filter on: {params:?}"
            );
        }
    }

    /// Every criterion the request states reaches the params, in the
    /// spelling the admin frontend sends them.
    #[test]
    fn the_named_criteria_are_read_from_the_query() {
        let params = list_params(Some(
            "query=al%20ice&name=Alice&email=alice%40example.org&tags[]=bots&tags[]=ops&actor_types[]=Service&page=3&page_size=10",
        ));
        assert_eq!(params.query.as_deref(), Some("al ice"), "the decoded query");
        assert_eq!(params.name.as_deref(), Some("Alice"), "the display name");
        assert_eq!(
            params.email.as_deref(),
            Some("alice@example.org"),
            "the decoded email"
        );
        assert_eq!(params.tags, vec!["bots", "ops"], "both bracketed tags");
        assert_eq!(params.actor_types, vec!["Service"], "and the actor type");
        assert_eq!(params.page, 3, "the page number");
        assert_eq!(params.page_size, 10, "and the size");
    }

    /// An array key arrives bracketed or bare, and both spellings fill the
    /// same list in occurrence order; an empty element is no criterion.
    #[test]
    fn an_array_key_arrives_in_both_spellings() {
        let params = list_params(Some(
            "tags=a&tags[]=b&tags=&actor_types=Bot&actor_types[]=Service",
        ));
        assert_eq!(params.tags, vec!["a", "b"], "bare and bracketed together");
        assert_eq!(
            params.actor_types,
            vec!["Bot", "Service"],
            "the same for actor types"
        );
    }

    /// The `filters` comma list is handed to the model's parser whole, and
    /// the page window keeps its defaults and its cap when a parameter is
    /// no usable number.
    #[test]
    fn the_filters_and_the_window_are_read_leniently() {
        let params = list_params(Some(
            "filters=is_admin,need_approval,ghost&page=x&page_size=9000",
        ));
        assert!(
            params.filters.is_admin.get() && params.filters.need_approval.get(),
            "two known filters"
        );
        assert!(
            !params.filters.local.get() && !params.filters.active.get(),
            "and nothing else: {params:?}"
        );
        assert_eq!(
            params.page,
            super::super::DEFAULT_PAGE,
            "a non-number page is the first page"
        );
        assert_eq!(
            params.page_size,
            super::super::MAX_PAGE_SIZE,
            "and an oversized page is capped"
        );
    }

    /// A repeated scalar keeps its last value, the way a query map folds
    /// duplicates.
    #[test]
    fn a_repeated_scalar_keeps_its_last_value() {
        let params = list_params(Some("query=first&query=second"));
        assert_eq!(
            params.query.as_deref(),
            Some("second"),
            "the last occurrence wins"
        );
    }

    /// The pair reader's own rules: an absent key, an empty value and the
    /// order of a list.
    #[test]
    fn the_pair_reader_folds_duplicates_and_drops_empties() {
        let pairs = QueryPairs::parse(Some("a=1&a=&b=2&c[]=x&a[]=y&empty=&empty[]="));
        assert_eq!(pairs.value("a").as_deref(), None, "the empty last value");
        assert_eq!(pairs.value("b").as_deref(), Some("2"), "an ordinary value");
        assert_eq!(pairs.value("missing"), None, "and an absent key");
        assert_eq!(
            pairs.values("empty"),
            Vec::<String>::new(),
            "a key whose every value is empty lists nothing"
        );
        assert_eq!(
            pairs.values("a"),
            vec!["1", "y"],
            "a key with values in both spellings lists them in order"
        );
        assert_eq!(pairs.values("c"), vec!["x"], "the bracketed spelling");
        assert_eq!(
            pairs.values("missing"),
            Vec::<String>::new(),
            "an absent key is the empty list"
        );
        let none = QueryPairs::parse(None);
        assert_eq!(
            none.value("anything"),
            None,
            "no query string holds no pairs"
        );
    }
}
