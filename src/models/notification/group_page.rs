//! The grouped listing and the single-group reads: the shared
//! aggregates ([`GROUP_AGGREGATES`]), the metadata rows ([`GroupInfo`]
//! and [`GroupEntry`]), [`GroupPageParams`] with the listing's query
//! builders ([`push_keyed_groups`] and [`push_group_page`]),
//! [`group_page`], [`group_info`] and [`group_actor_ids`].

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, pagination::PageParams};

use super::{
    GroupSelector,
    flat_page::{NotificationFilters, push_scope},
    grouping::{UNGROUNDED_PREFIX, sample_slice},
};

/// The per-group aggregates the listing and the single-group read share:
/// counts, newest and oldest member ids, the newest timestamp, and the
/// type and status a member carries (a group is type-uniform by
/// construction; the status is constant inside a stored key and
/// `NULL`-safely picked otherwise). PostgreSQL has no `min`/`max` over
/// `uuid`, so the id extremes are the ends of an ordered `array_agg`.
const GROUP_AGGREGATES: &str = "
       count(*) AS notifications_count,
       min(type) AS notification_type,
       (array_agg(id ORDER BY id DESC))[1] AS most_recent_id,
       (array_agg(id ORDER BY id))[1] AS page_min_id,
       max(inserted_at) AS latest_page_notification_at,
       (array_agg(status_id ORDER BY id DESC) FILTER (WHERE status_id IS NOT NULL))[1]
           AS status_id,";

/// One group's metadata, as the reads aggregate it.
///
/// The newest members' actors ride along as
/// [`sample_account_ids`](GroupInfo::sample_account_ids) (at most
/// [`SAMPLE_SIZE`](super::SAMPLE_SIZE), newest first, still possibly repeating an actor — the
/// entity deduplicates, Pleroma's `Enum.uniq_by` step).
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct GroupInfo {
    /// How many (undismissed) notifications the group holds.
    pub notifications_count: i64,
    /// The group's type, from a member (all members share it).
    pub notification_type: String,
    /// The group's newest notification id: `most_recent_notification_id`,
    /// the cursor the listing pages on, and `page_max_id` — one page of a
    /// grouped listing is that whole group, so the page extreme and the
    /// global extreme coincide (Pleroma's rows select `max(n.id)` twice).
    pub most_recent_id: Uuid,
    /// The group's oldest notification id (`page_min_id`).
    pub page_min_id: Uuid,
    /// When its newest notification was created
    /// (`latest_page_notification_at`).
    pub latest_page_notification_at: DateTime<Utc>,
    /// The status the group is about, when it names one.
    pub status_id: Option<Uuid>,
    /// The actors of up to [`SAMPLE_SIZE`](super::SAMPLE_SIZE) newest members, newest first.
    pub sample_account_ids: Vec<Uuid>,
}

/// One entry of a grouped page: the group's key as the request computed it,
/// with its [`GroupInfo`] flattened beside it.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct GroupEntry {
    /// The group key: a stored `group_key`, or the derived
    /// `ungrouped-<id>` (the listing's `CASE`, Pleroma's
    /// `notification_group_keyed_query`).
    pub group_key: String,
    /// The group's aggregates and sample.
    #[sqlx(flatten)]
    pub info: GroupInfo,
}

/// Everything [`group_page`] needs: whose notifications, grouped over
/// which types, filtered how, and which page of groups.
#[derive(Clone, Debug)]
pub struct GroupPageParams<'a> {
    /// The recipient account.
    pub user_id: Uuid,
    /// The types that keep their stored group key this request (the
    /// effective `grouped_types[]`: the request's list, or
    /// [`GROUPABLE_TYPES`](super::GROUPABLE_TYPES) when it named none). Anything outside it reads
    /// as its own `ungrouped-<id>` key.
    pub grouped_types: Vec<&'a str>,
    /// The client's `types[]` / `exclude_types[]` / `account_id` filters.
    pub filters: NotificationFilters<'a>,
    /// The cursor page over groups, keyed on each group's `most_recent_id`.
    pub page: PageParams,
}

/// The listing query's head: the scoped rows with their this-request group
/// key computed — the stored key when the type is in the caller's grouped
/// set, the derived `ungrouped-<id>` otherwise (Pleroma's
/// `notification_group_keyed_query` `CASE`) — aggregated per group. The
/// statement stays open at the `GROUP BY` for [`push_group_page`]'s cursors.
fn push_keyed_groups(qb: &mut QueryBuilder<Postgres>, params: &GroupPageParams<'_>) {
    let _head: &mut _ = qb
        .push(
            "WITH keyed AS (SELECT n.id, n.inserted_at, n.type, n.status_id, n.from_account_id, \
             CASE WHEN n.group_key IS NOT NULL AND n.type = ANY(",
        )
        .push_bind(params.grouped_types.clone())
        .push(") THEN n.group_key ELSE '")
        .push(UNGROUNDED_PREFIX)
        .push("' || n.id::text END AS key FROM notifications n");
    push_scope(qb, params.user_id);
    params.filters.push(qb);
    let slice = sample_slice();
    let _grouped: &mut _ = qb
        .push("), grouped AS (SELECT key,")
        .push(GROUP_AGGREGATES)
        .push(" (array_agg(from_account_id ORDER BY id DESC))")
        .push(&slice)
        .push(" AS sample_account_ids FROM keyed GROUP BY key");
}

/// The cursor conditions on each group's newest id — the `max_id` /
/// `min_id` / `since_id` of [`PageParams::apply_keyset`] lifted from rows
/// to groups (Pleroma's `apply_group_cursor_filters` `HAVING`ing
/// `max(n.id)`) — then the page order and size. A `min_id` page reads
/// forward, [`PageParams::apply_keyset`]'s rule, and comes back flipped by
/// [`PageParams::finish`].
fn push_group_page(qb: &mut QueryBuilder<Postgres>, page: &PageParams) {
    const NEWEST: &str = "(array_agg(id ORDER BY id DESC))[1]";
    if page.max_id.is_some() || page.min_id.is_some() || page.since_id.is_some() {
        let _having: &mut _ = qb.push(" HAVING true");
    }
    if let Some(max_id) = page.max_id {
        let _lt: &mut _ = qb.push(" AND ").push(NEWEST).push(" < ").push_bind(max_id);
    }
    if let Some(min_id) = page.min_id {
        let _gt: &mut _ = qb.push(" AND ").push(NEWEST).push(" > ").push_bind(min_id);
    }
    if let Some(since_id) = page.since_id {
        let _gt: &mut _ = qb
            .push(" AND ")
            .push(NEWEST)
            .push(" > ")
            .push_bind(since_id);
    }
    let _ordered: &mut _ = qb
        .push(" ORDER BY ")
        .push(NEWEST)
        .push(if page.min_id.is_some() {
            " ASC"
        } else {
            " DESC"
        });
    let _limited: &mut _ = qb.push(" LIMIT ").push_bind(page.limit);
}

/// One page of groups as [`GroupEntry`] rows, newest group first.
///
/// The page walks groups by their `most_recent_id`, so a group never
/// splits across pages: `max_id` keeps groups whose newest notification is
/// older than the cursor, `since_id` and `min_id` newer, and a `min_id`
/// page is read forward and answered flipped.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn group_page(pool: &PgPool, params: &GroupPageParams<'_>) -> AppResult<Vec<GroupEntry>> {
    let mut qb = QueryBuilder::<Postgres>::new("");
    push_keyed_groups(&mut qb, params);
    push_group_page(&mut qb, &params.page);
    let order = if params.page.min_id.is_some() {
        " ASC"
    } else {
        " DESC"
    };
    let _tail: &mut _ = qb.push(
        ") SELECT g.key AS group_key, g.notifications_count, g.notification_type, \
         g.most_recent_id, g.page_min_id, g.latest_page_notification_at, g.status_id, \
         g.sample_account_ids FROM grouped g ORDER BY g.most_recent_id",
    );
    let _ordered: &mut _ = qb.push(order);
    let rows = qb.build_query_as::<GroupEntry>().fetch_all(pool).await?;
    Ok(params.page.finish(rows))
}

/// One group's metadata by its [`GroupSelector`].
///
/// The `ungrouped-<id>` key reports its single notification (Pleroma's
/// `Notification.get` clause), a stored key aggregates all of the group's
/// rows. `None` when the caller owns no undismissed group under that
/// selector — the endpoint's 404.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn group_info(
    pool: &PgPool,
    user_id: Uuid,
    selector: &GroupSelector,
) -> AppResult<Option<GroupInfo>> {
    match selector {
        GroupSelector::Notification(id) => single_info(pool, user_id, *id).await,
        GroupSelector::GroupKey(key) => keyed_info(pool, user_id, key).await,
    }
}

/// The metadata of the one notification an `ungrouped-<id>` key names: the
/// row as a one-member group.
async fn single_info(pool: &PgPool, user_id: Uuid, id: Uuid) -> AppResult<Option<GroupInfo>> {
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT 1::bigint AS notifications_count, n.type AS notification_type, \
         n.id AS most_recent_id, n.id AS page_min_id, n.inserted_at \
         AS latest_page_notification_at, n.status_id, \
         ARRAY[n.from_account_id]::uuid[] AS sample_account_ids FROM notifications n",
    );
    push_scope(&mut qb, user_id);
    let _id: &mut _ = qb.push(" AND n.id = ").push_bind(id);
    let row = qb
        .build_query_as::<GroupInfo>()
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The metadata of one stored-key group, aggregated over its rows; `None`
/// when it holds none.
async fn keyed_info(pool: &PgPool, user_id: Uuid, key: &str) -> AppResult<Option<GroupInfo>> {
    let slice = sample_slice();
    let mut qb = QueryBuilder::<Postgres>::new("SELECT");
    let _aggregates: &mut _ = qb
        .push(GROUP_AGGREGATES)
        .push(" (array_agg(from_account_id ORDER BY id DESC))")
        .push(&slice)
        .push(" AS sample_account_ids FROM notifications n");
    push_scope(&mut qb, user_id);
    let _key: &mut _ = qb.push(" AND n.group_key = ").push_bind(key.to_owned());
    let _grouped: &mut _ = qb.push(" GROUP BY n.group_key");
    let row = qb
        .build_query_as::<GroupInfo>()
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// The actor accounts of one group.
///
/// Newest actor first, each named once (Mastodon's group-accounts listing;
/// Pleroma's `get_notification_group_accounts` `distinct(true)`). The
/// `{group_key}/accounts` endpoint renders these ids.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn group_actor_ids(
    pool: &PgPool,
    user_id: Uuid,
    selector: &GroupSelector,
) -> AppResult<Vec<Uuid>> {
    let mut qb = QueryBuilder::<Postgres>::new("SELECT n.from_account_id FROM notifications n");
    push_scope(&mut qb, user_id);
    match selector {
        GroupSelector::Notification(id) => {
            let _id: &mut _ = qb.push(" AND n.id = ").push_bind(*id);
        }
        GroupSelector::GroupKey(key) => {
            let _key: &mut _ = qb.push(" AND n.group_key = ").push_bind(key.clone());
            let _ordered: &mut _ = qb.push(
                " GROUP BY n.from_account_id ORDER BY (array_agg(n.id ORDER BY n.id DESC))[1] DESC",
            );
        }
    }
    let ids = qb.build_query_scalar::<Uuid>().fetch_all(pool).await?;
    Ok(ids)
}
