//! The admin API's reads and writes over `statuses` (WBE-201).
//!
//! Three listings share one statement here, because Pleroma runs all three
//! through the same `ActivityPub.fetch_statuses/2` with a different filter
//! set: the whole-network ledger behind
//! `GET /api/v1/pleroma/admin/statuses` (`AdminAPI.StatusController.index`,
//! `lib/pleroma/web/admin_api/controllers/status_controller.ex`), one
//! account's posts behind `GET …/users/{nickname}/statuses`
//! (`AdminAPIController.list_user_statuses`) and one domain's behind
//! `GET …/instances/{instance}/statuses` (`AdminAPI.InstanceController.list_statuses`).
//! [`list`] answers a page of them and [`count`] the whole set the same
//! filters select, which is the `{activities, total}` envelope the two
//! narrower routes carry.
//!
//! The filters are Pleroma's, restated over decision 6's relational rows:
//!
//! - `godmode` is the visibility question. Off, the listing is what an
//!   anonymous reader may see — `public` and `unlisted` only; on, it is
//!   every stored visibility, `private`, `direct` and Pleroma's `local`
//!   included (`PUBLIC_ARMS` against `ALL_ARMS`). Pleroma reaches the
//!   same pair through `Filters.visible(nil, opts)`, whose `godmode: true`
//!   arm lifts the scope list to all five.
//! - `local_only` keeps the statuses whose author this instance owns
//!   (`users.local`), the same distinction
//!   [`super::status_visibility_counts`] draws between its own counters and
//!   one domain's.
//! - `with_reblogs` decides whether boost rows are listed at all: off is
//!   Pleroma's `exclude_reblogs: true`, and one fewer row per boosted post
//!   in the ledger (`LIVE` holds the arms [`list`] always applies).
//! - the author and the domain narrow one listing to one account's posts
//!   or to one host's mirrors; a domain no account came from simply
//!   selects nothing.
//!
//! Pages are offset windows — `page` from 1, `page_size` rows — because
//! that is what the admin API's clients ask for, newest post first on the
//! `UUIDv7` id (decision 4) rather than the keyset cursors the client
//! timelines page by.
//!
//! The one writer is [`set_scope`]: the admin's change of who may see a
//! post and whether it starts folded — Pleroma's
//! `CommonAPI.update_activity_scope` (`toggle_sensitive` then
//! `set_visibility`), which rewrites exactly those two fields of the object
//! and nothing else. Unlike [`crate::models::status_edit`]'s edit write it
//! records no revision and does not move `edited_at`: a scope change is not
//! a change of what was said.

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        status::{Status, Visibility},
        user::Flag,
    },
};

/// The head of [`list`]'s statement: every status joined to its author,
/// which is the row `local_only` and the domain filter weigh.
const LIST_SQL: &str = "SELECT s.* FROM statuses s JOIN users u ON u.id = s.user_id";

/// The head of [`count`]'s statement: the same rows, counted.
const COUNT_SQL: &str = "SELECT count(*) FROM statuses s JOIN users u ON u.id = s.user_id";

/// The arms every admin listing applies: a tombstone is nobody's ledger
/// entry, on either side of `godmode`.
const LIVE: &str = " WHERE s.deleted_at IS NULL";

/// The visibility arms of a listing without `godmode` — what an anonymous
/// reader may see, which is the audience Pleroma's `fetch_statuses(nil, …)`
/// states for a staff listing that has not asked for more.
const PUBLIC_ARMS: &str = " AND s.visibility IN ('public', 'unlisted')";

/// The visibility arms of a `godmode` listing: all five stored visibilities,
/// stated as the `CHECK` constraint lists them. Pleroma's
/// `Constants.visibility_scopes/1` for the same case.
const ALL_ARMS: &str = " AND s.visibility IN ('public', 'unlisted', 'private', 'direct', 'local')";

/// The arm that drops boost rows, which a listing without `with_reblogs`
/// never shows (Pleroma's `exclude_reblogs: true`).
const NO_REBLOGS: &str = " AND s.reblog_of_id IS NULL";

/// The arm `local_only` adds: the author is this instance's own account.
const LOCAL_AUTHORS: &str = " AND u.local";

/// The filters and window of one [`list`] (or [`count`]) read, as the three
/// admin status listings parse them (Pleroma's `StatusController.index`,
/// `list_user_statuses` and `InstanceController.list_statuses` params).
///
/// The three filters are [`Flag`]s for the reason [`TimelineFilters`] gives
/// its own: a params struct of bare booleans is what Clippy's denied
/// `struct_excessive_bools` refuses.
///
/// [`Flag`]: crate::models::user::Flag
/// [`TimelineFilters`]: crate::services::timeline::TimelineFilters
#[derive(Debug)]
pub struct ListParams {
    /// One-based page number; the caller defaults and validates it (1).
    pub page: i64,
    /// Rows per page; the caller defaults and caps it (50, max 200).
    pub page_size: i64,
    /// Whether the private visibilities are listed: off is `public`
    /// and `unlisted` only.
    pub godmode: Flag,
    /// Whether only statuses of local accounts are listed.
    pub local_only: Flag,
    /// Whether boost rows are listed beside the posts themselves.
    pub with_reblogs: Flag,
    /// Only this account's statuses; `None` is every author.
    pub user_id: Option<Uuid>,
    /// Only statuses of accounts mirrored from this domain; `None` is every
    /// host. The `local` literal of `GET /instances/{instance}/statuses` is
    /// the caller's `local_only`, which is why this is a plain domain.
    pub domain: Option<String>,
}

/// One admin status page: `params`' window of the ledger, newest first.
///
/// The window is the whole answer of the network-wide listing; the two
/// narrower listings put it beside [`count`]'s total, which is the
/// `{activities, total}` envelope Pleroma's `AdminAPI.StatusView` renders
/// for them.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list(pool: &PgPool, params: &ListParams) -> AppResult<Vec<Status>> {
    let mut builder = QueryBuilder::<Postgres>::new(LIST_SQL);
    push_filters(&mut builder, params);
    let _window: &mut _ = builder
        .push(" ORDER BY s.id DESC LIMIT ")
        .push_bind(params.page_size)
        .push(" OFFSET ")
        .push_bind((params.page - 1).saturating_mul(params.page_size));
    let statuses = builder.build_query_as::<Status>().fetch_all(pool).await?;
    Ok(statuses)
}

/// The number of statuses `params`' filters select, whatever page they
/// land on — the `total` of the two envelope listings.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn count(pool: &PgPool, params: &ListParams) -> AppResult<i64> {
    let mut builder = QueryBuilder::<Postgres>::new(COUNT_SQL);
    push_filters(&mut builder, params);
    let total = builder.build_query_scalar::<i64>().fetch_one(pool).await?;
    Ok(total)
}

/// Appends the shared `WHERE` clause of [`list`] and [`count`]: the live
/// arm always, then the visibility arms `godmode` decides, the boost arm
/// `with_reblogs` decides, and the three narrowing filters — `local_only`,
/// one author, one domain — each pushed only when it selects something.
/// The clause leads with [`LIVE`] so every filter after it is one more
/// `AND`, the shape [`super::status_visibility_counts`] gives its own
/// counters.
fn push_filters(builder: &mut QueryBuilder<Postgres>, params: &ListParams) {
    let _live: &mut _ = builder.push(LIVE);
    let _visibility: &mut _ = builder.push(if params.godmode.get() {
        ALL_ARMS
    } else {
        PUBLIC_ARMS
    });
    if !params.with_reblogs.get() {
        let _boosts: &mut _ = builder.push(NO_REBLOGS);
    }
    if params.local_only.get() {
        let _local: &mut _ = builder.push(LOCAL_AUTHORS);
    }
    if let Some(user_id) = params.user_id {
        let _author: &mut _ = builder.push(" AND s.user_id = ").push_bind(user_id);
    }
    if let Some(domain) = &params.domain {
        let _host: &mut _ = builder.push(" AND u.domain = ").push_bind(domain);
    }
}

/// [`set_scope`]'s write: the two scope columns, each held by `COALESCE`
/// when its parameter is `NULL` (the absent field), `updated_at` moved, and
/// the `deleted_at IS NULL` guard that keeps a tombstone from taking a new
/// scope. `RETURNING *` answers the row as it now stands for the render.
///
/// `$3` is cast to `text` so the `COALESCE` has the column's own type to
/// fall back to: the `Visibility` enum arrives as `sqlx`'s `text`, and an
/// absent one is a plain `NULL` of that type.
const SET_SCOPE_SQL: &str = "UPDATE statuses
     SET sensitive = COALESCE($2, sensitive),
         visibility = COALESCE($3::text, visibility),
         updated_at = now()
   WHERE id = $1 AND deleted_at IS NULL
RETURNING *";

/// The admin's scope write: the two fields of one live status.
///
/// `PUT /api/v1/pleroma/admin/statuses/{id}`'s write moves the two scope
/// fields of the live row `id` names, each only where the request presented
/// it (`None` is "leave this column alone", Pleroma's `toggle_sensitive`
/// and `set_visibility` clauses that fall through when the parameter is
/// absent).
///
/// `RETURNING *` answers the post as it now stands, which is what the
/// endpoint renders; `None` is the row a concurrent delete tombstoned
/// between the endpoint's gate and this write — the `deleted_at IS NULL`
/// arm of the update statement is the whole guard.
///
/// Nothing else moves: the text, the warnings, the mentions, the counters
/// and [`edited_at`](Status::edited_at) are the author's edit surface, not
/// the moderation one, and only `updated_at` rides the change.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_scope(
    pool: &PgPool,
    id: Uuid,
    sensitive: Option<bool>,
    visibility: Option<Visibility>,
) -> AppResult<Option<Status>> {
    let row = sqlx::query_as::<_, Status>(SET_SCOPE_SQL)
        .bind(id)
        .bind(sensitive)
        .bind(visibility)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

#[cfg(test)]
mod tests;
