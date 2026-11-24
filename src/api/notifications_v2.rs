//! Grouped notifications over HTTP: the Mastodon v2 notification groups,
//! `GET /api/v2/notifications` and its single-group, group-accounts,
//! group-dismiss and unread-count companions.
//!
//! The reference is Pleroma's `NotificationController`
//! (`lib/pleroma/web/mastodon_api/controllers/notification_controller.ex`,
//! the `grouped_index`, `unread_count`, `show_group`, `group_accounts` and
//! `dismiss_group` actions) and its `MastodonAPI` group queries; the row
//! storage and the grouping itself live in [`crate::models::notification`].
//!
//! Grouping: notifications of the same `type` about the same status share
//! one key — `<type>-<status_id>`, `<type>-none` when no status is named,
//! and `<type>-<status_id>-<emoji>` for a `pleroma:emoji_reaction`, so one
//! emoji's reactions to one status are one group. Every other type keeps
//! the unique key `ungrouped-<notification id>`. `grouped_types[]` narrows
//! the first rule: types outside the list read as ungrouped for that
//! request, and the key is recomputed per request (Pleroma's
//! `notification_group_keyed_query` `CASE`), while the `{group_key}` paths
//! match the stored key or the ungrouped id like Pleroma's two
//! `get_notification_group*` clauses do.
//!
//! - `GET /api/v2/notifications` answers `{accounts, statuses,
//!   notification_groups}`: one [`PageParams`] page of groups ordered by
//!   each group's `most_recent_notification_id` — the cursor walks compare
//!   that id, and the [`Link`](crate::pagination::link_header) header carries it — with
//!   `types[]`, `exclude_types[]`, `account_id` and `grouped_types[]`
//!   filters. Sample actors land in `accounts` and group statuses in
//!   `statuses`, expanded once per response; a status the reader may not
//!   see drops out of `statuses` while its group keeps its `status_id`.
//!   `expand_accounts` (`full` | `partial_avatars`, default `full`) is
//!   validated per Mastodon's enum and answered the same either way: this
//!   instance stores one avatar image per account, so Pleroma's rendering —
//!   which does not implement the parameter at all and always returns full
//!   accounts — is also the partial rendering here.
//! - `GET /api/v2/notifications/unread_count` answers `{"count": N}`: the
//!   caller's unseen undismissed notifications newer than their
//!   `notifications` marker, under `types[]` / `exclude_types[]` /
//!   `account_id`, capped at `limit` (default 100, at most 1000).
//! - `GET /api/v2/notifications/{group_key}` answers the same envelope for
//!   the one group — its whole-group bounds are its page bounds — and 404s
//!   a key the caller owns nothing undismissed under.
//! - `GET /api/v2/notifications/{group_key}/accounts` answers every actor
//!   of the group as full [`Account`] entities, newest actor first, each
//!   named once; an unknown key answers `[]` (Pleroma's render of an empty
//!   user list).
//! - `POST /api/v2/notifications/{group_key}/dismiss` marks the group's
//!   notifications dismissed — out of every read from then on — and answers
//!   `{}`; dismissing an unknown group changes nothing and still answers
//!   `{}` (Pleroma's unconditional `json(conn, %{})`).
//!
//! Group keys ride in path segments percent-encoded by clients
//! (`pleroma%3Aemoji_reaction-%E2%9D%A4`); axum hands them to the handlers
//! decoded. `ungrouped-<id>` names one notification, and every other key
//! names a stored group.
//!
//! Reads require `read:notifications` and the dismiss requires
//! `write:notifications` (the ticket's grant; Pleroma keeps group accounts
//! in the read set with the same least-privilege reasoning its controller
//! comment states). An `account_id` that is malformed, unknown or
//! deactivated is the shared 404 ([`account_or_404`]), like Pleroma's
//! `grouped_index` clause that looks the account up before listing.

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::{account_or_404, distinct, render_visible},
    auth::AuthUser,
    entities::{
        account::{Account, AccountViewer},
        notification::{GroupedNotifications, NotificationGroup},
        relationship::Relationship,
        status::Status as StatusEntity,
    },
    error::{AppError, AppResult},
    extract::{ApiParams, flex},
    models::{
        marker::{self, Marker},
        notification::{self, GroupInfo, GroupPageParams, NotificationFilters},
        user::{self, User},
    },
    oauth::scopes::{READ_NOTIFICATIONS, WRITE_NOTIFICATIONS},
    pagination::{NO_EXTRA_QUERY, PageParams, keyset_link, set_link_header},
    state::AppState,
    util::id::parse_id,
};

/// Pleroma's `unread_count_limit` default: a caller asking for no cap gets
/// a hundred counted.
const UNREAD_DEFAULT_LIMIT: i64 = 100;
/// Pleroma's `unread_count_limit` ceiling: `min(parse_limit(…), 1000)`.
const UNREAD_MAX_LIMIT: i64 = 1000;
/// The default `expand_accounts` mode (Mastodon's default).
const EXPAND_FULL: &str = "full";
/// The other accepted `expand_accounts` spelling (Mastodon's
/// `partial_avatars`).
const EXPAND_PARTIAL_AVATARS: &str = "partial_avatars";

/// The `/api/v2` grouped-notifications router, merged into
/// [`crate::api::v2`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/notifications", get(list_groups))
        .route("/notifications/unread_count", get(unread_count))
        .route("/notifications/{group_key}", get(show_group))
        .route("/notifications/{group_key}/accounts", get(group_accounts))
        .route("/notifications/{group_key}/dismiss", post(dismiss_group))
}

/// One name list of a repeated parameter: one value or a list (a JSON
/// array, or `types[]=…` keys in a query string) — the two spellings
/// Mastodon clients and forms both send, like the markers' timeline names.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum StringList {
    /// A lone name.
    Single(String),
    /// A list of names.
    Multiple(Vec<String>),
}

impl StringList {
    /// The names, borrowed, in the order sent.
    pub(crate) fn names(&self) -> Vec<&str> {
        match self {
            Self::Single(name) => vec![name.as_str()],
            Self::Multiple(names) => names.iter().map(String::as_str).collect(),
        }
    }
}

/// The names behind an optional list parameter, none when it was absent.
pub(crate) fn names_of(list: Option<&StringList>) -> Vec<&str> {
    list.map_or_else(Vec::new, StringList::names)
}

/// `GET /api/v2/notifications`' non-cursor parameters ([`ApiParams`],
/// decision 10; the cursors and `limit` ride on [`PageParams`]).
#[derive(Debug, Deserialize)]
struct ListParams {
    /// `types[]`: keep only these notification types.
    #[serde(default)]
    types: Option<StringList>,
    /// `exclude_types[]`: drop these notification types.
    #[serde(default)]
    exclude_types: Option<StringList>,
    /// `grouped_types[]`: which types keep their stored group; absent
    /// groups everything the server can group, and an explicit list that
    /// names none groups nothing.
    #[serde(default)]
    grouped_types: Option<StringList>,
    /// `account_id`: only notifications caused by this account.
    #[serde(default)]
    account_id: Option<String>,
    /// `expand_accounts`: the expansion mode, validated against Mastodon's
    /// enum (see the [module docs](self) for what both modes render).
    #[serde(default)]
    expand_accounts: Option<String>,
    /// `with_muted`: whether the caller's mutes (actor and thread) hide
    /// nothing; absent means `false`. Blocks hide either way.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    with_muted: Option<bool>,
}

impl ListParams {
    /// Checks `expand_accounts` against the two documented modes; anything
    /// else is Mastodon's enum violation, a 422. Both modes parse to the
    /// same render here, which is why validation is all this returns.
    fn validate_expand_accounts(&self) -> AppResult<()> {
        match self.expand_accounts.as_deref() {
            None | Some(EXPAND_FULL | EXPAND_PARTIAL_AVATARS) => Ok(()),
            Some(other) => Err(AppError::Unprocessable(format!(
                "Invalid parameters: expand_accounts must be {EXPAND_FULL} or {EXPAND_PARTIAL_AVATARS}, got `{other}`"
            ))),
        }
    }

    /// The effective `grouped_types[]`: the names the request listed, or
    /// every type the server can group when it listed none (Pleroma's
    /// `normalize_grouped_types(nil)`).
    fn grouped_types(&self) -> Vec<&str> {
        if self.grouped_types.is_none() {
            return notification::GROUPABLE_TYPES.to_vec();
        }
        names_of(self.grouped_types.as_ref())
    }

    /// The `types[]` / `exclude_types[]` half of the read filters; the
    /// account half arrives through [`account_filter`].
    fn type_filters(&self) -> NotificationFilters<'_> {
        NotificationFilters {
            types: names_of(self.types.as_ref()),
            exclude_types: names_of(self.exclude_types.as_ref()),
            account_id: None,
        }
    }
}

/// `GET /api/v2/notifications/unread_count`' non-cursor parameters: the
/// same filters as the listing, `grouped_types[]` aside (the answer counts
/// notifications, not groups).
#[derive(Debug, Deserialize)]
struct UnreadCountParams {
    /// `types[]`: count only these notification types.
    #[serde(default)]
    types: Option<StringList>,
    /// `exclude_types[]`: never count these.
    #[serde(default)]
    exclude_types: Option<StringList>,
    /// `account_id`: count only notifications caused by this account.
    #[serde(default)]
    account_id: Option<String>,
    /// `with_muted`: count what the caller's mutes hide too; absent means
    /// `false`. Blocks hide either way.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    with_muted: Option<bool>,
}

impl UnreadCountParams {
    /// The `types[]` / `exclude_types[]` half of the read filters.
    fn type_filters(&self) -> NotificationFilters<'_> {
        NotificationFilters {
            types: names_of(self.types.as_ref()),
            exclude_types: names_of(self.exclude_types.as_ref()),
            account_id: None,
        }
    }
}

/// The `account_id` parameter resolved (or absent).
///
/// # Errors
///
/// [`AppError::NotFound`] through [`account_or_404`] when the text names no
/// readable account.
pub(crate) async fn account_filter(
    state: &AppState,
    account_id: Option<&str>,
) -> AppResult<Option<Uuid>> {
    match account_id {
        None => Ok(None),
        Some(text) => account_or_404(state, text)
            .await
            .map(|found| Some(found.id)),
    }
}

/// `GET /api/v2/notifications` — one page of notification groups, newest
/// group first, with the sample actors and group statuses expanded for the
/// caller and a `Link` header keyed on the first and last group's
/// `most_recent_notification_id`.
///
/// # Errors
///
/// [`Unauthorized`](crate::error::AppError::Unauthorized) /
/// [`Forbidden`](crate::error::AppError::Forbidden) from [`AuthUser`] and
/// the `read:notifications` check; [`BadRequest`](crate::error::AppError::BadRequest)
/// from [`PageParams`] for a malformed cursor;
/// [`Unprocessable`](crate::error::AppError::Unprocessable) for an
/// `expand_accounts` outside the two modes;
/// [`NotFound`](crate::error::AppError::NotFound) for an `account_id`
/// naming no readable account. Any database failure arrives through `?` as
/// [`Database`](crate::error::AppError::Database), and
/// [`set_link_header`] reports a `Link` value that is not header-safe as
/// [`Internal`](crate::error::AppError::Internal).
async fn list_groups(
    State(state): State<AppState>,
    auth: AuthUser,
    OriginalUri(uri): OriginalUri,
    page: PageParams,
    ApiParams(params): ApiParams<ListParams>,
) -> AppResult<(HeaderMap, Json<GroupedNotifications>)> {
    auth.require(READ_NOTIFICATIONS)?;
    params.validate_expand_accounts()?;
    let mut filters = params.type_filters();
    filters.account_id = account_filter(&state, params.account_id.as_deref()).await?;
    let entries = notification::group_page(
        &state.db,
        &GroupPageParams {
            user_id: auth.0.id,
            grouped_types: params.grouped_types(),
            filters,
            with_muted: params.with_muted.unwrap_or_default(),
            page,
        },
    )
    .await?;
    debug!(
        count = entries.len(),
        user = %auth.0.id,
        "notification groups listed"
    );
    // The cursor is each group's `most_recent_notification_id`, the id the
    // listing walks on.
    let link = keyset_link(
        &state.config.base_url,
        uri.path(),
        &NO_EXTRA_QUERY,
        &entries,
        |entry| entry.info.most_recent_id,
    );
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    let groups: Vec<(&str, &GroupInfo)> = entries
        .iter()
        .map(|entry| (entry.group_key.as_str(), &entry.info))
        .collect();
    let body = groups_response(&state, Some(&auth.0), &groups).await?;
    Ok((headers, Json(body)))
}

/// `GET /api/v2/notifications/unread_count` — how many unseen undismissed
/// notifications sit past the caller's `notifications` marker, capped at
/// `limit` (default [`UNREAD_DEFAULT_LIMIT`], at most [`UNREAD_MAX_LIMIT`]).
///
/// # Errors
///
/// The auth errors of [`list_groups`];
/// [`NotFound`](crate::error::AppError::NotFound) for an `account_id`
/// naming no readable account;
/// [`Database`](crate::error::AppError::Database) from the reads.
async fn unread_count(
    State(state): State<AppState>,
    auth: AuthUser,
    OriginalUri(uri): OriginalUri,
    ApiParams(params): ApiParams<UnreadCountParams>,
) -> AppResult<Json<Value>> {
    auth.require(READ_NOTIFICATIONS)?;
    let page =
        PageParams::from_query_with_default(uri.query(), UNREAD_DEFAULT_LIMIT, UNREAD_MAX_LIMIT)?;
    let mut filters = params.type_filters();
    filters.account_id = account_filter(&state, params.account_id.as_deref()).await?;
    let marker = read_marker(&state.db, auth.0.id).await?;
    let count = notification::unread_count(
        &state.db,
        &notification::UnreadParams {
            user_id: auth.0.id,
            filters,
            with_muted: params.with_muted.unwrap_or_default(),
            marker,
            limit: page.limit,
        },
    )
    .await?;
    debug!(
        count,
        user = %auth.0.id,
        "notification unread count read"
    );
    Ok(Json(json!({ "count": count })))
}

/// `GET /api/v2/notifications/{group_key}` — the one group under that key,
/// in the listing's envelope: its whole-group bounds are its page bounds,
/// and Pleroma's `show_group` renders the very same view for it.
///
/// # Errors
///
/// The auth errors of [`list_groups`];
/// [`NotFound`](crate::error::AppError::NotFound) when the caller owns no
/// undismissed notifications under the key;
/// [`Database`](crate::error::AppError::Database) from the reads.
async fn show_group(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(key): Path<String>,
) -> AppResult<Json<GroupedNotifications>> {
    auth.require(READ_NOTIFICATIONS)?;
    let selector = notification::select_group(&key);
    let info = notification::group_info(&state.db, auth.0.id, &selector)
        .await?
        .ok_or(AppError::NotFound)?;
    debug!(
        group_key = %key,
        count = info.notifications_count,
        user = %auth.0.id,
        "notification group read"
    );
    let body = groups_response(&state, Some(&auth.0), &[(key.as_str(), &info)]).await?;
    Ok(Json(body))
}

/// `GET /api/v2/notifications/{group_key}/accounts` — every actor of the
/// group as a full [`Account`], newest actor first, each named once; an
/// unknown key answers `[]`, Pleroma's render of an empty user list.
///
/// # Errors
///
/// The auth errors of [`list_groups`];
/// [`Database`](crate::error::AppError::Database) from the reads.
async fn group_accounts(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(key): Path<String>,
) -> AppResult<Json<Vec<Account>>> {
    auth.require(READ_NOTIFICATIONS)?;
    let selector = notification::select_group(&key);
    let ids = notification::group_actor_ids(&state.db, auth.0.id, &selector).await?;
    debug!(
        group_key = %key,
        count = ids.len(),
        user = %auth.0.id,
        "notification group accounts listed"
    );
    let accounts = expand_accounts(&state, Some(&auth.0), &ids).await?;
    Ok(Json(accounts))
}

/// `POST /api/v2/notifications/{group_key}/dismiss` — dismiss every
/// notification of the group and answer `{}`. The rows stay (hidden from
/// every read), so an unknown key simply dismisses nothing.
///
/// # Errors
///
/// The auth errors of [`list_groups`] under `write:notifications`;
/// [`Database`](crate::error::AppError::Database) from the write.
async fn dismiss_group(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(key): Path<String>,
) -> AppResult<Json<Map<String, Value>>> {
    auth.require(WRITE_NOTIFICATIONS)?;
    let selector = notification::select_group(&key);
    let dismissed = notification::dismiss_group(&state.db, auth.0.id, &selector).await?;
    debug!(
        group_key = %key,
        dismissed,
        user = %auth.0.id,
        "notification group dismissed"
    );
    Ok(Json(Map::new()))
}

/// The caller's `notifications` marker position as an id: `None` when they
/// have no marker, and when it holds text that names no id (Pleroma's
/// `restrict_after_marker` only ever binds a real position).
///
/// # Errors
///
/// [`Database`](crate::error::AppError::Database) from the marker read.
async fn read_marker(pool: &PgPool, user_id: Uuid) -> AppResult<Option<Uuid>> {
    let rows: Vec<Marker> = marker::get_many(pool, user_id, &[marker::NOTIFICATIONS]).await?;
    Ok(rows.first().and_then(|row| parse_id(&row.last_read_id)))
}

/// The `{accounts, statuses, notification_groups}` envelope for `groups`:
/// each entry rendered under its key, every sample actor expanded once
/// into `accounts` and every group status once into `statuses`, both as
/// seen by `viewer`.
async fn groups_response(
    state: &AppState,
    viewer: AccountViewer<'_>,
    groups: &[(&str, &GroupInfo)],
) -> AppResult<GroupedNotifications> {
    let actor_ids = collect_actor_ids(groups);
    let status_ids = collect_status_ids(groups);
    let accounts = expand_accounts(state, viewer, &actor_ids).await?;
    let statuses = expand_statuses(state, viewer, &status_ids).await?;
    let notification_groups = groups
        .iter()
        .map(|(key, info)| NotificationGroup::from_group(key, info))
        .collect();
    Ok(GroupedNotifications {
        accounts,
        statuses,
        notification_groups,
    })
}

/// The distinct sample actor ids of the page's groups, in group order and
/// within each group newest-first, each named once across groups (Pleroma's
/// envelope flattens its samples and dedupes by id).
fn collect_actor_ids(groups: &[(&str, &GroupInfo)]) -> Vec<Uuid> {
    distinct(
        groups
            .iter()
            .flat_map(|(_, info)| info.sample_account_ids.iter().copied()),
    )
}

/// The distinct status ids the groups name, in group order.
fn collect_status_ids(groups: &[(&str, &GroupInfo)]) -> Vec<Uuid> {
    distinct(groups.iter().filter_map(|(_, info)| info.status_id))
}

/// Expands account ids into [`Account`] entities as seen by `viewer`:
/// every stored account once, in the requested order, with the viewer's
/// [`Relationship`] embedded through the batched preload
/// [`Account::from_user_for`] reads. Actors that left no row (deleted
/// mid-request) simply drop out.
///
/// # Errors
///
/// [`Database`](crate::error::AppError::Database) from the account and
/// relationship reads.
async fn expand_accounts(
    state: &AppState,
    viewer: AccountViewer<'_>,
    ids: &[Uuid],
) -> AppResult<Vec<Account>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let users = user::get_many(&state.db, ids).await?;
    let mut by_id: HashMap<Uuid, User> = users.into_iter().map(|found| (found.id, found)).collect();
    let relationships = Relationship::load_map(&state.db, viewer, ids).await?;
    let accounts = ids
        .iter()
        .filter_map(|id| by_id.remove(id))
        .map(|found| Account::from_user_for(viewer, &found, &state.config, relationships.as_ref()))
        .collect();
    Ok(accounts)
}

/// Expands status ids into [`StatusEntity`]s as seen by `viewer`: every
/// stored status once, in the requested order, through [`render_visible`]
/// (the batched visibility filter and renderer). A status with no row and
/// one the visibility rules hide from this reader both drop out of the
/// array — the groups still
/// carry their `status_id`, Pleroma's id-only reference aside — and a
/// deleted one renders as nothing, which is what the renderer answers for
/// a tombstone.
///
/// # Errors
///
/// [`Database`](crate::error::AppError::Database) from the status and
/// visibility reads.
async fn expand_statuses(
    state: &AppState,
    viewer: AccountViewer<'_>,
    ids: &[Uuid],
) -> AppResult<Vec<StatusEntity>> {
    render_visible(state, viewer, ids).await
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// Deserializes `value` as the listing's parameters, as the extractor
    /// would.
    fn list_params(value: Value) -> ListParams {
        crate::extract::into_params(value).expect("the fixture deserializes")
    }

    #[test]
    fn absent_parameters_leave_everything_open_and_grouped() {
        let params = list_params(json!({}));
        let filters = params.type_filters();
        assert!(filters.types.is_empty(), "no type filter asked for");
        assert!(filters.exclude_types.is_empty(), "no exclusion asked for");
        assert_eq!(
            params.grouped_types(),
            notification::GROUPABLE_TYPES.to_vec(),
            "absent grouped_types groups every groupable type"
        );
        params
            .validate_expand_accounts()
            .expect("absent is the default mode");
    }

    #[test]
    fn list_names_parse_from_arrays_and_lone_values() {
        let params = list_params(json!({
            "types": ["favourite", "mention"],
            "exclude_types": "follow",
            "grouped_types": [],
        }));
        let filters = params.type_filters();
        assert_eq!(filters.types, ["favourite", "mention"], "the json array");
        assert_eq!(
            filters.exclude_types,
            ["follow"],
            "and the lone `exclude_types=` value"
        );
        assert!(
            params.grouped_types().is_empty(),
            "an explicit empty grouped_types groups nothing"
        );
    }

    #[test]
    fn a_non_string_list_is_a_parameter_error() {
        for body in [
            json!({"types": 1}),
            json!({"types": {"a": "b"}}),
            json!({"exclude_types": [1]}),
        ] {
            let err = crate::extract::into_params::<ListParams>(body.clone())
                .expect_err(&format!("`{body}` is not a name list"));
            assert!(
                err.to_string().starts_with("Invalid parameters"),
                "the extractor's wording for `{body}`: {err}"
            );
        }
    }

    #[test]
    fn expand_accounts_accepts_the_two_modes_and_rejects_anything_else() {
        for value in [EXPAND_FULL, EXPAND_PARTIAL_AVATARS] {
            let params = list_params(json!({"expand_accounts": value}));
            params
                .validate_expand_accounts()
                .expect("a documented mode parses");
        }
        for value in ["", "full_avatars", "avatars", "FULL"] {
            let params = list_params(json!({"expand_accounts": value}));
            let err = params
                .validate_expand_accounts()
                .expect_err(&format!("`{value}` is not a documented mode"));
            assert!(
                matches!(err, AppError::Unprocessable(_)),
                "`{value}` is a 422, not {err}"
            );
        }
    }

    #[test]
    fn the_unread_parameters_share_the_filters_shape() {
        let params: UnreadCountParams =
            crate::extract::into_params(json!({"types": "follow", "account_id": "7"}))
                .expect("the fixture deserializes");
        assert_eq!(
            params.type_filters().types,
            ["follow"],
            "the lone-value spelling reaches the same list"
        );
        assert_eq!(params.account_id.as_deref(), Some("7"), "the id stays text");
    }
}
