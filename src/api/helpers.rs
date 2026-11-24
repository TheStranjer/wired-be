//! Shared helpers for the API area handlers (`docs/ARCHITECTURE.md`
//! decision 3).
//!
//! Small request-level glue that several endpoints need and that belongs
//! neither inside one area's module nor in [`crate::util`] (which knows no
//! HTTP): resolving client-supplied ids to rows with exactly the error the
//! endpoint owes the client, reading the `id[]` array a batch endpoint
//! is keyed on, answering one [`TimelineQuery`] page as a status listing
//! ([`timeline_response`], with the `Link` header and the filter-parameter
//! readers the timeline routes and the account status listing share), the
//! two account-lifecycle writes the two admin surfaces share
//! ([`start_deletion`] and [`approve_account`]), and more as later tickets
//! need it.

use std::{collections::HashSet, hash::Hash};

use axum::{
    Json,
    http::{HeaderMap, Uri},
};
use serde::Deserialize;
use tracing::debug;
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    config::Config,
    entities::status::Status as StatusEntity,
    error::{AppError, AppResult},
    jobs::{QUEUE_BACKGROUND, deletion},
    mailer::{self, templates},
    models::{
        job::{NewJob, enqueue},
        oauth_token,
        status::{self, Status, Visibility},
        status_visibility,
        user::{self, AccountStatus, User, admin},
    },
    pagination::{keyset_link, set_link_header},
    services::{
        filter,
        render_status::{ViewerMode, render_statuses},
        timeline::TimelineQuery,
    },
    state::AppState,
    util::{id::parse_id, params::truthy},
};

mod visible;

pub use visible::{keyed_json, render_visible};

/// The `RESTRICT_UNAUTHENTICATED` area that closes anonymous reads of a
/// local status.
const ACTIVITIES_LOCAL: &str = "activities_local";
/// The `RESTRICT_UNAUTHENTICATED` area that closes anonymous reads of a
/// remote status.
const ACTIVITIES_REMOTE: &str = "activities_remote";

/// The keys the accounts batch endpoints read their ids from: Mastodon's
/// bracketed array plus the single value Pleroma's `List.wrap` accepts.
const ID_KEYS: [&str; 2] = ["id", "id[]"];
/// The deprecated alias of [`ID_KEYS`] that only the status batch lookup
/// answers to (`GET /api/v1/statuses`), per
/// [`requested_status_ids`].
const IDS_KEYS: [&str; 2] = ["ids", "ids[]"];

/// The most distinct ids a client-supplied id array may name on the routes
/// that do per-id work (list membership, conversation recipients, bulk
/// notification dismissal); more is refused with [`TOO_MANY_IDS`].
pub const MAX_BATCH_IDS: usize = 200;
/// The 422 message of an id array past [`MAX_BATCH_IDS`].
pub const TOO_MANY_IDS: &str = "Too many ids";

/// Resolves the `{id}` segment of an account route to its [`User`] row.
///
/// `id_str` must be `UUID` text as clients know it (decision 4: the
/// hyphenated or simple spelling, [`parse_id`]) and must name a row that
/// is still active. Anything else — a malformed id, an unknown id, a
/// deactivated account — is [`AppError::NotFound`]: Pleroma's
/// `assign_account_by_id` plus `visible_for` answer, and one that keeps
/// the endpoint from telling a bad id apart from a missing one. The show
/// endpoint (this ticket) and later routes keyed by account id share it.
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives
/// through `?` as [`AppError::Database`].
pub async fn account_or_404(state: &AppState, id_str: &str) -> AppResult<User> {
    let Some(id) = parse_id(id_str) else {
        return Err(AppError::NotFound);
    };
    user::get_by_id(&state.db, id)
        .await?
        .filter(|found| found.is_active.get())
        .ok_or(AppError::NotFound)
}

/// Whether `viewer` may see `account` at all (`None`: an anonymous request).
///
/// Pleroma's `User.visible_for/2`: the account itself and staff (an admin or
/// a moderator, who can manage activation state) may see an account that
/// is still waiting on approval or email confirmation; everyone else sees
/// only an account with nothing waiting on it ([`AccountStatus::Active`]),
/// so registration spam shows nothing before a moderator has looked at it.
/// A deactivated (or deleted) account stays hidden from every viewer, as
/// [`account_or_404`] has always answered.
#[must_use]
pub fn visible_to(account: &User, viewer: Option<&User>) -> bool {
    if !account.is_active.get() {
        return false;
    }
    if let Some(viewer) = viewer
        && (viewer.id == account.id || viewer.is_admin.get() || viewer.is_moderator.get())
    {
        return true;
    }
    matches!(account.account_status(), AccountStatus::Active)
}

/// [`account_or_404`] for a request made by `viewer`: the row is returned
/// only when [`visible_to`] says the viewer may see it.
///
/// A malformed id, an unknown id and an account the viewer may not see are
/// the same [`AppError::NotFound`], so the endpoint never tells a pending
/// registration from a missing one.
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives
/// through `?` as [`AppError::Database`].
pub async fn visible_account_or_404(
    state: &AppState,
    id_str: &str,
    viewer: Option<&User>,
) -> AppResult<User> {
    let Some(id) = parse_id(id_str) else {
        return Err(AppError::NotFound);
    };
    user::get_by_id(&state.db, id)
        .await?
        .filter(|found| visible_to(found, viewer))
        .ok_or(AppError::NotFound)
}

/// The `id` parameters of a raw query string, as parsed values in
/// occurrence order.
///
/// Both the bracketed array form Mastodon clients send (`id[]=a&id[]=b`,
/// percent-encodings included) and a single `id=a` (Pleroma's
/// `List.wrap(id)`), or any mix of the two. Values that are not `UUID` text
/// are dropped here, already skipped; whether an id names a stored account
/// is the caller's lookup (Pleroma's `User.get_all_by_ids` simply finds
/// nothing for an unknown one).
///
/// The batch endpoints keyed on a client-supplied id array share it: the
/// relationship batch (`GET /api/v1/accounts/relationships`) and the
/// familiar-followers batch (`GET /api/v1/accounts/familiar_followers`).
#[must_use]
pub fn requested_ids(query: Option<&str>) -> Vec<Uuid> {
    ids_under(query, &ID_KEYS)
}

/// The id array of `GET /api/v1/statuses`, which answers to both spellings.
///
/// [`requested_ids`]' `id[]` (and lone `id=`) and the deprecated `ids[]`
/// (and `ids=`) alias Pleroma's `StatusController.index` falls back to —
/// `Map.get(params, :id, Map.get(params, :ids))`. The `id` form wins
/// outright: a request that carries a usable `id` is answered from it and
/// its `ids` values are never read, exactly as that `Map.get` default
/// works. With no `id` at all the `ids` values are used, in occurrence
/// order; values that are not `UUID` text are dropped, already skipped.
///
/// How many of the ids an endpoint acts on is the caller's cap
/// ([`MAX_STATUS_IDS`](crate::api::statuses::MAX_STATUS_IDS)); this reads
/// them all.
#[must_use]
pub fn requested_status_ids(query: Option<&str>) -> Vec<Uuid> {
    let requested = ids_under(query, &ID_KEYS);
    if requested.is_empty() {
        return ids_under(query, &IDS_KEYS);
    }
    requested
}

/// The parsed values of the query params named by `keys`, in occurrence
/// order, with the values that are not `UUID` text dropped.
fn ids_under(query: Option<&str>, keys: &[&str]) -> Vec<Uuid> {
    let Some(query) = query else {
        return Vec::new();
    };
    form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _value)| keys.contains(&key.as_ref()))
        .filter_map(|(_key, value)| parse_id(&value))
        .collect()
}

/// Resolves the `{id}` segment of a status route to its live [`Status`]
/// row, as `viewer` may see it (`None`: an anonymous request).
///
/// `id_str` must be `UUID` text as clients know it (decision 4: the
/// hyphenated or simple spelling, [`parse_id`]). Everything that fails to
/// produce a row the viewer may read is the same [`AppError::NotFound`] —
/// a malformed id, an id that names no row, a tombstone (deleted), and a
/// status [`status_visibility`] hides from this viewer — so the endpoint
/// never reveals that a private post exists, which is Pleroma's
/// `StatusController.show` `with` block answering `{:error, :not_found}`
/// for both the missing and the invisible activity.
///
/// The row is the answer, not a rendered entity: this ticket's show route
/// renders it through [`crate::services::render_status`], and every later
/// `/api/v1/statuses/{id}/...` route (the context, history, source and
/// per-status state listings still to come) resolves and authorizes the
/// status through here and then reads what it needs from the row. A reblog
/// row is returned as itself; requesting a reblog's id yields the reblog,
/// whose original the renderer nests.
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives
/// through `?` as [`AppError::Database`].
pub async fn status_for_viewer_or_404(
    state: &AppState,
    viewer: Option<&User>,
    id_str: &str,
) -> AppResult<Status> {
    // Rule 7 of the visibility model reads the original of a reblog with
    // the same tests, so a reblog of a status this viewer may not see
    // fails here and is answered the same 404 as the original would be.
    let row = status_visibility::get_visible(&state.db, id_str, viewer).await?;
    // An anonymous read of an area the instance restricts is the same 404 a
    // hidden status gets, so the gate never confirms the post exists.
    if viewer.is_none() && anonymous_read_closed(state, &row).await? {
        return Err(AppError::NotFound);
    }
    Ok(row)
}

/// Whether the instance keeps anonymous readers from `row` itself: its
/// locality's `activities_*` area is listed in `RESTRICT_UNAUTHENTICATED`
/// ([`Config::restricts`]).
///
/// Pleroma's `restrict_unauthenticated_access?` for an activity. A reblog
/// row's original is not read here; see [`anonymous_read_closed`], which
/// also checks it. The caller applies this to anonymous requests only.
#[must_use]
pub fn anonymous_read_restricted(config: &Config, row: &Status) -> bool {
    config.restricts(if row.local {
        ACTIVITIES_LOCAL
    } else {
        ACTIVITIES_REMOTE
    })
}

/// Whether either `activities_*` area is restricted at all, so a caller can
/// skip the original lookups when nothing is.
fn activities_restricted(config: &Config) -> bool {
    config.restricts(ACTIVITIES_LOCAL) || config.restricts(ACTIVITIES_REMOTE)
}

/// [`anonymous_read_restricted`] for `row` and, when it is a reblog, the
/// original it announces (Pleroma checks both the activity and the object).
/// An original that is gone adds nothing.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
pub async fn anonymous_read_closed(state: &AppState, row: &Status) -> AppResult<bool> {
    if !activities_restricted(&state.config) {
        return Ok(false);
    }
    if anonymous_read_restricted(&state.config, row) {
        return Ok(true);
    }
    let Some(original_id) = row.reblog_of_id else {
        return Ok(false);
    };
    let original = status::get_by_id(&state.db, original_id).await?;
    Ok(original.is_some_and(|found| anonymous_read_restricted(&state.config, &found)))
}

/// `rows` without what the instance keeps from an anonymous reader: a
/// request with a `viewer` (or an instance with no `activities_*` area
/// restricted) gets them back untouched, in order.
///
/// The batch counterpart of the [`status_for_viewer_or_404`] check, for the
/// listings that narrow a set of rows rather than resolve one: the status
/// batch, the context arms and the quoters. Reblog originals are read in
/// one batch.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
pub async fn retain_open_to_viewer(
    state: &AppState,
    viewer: Option<&User>,
    rows: Vec<Status>,
) -> AppResult<Vec<Status>> {
    if viewer.is_some() || !activities_restricted(&state.config) {
        return Ok(rows);
    }
    let original_ids = distinct(rows.iter().filter_map(|row| row.reblog_of_id));
    let originals = status::get_many(&state.db, &original_ids).await?;
    let closed_originals: HashSet<Uuid> = originals
        .iter()
        .filter(|original| anonymous_read_restricted(&state.config, original))
        .map(|original| original.id)
        .collect();
    Ok(rows
        .into_iter()
        .filter(|row| {
            !anonymous_read_restricted(&state.config, row)
                && !row
                    .reblog_of_id
                    .is_some_and(|original| closed_originals.contains(&original))
        })
        .collect())
}

/// The decoded pairs of one query string, read the way Pleroma's
/// `conn.query_params` hands them to its controller.
///
/// The pairs are kept in order, so a repeated array key can be read as the
/// list it is while a repeated scalar folds to its last value — how a
/// query-parameter map reads either way.
///
/// Every listing that filters on more than the cursors reads its parameters
/// through it rather than through `Query<HashMap<…>>`: a map refuses the
/// bracketed array spelling a client sends (`tags[]=a`) outright, and the
/// admin listings are exactly where clients send both shapes.
#[derive(Debug)]
pub struct QueryPairs(Vec<(String, String)>);

impl QueryPairs {
    /// Decodes `query` (an absent query string is no pairs at all).
    #[must_use]
    pub fn parse(query: Option<&str>) -> Self {
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
    #[must_use]
    pub fn value(&self, key: &str) -> Option<String> {
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
    #[must_use]
    pub fn values(&self, key: &str) -> Vec<String> {
        let bracketed = format!("{key}[]");
        self.0
            .iter()
            .filter(|(found, value)| !value.is_empty() && (found == key || found == &bracketed))
            .map(|(_, value)| value.clone())
            .collect()
    }
}

/// The pagination keys the `Link` header replaces with its own cursors: the
/// three id cursors and `offset`, the cursors [`PageParams`] reads
/// (`limit` is a page size, not a cursor, and stays).
///
/// [`PageParams`]: crate::pagination::PageParams
const CURSOR_KEYS: [&str; 4] = ["max_id", "min_id", "since_id", "offset"];

/// The shared answer of every status-timeline page: one fetched, rendered,
/// filtered and linked [`TimelineQuery`].
///
/// [`fetch`](TimelineQuery::fetch) `query`, [`render_statuses`] the rows for
/// its viewer, post-process the page through
/// [`apply_active`](crate::services::filter::apply_active) in the source's
/// filter context ([`TimelineSource::filter_context`] — `None` for the
/// direct timeline, whose answer skips the pass entirely), and
/// answer the newest-first [`StatusEntity`] array with the `Link` header
/// this module's own page cursor derives from the request's `uri` and the
/// fetched rows' ids (Pleroma's `add_link_headers`). An empty page carries
/// no `Link` header at all.
///
/// Every route that reads a [`TimelineQuery`] — the `/api/v1/timelines/*`
/// endpoints and an account's status listing — builds its query and hands it
/// here, so the rendering, filtering, cursors and headers are stated once.
/// A route with its own order or its own header rule (the pinned profile
/// page) fetches and renders through the same two services and simply writes
/// no `Link`.
///
/// The filter pass runs after the render, and the `Link` keeps its meaning
/// either way: its cursors key on the fetched rows' ids, not the page's
/// survivors, so filtering one status off the page moves no cursor. An
/// anonymous viewer has no filters to read and comes back unfiltered — the
/// ticket's no-op.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`], and
/// [`set_link_header`] reports a `Link` value that is not header-safe as
/// [`AppError::Internal`].
///
/// [`render_statuses`]: crate::services::render_status::render_statuses
/// [`TimelineSource::filter_context`]: crate::services::timeline::TimelineSource::filter_context
pub async fn timeline_response(
    state: &AppState,
    query: &TimelineQuery,
    uri: &Uri,
) -> AppResult<(HeaderMap, Json<Vec<StatusEntity>>)> {
    let rows = query.fetch(&state.db).await?;
    let link = keyset_link(
        &state.config.base_url,
        uri.path(),
        &link_query(uri.query()),
        &rows,
        |row| row.id,
    );
    let rendered = render_statuses(state, ViewerMode::User(query.viewer.as_ref()), &rows).await?;
    let statuses = match query.source.filter_context() {
        Some(context) => {
            filter::apply_active(state, query.viewer.as_ref(), rendered, context).await?
        }
        None => rendered,
    };
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    Ok((headers, Json(statuses)))
}

/// The query parameters a page's `Link` targets repeat: the request's own
/// pairs with the four cursors [`PageParams`] reads (`CURSOR_KEYS`) dropped.
///
/// Each link carries its own cursor, and the next page starts where this one
/// ended. Every other parameter, `limit` included, rides on both links exactly
/// as it arrived, so a followed link re-asks the same filtered listing: the
/// timelines share it with the admin account listings, whose filters mean as
/// much to a second page as a `tag` does.
///
/// [`PageParams`]: crate::pagination::PageParams
#[must_use]
pub fn link_query(query: Option<&str>) -> Vec<(String, String)> {
    form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .filter(|(key, _value)| !CURSOR_KEYS.contains(&key.as_ref()))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

/// A request's tag name in the spelling the `hashtags` column stores.
///
/// Trimmed, leading `#`s dropped, lowercased (see
/// `crate::models::hashtag::normalize_name`, from which this differs only
/// in not capping the length — a name over the storage bound can be no
/// stored tag, and a timeline answers such a lookup the empty page rather
/// than an error). `None` when nothing usable is left.
#[must_use]
pub fn normalize_tag(raw: &str) -> Option<String> {
    let stripped = raw.trim().trim_start_matches('#').trim();
    (!stripped.is_empty()).then(|| stripped.to_lowercase())
}

/// Appends `raw` to `list` as [`normalize_tag`] spells it; a name that
/// normalizes to nothing is dropped, Pleroma's `Enum.filter(& &1)` — an
/// empty `any[]` entry widens the match with nothing.
pub fn push_tag(list: &mut Vec<String>, raw: &str) {
    if let Some(tag) = normalize_tag(raw) {
        list.push(tag);
    }
}

/// Appends `raw` to `list` when it spells one of the five stored
/// visibilities ([`Visibility`]'s `FromStr`, the verbatim lowercase
/// spellings clients send); anything else is ignored, narrowing nothing.
pub fn push_visibility(list: &mut Vec<Visibility>, raw: &str) {
    if let Ok(visibility) = raw.trim().parse::<Visibility>() {
        list.push(visibility);
    }
}

/// Whether a query parameter's text spells a true boolean.
///
/// [`truthy`] is `Some(true)` — `true`, `1`, `on`, `yes` or `t`, trimmed and
/// matched case-insensitively, the reading every flag in the crate shares;
/// anything else (absent, empty, `false`, junk) is `false`.
#[must_use]
pub fn is_true(text: &str) -> bool {
    truthy(text) == Some(true)
}

/// Starts `account`'s deletion: the write the admin `DELETE
/// /api/v1/pleroma/admin/users` and the Mastodon admin `DELETE
/// /api/v1/admin/accounts/{id}` share.
///
/// It is the shape the user-facing `POST /api/pleroma/delete_account`
/// performs: [`mark_deleted`] stamps `deleted_at` and deactivates beside
/// [`delete_all_for_user`]'s revocation of every token, in one transaction,
/// and the [`users.delete`](deletion::KIND_DELETE) sweep goes onto
/// [`QUEUE_BACKGROUND`]. The `Delete` announcement to the peers is the
/// sweep's own first act, not the endpoint's — a client that gets this 200
/// must not wait on federation.
///
/// # Errors
///
/// [`AppError::Database`] from the transaction or the queue write.
///
/// [`mark_deleted`]: crate::models::user::mark_deleted
/// [`delete_all_for_user`]: crate::models::oauth_token::delete_all_for_user
pub async fn start_deletion(state: &AppState, account: &User) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    user::mark_deleted(&mut *tx, account.id).await?;
    let revoked = oauth_token::delete_all_for_user(&mut *tx, account.id).await?;
    tx.commit().await?;
    let queued = enqueue(
        &state.db,
        NewJob {
            queue: QUEUE_BACKGROUND,
            kind: deletion::KIND_DELETE,
            args: deletion::job_args(account.id),
            run_at: None,
            max_attempts: None,
            unique_key: None,
        },
    )
    .await?;
    debug!(
        account = %account.nickname,
        revoked,
        queued = queued.is_some(),
        "started a user deletion"
    );
    Ok(())
}

/// Approves the pending registration `pending` and posts its
/// `account_approved` letter.
///
/// [`set_approved`] clears the staff gate and hands the fresh row back (a
/// row that went away mid-request keeps the stored copy as the answer), and
/// the newly-approved account gets its letter through the mailer queue when
/// it gave an address at all — Pleroma's `send_user_approval_email` has the
/// same guard on its `is_binary(email)` clause.
///
/// The shared half of the admin `PATCH /api/v1/pleroma/admin/users/approve`
/// batch and the Mastodon admin `POST /api/v1/admin/accounts/{id}/approve`:
/// which account is a pending one at all is each endpoint's own check.
///
/// # Errors
///
/// [`AppError::Database`] from the write or the letter's queueing.
///
/// [`set_approved`]: crate::models::user::admin::set_approved
pub async fn approve_account(state: &AppState, pending: User) -> AppResult<User> {
    let Some(updated) = admin::set_approved(&state.db, pending.id).await? else {
        return Ok(pending);
    };
    if let Some(email) = updated.email.as_deref() {
        mailer::deliver_later(state, templates::account_approved(&state.config, email)).await?;
    }
    Ok(updated)
}

/// The distinct `values`, in order of first appearance. One pass over a
/// [`HashSet`], so a body-sized id list costs linear time, not the
/// quadratic `Vec::contains` scan.
pub fn distinct<T, I>(values: I) -> Vec<T>
where
    T: Copy + Eq + Hash,
    I: IntoIterator<Item = T>,
{
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(*value))
        .collect()
}

/// The ids `texts` spell, each named once, in the order sent (see
/// [`distinct`]), plus whether every text was an id at all.
///
/// The strict callers turn a `false` into the 404 an unknown id owes; the
/// lenient ones ignore it, because text that is no id names no row.
pub fn distinct_ids<'a, I>(texts: I) -> (Vec<Uuid>, bool)
where
    I: IntoIterator<Item = &'a str>,
{
    let mut all_ids = true;
    let ids = distinct(texts.into_iter().filter_map(|text| {
        let id = parse_id(text);
        all_ids &= id.is_some();
        id
    }));
    (ids, all_ids)
}

/// [`distinct_ids`], refused with [`TOO_MANY_IDS`] when the distinct ids
/// number more than [`MAX_BATCH_IDS`].
///
/// The count is of ids, not of texts: repeats fold into one and junk text
/// names nothing, so a request is refused only for naming more accounts
/// than the batch routes will do per-id work for. Callers run it before
/// any database read.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`TOO_MANY_IDS`] past the cap.
pub fn capped_distinct_ids<'a, I>(texts: I) -> AppResult<(Vec<Uuid>, bool)>
where
    I: IntoIterator<Item = &'a str>,
{
    let (ids, all_ids) = distinct_ids(texts);
    if ids.len() > MAX_BATCH_IDS {
        return Err(AppError::Unprocessable(TOO_MANY_IDS.to_owned()));
    }
    Ok((ids, all_ids))
}

/// One name list of a repeated parameter: one value or a list (a JSON
/// array, or `types[]=…` keys in a query string) — the two spellings
/// Mastodon clients and forms both send.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_list_reads_a_lone_value_and_an_array() {
        let lone: StringList =
            serde_json::from_value(serde_json::json!("follow")).expect("a lone value");
        assert_eq!(lone.names(), ["follow"], "one name");
        let many: StringList =
            serde_json::from_value(serde_json::json!(["a", "b"])).expect("an array");
        assert_eq!(many.names(), ["a", "b"], "the names in the order sent");
        assert!(names_of(None).is_empty(), "absent is no names");
        assert_eq!(names_of(Some(&many)), ["a", "b"], "present is its names");
    }

    /// One of the ids the fixtures pass around.
    const ID_A: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// A second id, distinct from [`ID_A`].
    const ID_B: &str = "01934567-89ab-7cde-f012-3456789abcdf";

    /// The ids [`requested_ids`] found in `query`, as strings, so the
    /// assertions read like the query they came from.
    fn parsed(query: Option<&str>) -> Vec<String> {
        requested_ids(query)
            .into_iter()
            .map(|id| id.to_string())
            .collect()
    }

    #[test]
    fn array_params_are_read_in_order() {
        assert_eq!(
            parsed(Some(&format!("id[]={ID_A}&id[]={ID_B}"))),
            vec![ID_A, ID_B],
            "Mastodon's repeated bracketed params, in the order sent"
        );
        assert_eq!(
            parsed(Some(&format!("id[]={ID_A}&other=x&id[]={ID_B}"))),
            vec![ID_A, ID_B],
            "unrelated params in between are ignored"
        );
        assert_eq!(
            parsed(Some(&format!("id[]={ID_A}&id[]={ID_A}"))),
            vec![ID_A, ID_A],
            "a repeated id is kept twice: the response answers what was asked"
        );
    }

    #[test]
    fn a_single_id_param_is_accepted() {
        assert_eq!(
            parsed(Some(&format!("id={ID_B}"))),
            vec![ID_B],
            "Pleroma's List.wrap accepts the single form"
        );
        assert_eq!(
            parsed(Some(&format!("id={ID_B}&id[]={ID_A}"))),
            vec![ID_B, ID_A],
            "and the two forms merge in occurrence order"
        );
    }

    #[test]
    fn values_are_percent_decoded_and_garbage_is_skipped() {
        assert_eq!(
            parsed(Some(&format!("id%5B%5D={ID_A}"))),
            vec![ID_A],
            "the bracketed key may arrive percent-encoded"
        );
        assert_eq!(
            parsed(Some("id[]=not-an-id")),
            Vec::<String>::new(),
            "a value that is not id text is skipped, not an error"
        );
        assert_eq!(
            parsed(Some(&format!("id[]={ID_B}&id=oops&id[]={ID_A}"))),
            vec![ID_B, ID_A],
            "one bad value does not sink its neighbours"
        );
    }

    #[test]
    fn nothing_usable_is_the_empty_list() {
        for query in [None, Some(""), Some("id="), Some("other=1"), Some("id")] {
            assert_eq!(
                parsed(query),
                Vec::<String>::new(),
                "`{query:?}` carries no usable id"
            );
        }
    }

    /// The ids [`requested_status_ids`] found in `query`, as strings.
    fn status_parsed(query: Option<&str>) -> Vec<String> {
        requested_status_ids(query)
            .into_iter()
            .map(|id| id.to_string())
            .collect()
    }

    #[test]
    fn the_status_batch_also_reads_the_ids_alias() {
        assert_eq!(
            status_parsed(Some(&format!("ids[]={ID_A}&ids[]={ID_B}"))),
            vec![ID_A, ID_B],
            "Pleroma's deprecated spelling arrives in occurrence order too"
        );
        assert_eq!(
            status_parsed(Some(&format!("ids={ID_B}"))),
            vec![ID_B],
            "and the alias has its own unwrapped form"
        );
        assert_eq!(
            status_parsed(Some(&format!("id[]={ID_A}&ids%5B%5D={ID_B}"))),
            vec![ID_A],
            "`id` wins outright, so the `ids` value alongside it is never read"
        );
    }

    #[test]
    fn the_status_batch_falls_back_only_when_id_carries_nothing() {
        assert_eq!(
            status_parsed(Some(&format!("id[]={ID_A}&id[]={ID_B}"))),
            vec![ID_A, ID_B],
            "the documented spelling is read as `requested_ids` reads it"
        );
        assert_eq!(
            status_parsed(Some("id[]=not-an-id&ids[]=also-not")),
            Vec::<String>::new(),
            "nothing usable under either spelling is the empty list"
        );
        assert_eq!(
            status_parsed(None),
            Vec::<String>::new(),
            "no query at all is the empty list, not an error"
        );
    }

    #[test]
    fn the_link_repeats_the_filters_and_drops_the_cursors() {
        let extras = link_query(Some(
            "any[]=a&local=1&limit=2&max_id=old&min_id=new&since_id=s&offset=3",
        ));
        assert_eq!(
            extras,
            vec![
                ("any[]".to_owned(), "a".to_owned()),
                ("local".to_owned(), "1".to_owned()),
                ("limit".to_owned(), "2".to_owned()),
            ],
            "filters and `limit` ride on, the cursors and `offset` do not"
        );
        assert!(link_query(None).is_empty(), "no query repeats nothing");
    }

    /// The pair reader's own rules: an absent key, an empty value, the order
    /// of a list and the percent-decoding both listings rely on.
    #[test]
    fn the_pair_reader_folds_duplicates_and_drops_empties() {
        let pairs = QueryPairs::parse(Some("a=1&a=&b=2&c[]=x&a[]=y&empty=&empty[]=&d=a%20b"));
        assert_eq!(pairs.value("a").as_deref(), None, "the empty last value");
        assert_eq!(pairs.value("b").as_deref(), Some("2"), "an ordinary value");
        assert_eq!(pairs.value("d").as_deref(), Some("a b"), "a decoded value");
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

    #[test]
    fn normalize_tag_lowercases_strips_hash_and_rejects_nothing_usable() {
        assert_eq!(normalize_tag("#Foo").as_deref(), Some("foo"));
        assert_eq!(normalize_tag("##Bar ").as_deref(), Some("bar"));
        assert_eq!(normalize_tag(" Café ").as_deref(), Some("café"));
        assert_eq!(normalize_tag("#").as_deref(), None, "a lone `#` is empty");
        assert_eq!(
            normalize_tag("  #  ").as_deref(),
            None,
            "whitespace is empty"
        );
        let mut names = Vec::new();
        push_tag(&mut names, "#One");
        push_tag(&mut names, "");
        push_tag(&mut names, "two");
        assert_eq!(
            names,
            ["one", "two"],
            "`push_tag` keeps the usable names in order and drops the rest"
        );
    }

    #[test]
    fn the_shared_readers_spell_true_and_a_stored_visibility() {
        for text in ["true", "1", "TRUE", " true ", "yes", "on", "t"] {
            assert!(is_true(text), "`{text}` spells true");
        }
        for text in ["", " ", "false", "0", "no", "junk"] {
            assert!(!is_true(text), "`{text}` is not true here");
        }
        let mut kept = Vec::new();
        push_visibility(&mut kept, " public ");
        push_visibility(&mut kept, "Public");
        push_visibility(&mut kept, "direct");
        push_visibility(&mut kept, "nonsense");
        assert_eq!(
            kept,
            vec![Visibility::Public, Visibility::Direct],
            "the verbatim spellings count, a capitalization and junk do not"
        );
    }

    #[test]
    fn distinct_keeps_first_occurrences_in_order() {
        assert_eq!(distinct([3, 1, 3, 2, 1, 2]), vec![3, 1, 2]);
        assert_eq!(distinct([7, 7, 7]), vec![7], "all repeats fold into one");
        assert_eq!(distinct([1, 2, 3]), vec![1, 2, 3], "no repeats: unchanged");
    }

    #[test]
    fn distinct_of_nothing_is_empty() {
        assert_eq!(distinct(Vec::<u8>::new()), Vec::<u8>::new());
    }

    #[test]
    fn distinct_ids_parses_dedupes_and_keeps_order() {
        let (ids, all_ids) = distinct_ids([ID_B, ID_A, ID_B, ID_A]);
        assert!(all_ids, "every text was an id");
        assert_eq!(
            ids.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec![ID_B, ID_A],
            "first-occurrence order"
        );
    }

    #[test]
    fn distinct_ids_flags_junk_and_keeps_the_ids_around_it() {
        let (found, every_text_parsed) = distinct_ids([ID_A, "junk", ID_A, ""]);
        assert!(!every_text_parsed, "junk clears the flag");
        assert_eq!(found.len(), 1, "the ids around the junk are still kept");
    }

    /// `count` distinct id texts (the simple spelling of 1, 2, ...).
    fn id_texts(count: usize) -> Vec<String> {
        (1..=count).map(|n| format!("{n:032x}")).collect()
    }

    #[test]
    fn capped_distinct_ids_allows_exactly_the_cap() {
        let texts = id_texts(MAX_BATCH_IDS);
        let (ids, all_ids) = capped_distinct_ids(texts.iter().map(String::as_str))
            .expect("the cap itself is allowed");
        assert_eq!(ids.len(), MAX_BATCH_IDS);
        assert!(all_ids);
    }

    #[test]
    fn capped_distinct_ids_refuses_one_past_the_cap() {
        let texts = id_texts(MAX_BATCH_IDS + 1);
        let err = capped_distinct_ids(texts.iter().map(String::as_str))
            .expect_err("one past the cap is refused");
        assert!(matches!(&err, AppError::Unprocessable(m) if m.as_str() == TOO_MANY_IDS));
    }

    #[test]
    fn capped_distinct_ids_counts_distinct_ids_only() {
        let mut texts = id_texts(MAX_BATCH_IDS);
        texts.extend(id_texts(MAX_BATCH_IDS));
        texts.push("junk".to_owned());
        let (ids, all_ids) = capped_distinct_ids(texts.iter().map(String::as_str))
            .expect("repeats and junk do not count");
        assert_eq!(ids.len(), MAX_BATCH_IDS);
        assert!(!all_ids, "the junk is still flagged");
    }

    #[test]
    fn distinct_ids_of_nothing_is_complete_and_empty() {
        let (none, nothing_wrong) = distinct_ids(Vec::<&str>::new());
        assert_eq!(none, Vec::<Uuid>::new(), "no ids");
        assert!(nothing_wrong, "nothing asked: nothing wrong");
    }
}
