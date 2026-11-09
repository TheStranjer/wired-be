//! The `notifications` table: one account being told something happened.
//!
//! Reference: Pleroma's `Pleroma.Notification` (`lib/pleroma/notification.ex`)
//! and the grouped reads of `Pleroma.Web.MastodonAPI.MastodonAPI`
//! (`lib/pleroma/web/mastodon_api/mastodon_api.ex`, the
//! `get_grouped_notification_page` / `notification_group_*` family the v2
//! API calls). What that side calls the *group key* is [`grouped_key`]
//! here, computed once at insert and stored in the `group_key` column:
//! `<type>-<status_id>` for the groupable types — `<type>-none` when the
//! notification names no status — with the emoji appended for a
//! `pleroma:emoji_reaction` so two reactions to one status are two groups.
//! Every other type is ungrouped and reads as the unique key
//! `ungrouped-<notification id>`, spelled by [`UNGROUNDED_PREFIX`] and
//! derived from the row rather than stored.
//!
//! The grouped reads are the ones [`crate::api::notifications_v2`] answers
//! with: [`group_page`] the grouped listing (one page of groups, newest
//! group first, each entry carrying up to [`SAMPLE_SIZE`] of its newest
//! members' actors), [`group_info`] one group's metadata,
//! [`group_actor_ids`] every actor of one group, [`dismiss_group`] the
//! group dismissal, and [`unread_count`] the capped count of unseen
//! undismissed notifications past the caller's marker. The flat reads and
//! writes are the ones [`crate::api::notifications`] answers with:
//! [`create`] the one raw insert, [`maybe_notify`] the insert that honours
//! the recipient's `block_from_strangers` setting,
//! [`get`] one row by id, [`get_visible`] the same row as the listings
//! show it, [`page_for_user`] the flat listing under the client filters
//! and the mute exclusions, [`dismiss`] / [`dismiss_all`] / [`dismiss_ids`]
//! the three dismissal shapes, [`mark_read_through_id`] the read marking
//! Pleroma's `set_read_up_to` performs, and [`delete_for_status`] the
//! cleanup that takes a deleted status's notifications with it.
//!
//! Dismissal is soft: the writes stamp [`dismissed_at`](Notification::dismissed_at)
//! and every read hides the stamped rows rather than deleting them. Every
//! read and write is scoped to the recipient (`user_id`), skips dismissed
//! rows, and — as Pleroma's `for_user_query` does — skips rows whose
//! actor's account is deactivated, so an invisible account silently leaves
//! its group.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{follow, user, user::notification_settings::NotificationSettings},
    pagination::PageParams,
    util::id::{new_id, parse_id},
};

/// The favourite type: groupable, keyed by the favourited status.
pub const TYPE_FAVOURITE: &str = "favourite";
/// The reblog type: groupable, keyed by the reblogged status.
pub const TYPE_REBLOG: &str = "reblog";
/// The Pleroma emoji-reaction type: groupable, keyed by the status *and*
/// the emoji, so the key carries the distinction Pleroma's view renders as
/// the group's `emoji` key.
pub const TYPE_EMOJI_REACTION: &str = "pleroma:emoji_reaction";
/// The follow-request type: the one type [`maybe_notify`] never withholds
/// from an account that blocks strangers.
///
/// Pleroma applies `block_from_strangers` to every other type — a
/// stranger's *follow* needs the recipient to follow them back first,
/// which is precisely what an incoming request is not.
pub const TYPE_FOLLOW_REQUEST: &str = "follow_request";

/// The types the grouped API merges across notifications.
///
/// Every other type keeps the unique [`UNGROUNDED_PREFIX`] key. Pleroma's
/// `@groupable_notification_types` with the follow dropped (this project's
/// grouping keeps only the status-keyed types: a status-less follow would
/// merge every follow on earth into `follow-none`) and the emoji reaction
/// added.
pub const GROUPABLE_TYPES: [&str; 3] = [TYPE_FAVOURITE, TYPE_REBLOG, TYPE_EMOJI_REACTION];

/// The prefix of the unique key an ungrouped notification answers to:
/// `ungrouped-<id>`, Pleroma's `ungrouped_group_key/1`.
pub const UNGROUNDED_PREFIX: &str = "ungrouped-";

/// How many of a group's most recent members feed its
/// `sample_account_ids` (Pleroma's `@notification_group_sample_limit`,
/// Mastodon's group sample size).
pub const SAMPLE_SIZE: usize = 8;

/// Whether `notification_type` is one of the status-keyed types the API
/// groups, and so gets a stored [`grouped_key`].
#[must_use]
pub fn is_groupable(notification_type: &str) -> bool {
    GROUPABLE_TYPES.contains(&notification_type)
}

/// The stored group key of `notification_type`, or `None` when the type is
/// not groupable.
///
/// `<type>-<status_id>`, `<type>-none` for a notification that names no
/// status, and `<type>-<status_id>-<emoji>` for a reaction. A reaction row
/// that stored no emoji falls back to the plain status key, so a key never
/// ends in a dangling `-`.
#[must_use]
pub fn grouped_key(
    notification_type: &str,
    status_id: Option<Uuid>,
    emoji: Option<&str>,
) -> Option<String> {
    if !is_groupable(notification_type) {
        return None;
    }
    let subject = status_id.map_or_else(|| "none".to_owned(), |id| id.to_string());
    let mut key = format!("{notification_type}-{subject}");
    if notification_type == TYPE_EMOJI_REACTION
        && let Some(emoji) = emoji
    {
        key.push('-');
        key.push_str(emoji);
    }
    Some(key)
}

/// The [`GroupSelector`] a client's `{group_key}` path segment names: the
/// row behind an `ungrouped-<id>` spelling, or the stored group key itself
/// (Pleroma's two `get_notification_group*` clauses).
#[must_use]
pub fn select_group(group_key: &str) -> GroupSelector {
    group_key
        .strip_prefix(UNGROUNDED_PREFIX)
        .and_then(parse_id)
        .map_or_else(
            || GroupSelector::GroupKey(group_key.to_owned()),
            GroupSelector::Notification,
        )
}

/// Which notifications one `{group_key}` addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupSelector {
    /// The unique key of one notification: `ungrouped-<id>`, matched on the
    /// row id like Pleroma's `"ungrouped-" <> notification_id` clauses.
    Notification(Uuid),
    /// A stored group key, matched on the `group_key` column.
    GroupKey(String),
}

/// The array-subscript slice text that caps a sample array at
/// [`SAMPLE_SIZE`], built from the constant so the SQL and the documented
/// cap cannot drift.
fn sample_slice() -> String {
    let size = i64::try_from(SAMPLE_SIZE).unwrap_or(i64::MAX);
    format!("[1:{size}]")
}

/// One row of `notifications`: one account's notification of one event.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Notification {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4); its text
    /// order is the notification's place in the account's timeline.
    pub id: Uuid,
    /// The recipient account.
    pub user_id: Uuid,
    /// The account that caused the notification (Mastodon's
    /// `from_account_id`).
    pub from_account_id: Uuid,
    /// Mastodon's type name as stored. Called `event_type` to keep it from
    /// reading as `Notification::notification_type`; the column is `type`.
    #[sqlx(rename = "type")]
    pub event_type: String,
    /// The status this notification is about; `None` for the status-less
    /// types.
    pub status_id: Option<Uuid>,
    /// The emoji of a reaction; `None` for every other type.
    pub emoji: Option<String>,
    /// The report of a `pleroma:report` notification; `None` otherwise.
    pub report_id: Option<Uuid>,
    /// The stored group key ([`grouped_key`]); `None` exactly when the type
    /// is not groupable.
    pub group_key: Option<String>,
    /// Whether the recipient has read past this notification.
    pub seen: bool,
    /// When the recipient dismissed it (and with it its group); `None`
    /// while it is live. Every read here hides the stamped rows.
    pub dismissed_at: Option<DateTime<Utc>>,
    /// When the notification was created, to the database clock.
    pub inserted_at: DateTime<Utc>,
}

/// The columns [`create`] writes; the rest are this module's own doing
/// (the id) or column defaults (`seen`, `dismissed_at`, `inserted_at`).
#[derive(Clone, Debug)]
pub struct NewNotification<'a> {
    /// The recipient account.
    pub user_id: Uuid,
    /// The account that caused the notification.
    pub from_account_id: Uuid,
    /// Mastodon's type name.
    pub notification_type: &'a str,
    /// The status the notification is about, when it names one.
    pub status_id: Option<Uuid>,
    /// The reaction emoji, for a `pleroma:emoji_reaction`.
    pub emoji: Option<&'a str>,
    /// The report, for a `pleroma:report`.
    pub report_id: Option<Uuid>,
}

/// Inserts one notification — its [`grouped_key`] computed from the parts
/// of `new`, `None` for an ungroupable type — and returns the stored row.
///
/// # Errors
///
/// Any database failure (connection, encoding, a foreign key naming no
/// user or status) arrives as [`AppError::Database`](crate::error::AppError).
pub async fn create(pool: &PgPool, new: &NewNotification<'_>) -> AppResult<Notification> {
    let group_key = grouped_key(new.notification_type, new.status_id, new.emoji);
    let row = sqlx::query_as::<_, Notification>(
        "INSERT INTO notifications
             (id, user_id, from_account_id, type, status_id, emoji, report_id, group_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING *",
    )
    .bind(new_id())
    .bind(new.user_id)
    .bind(new.from_account_id)
    .bind(new.notification_type)
    .bind(new.status_id)
    .bind(new.emoji)
    .bind(new.report_id)
    .bind(group_key)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Creates the notification of `new` unless the recipient's
/// [`block_from_strangers`](NotificationSettings::block_from_strangers)
/// setting withholds it.
///
/// A recipient that set the flag is not notified by actors it does not
/// follow — no accepted follow of the actor, no notification — while
/// [`TYPE_FOLLOW_REQUEST`] is exempt and reaches the recipient whatever
/// the setting (Pleroma's `Notification.skip?` `:block_from_strangers`
/// arm, which the ticket keeps narrower: exempting the follow request
/// alone).
///
/// [`create`] does the insert when the notification is not withheld, and
/// the answer distinguishes the two outcomes: `Some` carries the stored
/// row, `None` says the setting skipped it — which is not an error, the
/// causing endpoint moves on. A recipient or actor with no row is left to
/// [`create`]'s foreign key, so this never hides a broken id behind a
/// silent skip.
///
/// # Errors
///
/// Any database failure (connection, encoding, a foreign key naming no
/// user or status) arrives as [`AppError::Database`](crate::error::AppError).
pub async fn maybe_notify(
    pool: &PgPool,
    new: &NewNotification<'_>,
) -> AppResult<Option<Notification>> {
    if withholds_stranger(pool, new).await? {
        return Ok(None);
    }
    create(pool, new).await.map(Some)
}

/// Whether the recipient's setting withholds `new`: `true` only when the
/// flag is set, the type is not the exempt follow request, and the
/// recipient has no accepted follow of the actor. An unknown recipient
/// never withholds (see [`maybe_notify`]).
async fn withholds_stranger(pool: &PgPool, new: &NewNotification<'_>) -> AppResult<bool> {
    if new.notification_type == TYPE_FOLLOW_REQUEST {
        return Ok(false);
    }
    let Some(recipient) = user::get_by_id(pool, new.user_id).await? else {
        return Ok(false);
    };
    if !NotificationSettings::for_user(&recipient).block_from_strangers {
        return Ok(false);
    }
    let followed = follow::has_accepted_follow(pool, recipient.id, new.from_account_id).await?;
    Ok(!followed)
}

/// The stored notification `id` of `user_id`, whatever its state.
///
/// Pleroma's `Notification.get/2`, which only asks who the row belongs
/// to; `None` names no notification the caller owns (or the id text was
/// no id at all), which the endpoints answer 404.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn get(pool: &PgPool, user_id: Uuid, id: Uuid) -> AppResult<Option<Notification>> {
    let row = sqlx::query_as::<_, Notification>(
        "SELECT * FROM notifications WHERE id = $1 AND user_id = $2",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Dismisses the one notification `id` of `user_id` and returns the row.
///
/// The write stamps [`dismissed_at`](Notification::dismissed_at), keeping
/// the first stamp when it already has one; `None` when the caller owns
/// no such notification, foreign and unknown ids alike (Pleroma's
/// `dismiss` "Cannot dismiss notification").
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn dismiss(pool: &PgPool, user_id: Uuid, id: Uuid) -> AppResult<Option<Notification>> {
    let row = sqlx::query_as::<_, Notification>(
        "UPDATE notifications
            SET dismissed_at = COALESCE(dismissed_at, now())
          WHERE id = $1 AND user_id = $2
          RETURNING *",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Dismisses every undismissed notification of `user_id`.
///
/// The dismissal Pleroma's `clear` performs, soft here; an account with
/// nothing to dismiss changes nothing and is not an error. Returns how
/// many rows changed.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn dismiss_all(pool: &PgPool, user_id: Uuid) -> AppResult<u64> {
    let outcome = sqlx::query(
        "UPDATE notifications
            SET dismissed_at = now()
          WHERE user_id = $1 AND dismissed_at IS NULL",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(outcome.rows_affected())
}

/// Dismisses the notifications of `user_id` listed in `ids`.
///
/// The bulk dismissal Pleroma's `destroy_multiple` performs, soft here.
/// Ids the caller does not own, that name nothing, or that are dismissed
/// already simply change nothing; an empty list is the same. Returns how
/// many rows changed.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn dismiss_ids(pool: &PgPool, user_id: Uuid, ids: &[Uuid]) -> AppResult<u64> {
    let outcome = sqlx::query(
        "UPDATE notifications
            SET dismissed_at = now()
          WHERE user_id = $1 AND dismissed_at IS NULL AND id = ANY($2)",
    )
    .bind(user_id)
    .bind(ids.to_vec())
    .execute(pool)
    .await?;
    Ok(outcome.rows_affected())
}

/// Marks every unseen notification of `user_id` up to `max_id` seen.
///
/// Pleroma's `set_read_up_to`, whose `id <= ^id` walk is plain text
/// comparison here (decision 4); returns the rows this call changed,
/// newest first. Rows seen already do not appear again, so a repeat
/// marking answers `[]`.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn mark_read_through_id(
    pool: &PgPool,
    user_id: Uuid,
    max_id: Uuid,
) -> AppResult<Vec<Notification>> {
    let rows = sqlx::query_as::<_, Notification>(
        "WITH marked AS (
             UPDATE notifications
                SET seen = true
              WHERE user_id = $1 AND NOT seen AND id <= $2
             RETURNING *
         )
         SELECT * FROM marked ORDER BY id DESC",
    )
    .bind(user_id)
    .bind(max_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Removes every notification about `status_id`.
///
/// A deleted status leaves its favourites', boosts', reactions' and
/// mentions' notifications nothing to render, and this is what takes them
/// with it (the status row it names is a tombstone, so the schema's
/// `ON DELETE CASCADE` never fires). Returns how many rows went.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn delete_for_status(pool: &PgPool, status_id: Uuid) -> AppResult<u64> {
    let outcome = sqlx::query("DELETE FROM notifications WHERE status_id = $1")
        .bind(status_id)
        .execute(pool)
        .await?;
    Ok(outcome.rows_affected())
}

/// The client filters every filtered read shares: which types to show,
/// which to hide, and which single actor's notifications to keep. All
/// three are open at their absence.
#[derive(Clone, Debug, Default)]
pub struct NotificationFilters<'a> {
    /// `types[]`: keep only these types; empty keeps all.
    pub types: Vec<&'a str>,
    /// `exclude_types[]`: drop these types; empty drops none.
    pub exclude_types: Vec<&'a str>,
    /// `account_id`: keep only notifications caused by this account.
    pub account_id: Option<Uuid>,
}

impl NotificationFilters<'_> {
    /// The `AND (…)` fragments of the three filters (each skipped when it
    /// asks for nothing), continuing the `WHERE` the caller's statement
    /// already opened on the `n` alias.
    fn push(&self, qb: &mut QueryBuilder<Postgres>) {
        if !self.types.is_empty() {
            let _in: &mut _ = qb
                .push(" AND n.type = ANY(")
                .push_bind(self.types.clone())
                .push(")");
        }
        if !self.exclude_types.is_empty() {
            let _out: &mut _ = qb
                .push(" AND n.type <> ALL(")
                .push_bind(self.exclude_types.clone())
                .push(")");
        }
        if let Some(account_id) = self.account_id {
            let _by: &mut _ = qb.push(" AND n.from_account_id = ").push_bind(account_id);
        }
    }
}

/// The scope every notification read shares, as one `WHERE` clause on the
/// `n` alias: the recipient's own, undismissed, from a still-active actor
/// (Pleroma's `for_user_query`).
fn push_scope(qb: &mut QueryBuilder<Postgres>, user_id: Uuid) {
    let _scoped: &mut _ = qb.push(" WHERE n.user_id = ").push_bind(user_id).push(
        " AND n.dismissed_at IS NULL AND EXISTS \
             (SELECT 1 FROM users au WHERE au.id = n.from_account_id AND au.is_active)",
    );
}

/// The `AND` fragments that hide what the recipient refused, in Pleroma's
/// `for_user_query` behind `exclude_blocked`: actors the recipient blocked,
/// and actors mirrored from a domain the recipient blocked, whatever
/// `with_muted` says. Both read the recipient off `n.user_id`, so the
/// fragment binds nothing; a local actor's `NULL` domain never equals a
/// blocked-domain row, so only remote actors can be domain-hidden. Pleroma
/// keeps notifications from a domain-blocked actor it still follows; this
/// listing hides the whole domain, the fact the flag records.
const BLOCKED_CAUSES: &str = "
     AND NOT EXISTS (SELECT 1 FROM blocks bl
                      WHERE bl.blocker_id = n.user_id
                        AND bl.blocked_id = n.from_account_id)
     AND NOT EXISTS (SELECT 1 FROM user_domain_blocks db
                               JOIN users au ON au.id = n.from_account_id
                            WHERE db.user_id = n.user_id
                              AND db.domain = au.domain)";

/// The `AND` fragments that hide what the recipient muted — the exclusions
/// Pleroma's `exclude_notification_muted` skips when its `:with_muted`
/// option is true: actors under a still-live mute that hides notifications
/// (a plain timeline mute never hides anything here), and notifications
/// about a status in a thread the recipient muted.
const MUTED_CAUSES: &str = "
     AND NOT EXISTS (SELECT 1 FROM mutes mu
                      WHERE mu.muter_id = n.user_id
                        AND mu.muted_id = n.from_account_id
                        AND mu.hide_notifications
                        AND (mu.expires_at IS NULL OR mu.expires_at > now()))
     AND NOT EXISTS (SELECT 1 FROM conversation_mutes cm
                               JOIN statuses cs ON cs.id = n.status_id
                            WHERE cm.user_id = n.user_id
                              AND cm.conversation_id = cs.conversation_id)";

/// Everything [`page_for_user`] needs: whose notifications, filtered how,
/// whether the recipient's own mutes hide them, and which page of rows.
#[derive(Clone, Debug)]
pub struct FlatPageParams<'a> {
    /// The recipient account.
    pub user_id: Uuid,
    /// The client's `types[]` / `exclude_types[]` / `account_id` filters.
    pub filters: NotificationFilters<'a>,
    /// Whether notifications whose cause the recipient muted — a muted
    /// actor, a muted thread — stay in the page (Pleroma's `:with_muted`,
    /// the endpoint's `with_muted` parameter).
    ///
    /// Blocked actors and blocked domains stay hidden either way.
    pub with_muted: bool,
    /// The cursor page over notification rows, keyed on the row id.
    pub page: PageParams,
}

/// One page of the flat listing: the recipient's notifications, newest row
/// first, under `push_scope`'s read scope (dismissed rows never appear,
/// and neither do an invisible actor's).
///
/// The page is narrowed by the client's [`NotificationFilters`] — `types`
/// restricts to the named types, `exclude_types` removes them,
/// `account_id` keeps one actor, and a type name no row carries simply
/// matches nothing — and by `MUTED_CAUSES` unless the request asked to
/// keep what its reader muted.
///
/// The cursors walk plain id comparison (decision 4) through
/// [`PageParams::apply_keyset`], so a `min_id` page reads forward and comes
/// back flipped like every other listing.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn page_for_user(
    pool: &PgPool,
    params: &FlatPageParams<'_>,
) -> AppResult<Vec<Notification>> {
    let mut qb = QueryBuilder::<Postgres>::new("SELECT n.* FROM notifications n");
    push_scope(&mut qb, params.user_id);
    params.filters.push(&mut qb);
    let _blocked: &mut _ = qb.push(BLOCKED_CAUSES);
    if !params.with_muted {
        let _muted: &mut _ = qb.push(MUTED_CAUSES);
    }
    params.page.apply_keyset(&mut qb, "n.id");
    let rows = qb.build_query_as::<Notification>().fetch_all(pool).await?;
    Ok(params.page.finish(rows))
}

/// The notification `id` of `user_id` as the listings show it: the owner's,
/// undismissed, from a still-active actor — `push_scope` narrowed to one
/// row.
///
/// `None` says the caller owns no such live row (the unknown, the dismissed
/// and the invisible-actor id all answer the shared 404), where [`get`]
/// reports the row whatever its state.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn get_visible(
    pool: &PgPool,
    user_id: Uuid,
    id: Uuid,
) -> AppResult<Option<Notification>> {
    let mut qb = QueryBuilder::<Postgres>::new("SELECT n.* FROM notifications n");
    push_scope(&mut qb, user_id);
    let _one: &mut _ = qb.push(" AND n.id = ").push_bind(id);
    let row = qb
        .build_query_as::<Notification>()
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

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
/// [`SAMPLE_SIZE`], newest first, still possibly repeating an actor — the
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
    /// The actors of up to [`SAMPLE_SIZE`] newest members, newest first.
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
    /// [`GROUPABLE_TYPES`] when it named none). Anything outside it reads
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

/// Marks every undismissed notification of one group dismissed.
///
/// Returns how many rows changed. Dismissing an unknown group (or one
/// already dismissed) changes nothing and is not an error — Pleroma's
/// `dismiss_group` answers `{}` either way.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn dismiss_group(
    pool: &PgPool,
    user_id: Uuid,
    selector: &GroupSelector,
) -> AppResult<u64> {
    let mut qb = QueryBuilder::<Postgres>::new("UPDATE notifications n SET dismissed_at = now()");
    push_scope(&mut qb, user_id);
    match selector {
        GroupSelector::Notification(id) => {
            let _id: &mut _ = qb.push(" AND n.id = ").push_bind(*id);
        }
        GroupSelector::GroupKey(key) => {
            let _key: &mut _ = qb.push(" AND n.group_key = ").push_bind(key.clone());
        }
    }
    let outcome = qb.build().execute(pool).await?;
    Ok(outcome.rows_affected())
}

/// Everything [`unread_count`] needs: whose, filtered how, where the
/// caller's `notifications` marker stands, and the cap.
#[derive(Clone, Debug)]
pub struct UnreadParams<'a> {
    /// The recipient account.
    pub user_id: Uuid,
    /// The client's `types[]` / `exclude_types[]` / `account_id` filters.
    pub filters: NotificationFilters<'a>,
    /// The id the `notifications` marker holds; `None` (no marker, or one
    /// that names no id) counts from the beginning of time.
    pub marker: Option<Uuid>,
    /// The cap on the answer: the count never exceeds it.
    pub limit: i64,
}

/// How many unseen undismissed notifications of `params.user_id` sit past
/// the marker — the `{"count": N}` of `GET /api/v2/notifications/unread_count`.
///
/// The cap is applied inside the count, so a caller asking for at most
/// `limit` gets exactly that when more would match.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn unread_count(pool: &PgPool, params: &UnreadParams<'_>) -> AppResult<i64> {
    let mut qb =
        QueryBuilder::<Postgres>::new("SELECT count(*) FROM (SELECT 1 FROM notifications n");
    push_scope(&mut qb, params.user_id);
    let _unseen: &mut _ = qb.push(" AND NOT n.seen");
    if let Some(marker) = params.marker {
        let _past: &mut _ = qb.push(" AND n.id > ").push_bind(marker);
    }
    params.filters.push(&mut qb);
    let _capped: &mut _ = qb.push(" LIMIT ").push_bind(params.limit).push(") counted");
    let count = qb.build_query_scalar::<i64>().fetch_one(pool).await?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `UUIDv7` for key assertions.
    fn id() -> Uuid {
        Uuid::now_v7()
    }

    #[test]
    fn grouped_keys_carry_the_type_and_the_status() {
        let status = id();
        assert_eq!(
            grouped_key(TYPE_FAVOURITE, Some(status), None).as_deref(),
            Some(format!("favourite-{status}").as_str()),
            "a favourite groups on its status"
        );
        assert_eq!(
            grouped_key(TYPE_REBLOG, Some(status), None).as_deref(),
            Some(format!("reblog-{status}").as_str()),
            "and so does a reblog"
        );
        assert_eq!(
            grouped_key(TYPE_FAVOURITE, None, None).as_deref(),
            Some("favourite-none"),
            "a status-less groupable row groups under none"
        );
        assert_eq!(
            grouped_key(TYPE_EMOJI_REACTION, Some(status), Some("🎉")).as_deref(),
            Some(format!("pleroma:emoji_reaction-{status}-🎉").as_str()),
            "the reaction key carries the emoji"
        );
        assert_eq!(
            grouped_key(TYPE_EMOJI_REACTION, Some(status), None).as_deref(),
            Some(format!("pleroma:emoji_reaction-{status}").as_str()),
            "and without an emoji it is the plain status key"
        );
    }

    #[test]
    fn ungroupable_types_have_no_stored_key() {
        for name in [
            "mention",
            "follow",
            "follow_request",
            "move",
            "update",
            "poll",
            "status",
        ] {
            assert!(!is_groupable(name), "{name} is not groupable");
            assert_eq!(
                grouped_key(name, Some(id()), None),
                None,
                "{name} stores no key"
            );
        }
        assert!(is_groupable(TYPE_FAVOURITE), "the ticketed types group");
        assert!(is_groupable(TYPE_REBLOG));
        assert!(is_groupable(TYPE_EMOJI_REACTION));
    }

    #[test]
    fn selectors_split_the_ungrouped_prefix() {
        let notification = id();
        assert_eq!(
            select_group(&format!("{UNGROUNDED_PREFIX}{notification}")),
            GroupSelector::Notification(notification),
            "ungrouped-<id> is the row"
        );
        assert_eq!(
            select_group(&format!("favourite-{notification}")),
            GroupSelector::GroupKey(format!("favourite-{notification}")),
            "a grouped key stays a key"
        );
        assert_eq!(
            select_group("ungrouped-not-an-id"),
            GroupSelector::GroupKey("ungrouped-not-an-id".to_owned()),
            "and a broken prefix matches nothing rather than guessing"
        );
    }

    /// The (recipient, actor, status) id trio a fixture notification needs,
    /// three fresh rows of the test slot.
    async fn trio(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
        use crate::{
            config::TEST_BASE_URL,
            models::{
                status::{self, NewStatus, Visibility},
                user::{NewUser, insert, local_ap_id},
            },
        };
        let recipient = insert(
            pool,
            &NewUser::local(
                "notif-recipient",
                &local_ap_id(TEST_BASE_URL, "notif-recipient"),
            ),
        )
        .await
        .expect("recipient fixture");
        let actor = insert(
            pool,
            &NewUser::local("notif-actor", &local_ap_id(TEST_BASE_URL, "notif-actor")),
        )
        .await
        .expect("actor fixture");
        let status = status::insert(
            pool,
            TEST_BASE_URL,
            &NewStatus {
                visibility: Visibility::Public,
                ..NewStatus::local(&actor, "a favourited post")
            },
        )
        .await
        .expect("status fixture");
        (recipient.id, actor.id, status.id)
    }

    #[tokio::test]
    async fn create_stores_the_computed_group_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let row = create(
            pool,
            &NewNotification {
                user_id: recipient,
                from_account_id: actor,
                notification_type: TYPE_FAVOURITE,
                status_id: Some(status_id),
                emoji: None,
                report_id: None,
            },
        )
        .await
        .expect("the notification inserts");
        assert_eq!(
            row.group_key.as_deref(),
            grouped_key(TYPE_FAVOURITE, Some(status_id), None).as_deref(),
            "the stored key is the computed one"
        );
        assert!(
            row.dismissed_at.is_none() && !row.seen,
            "a fresh row is unread and undismissed"
        );
        assert!(row.inserted_at <= Utc::now(), "stamped by the database");

        let follow = create(
            pool,
            &NewNotification {
                user_id: recipient,
                from_account_id: actor,
                notification_type: "follow",
                status_id: None,
                emoji: None,
                report_id: None,
            },
        )
        .await
        .expect("the follow inserts");
        assert_eq!(follow.group_key, None, "a follow stores no group key");
    }

    #[tokio::test]
    async fn dismissal_is_scoped_to_the_owner_and_stamps_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let row = create(
            pool,
            &NewNotification {
                user_id: recipient,
                from_account_id: actor,
                notification_type: TYPE_FAVOURITE,
                status_id: Some(status_id),
                emoji: None,
                report_id: None,
            },
        )
        .await
        .expect("the notification inserts");

        let foreign = Uuid::now_v7();
        assert!(
            dismiss(pool, foreign, row.id)
                .await
                .expect("read")
                .is_none(),
            "a stranger's dismissal finds no row"
        );
        assert!(
            dismiss(pool, recipient, Uuid::now_v7())
                .await
                .expect("read")
                .is_none(),
            "and an unknown id does too"
        );

        let once = dismiss(pool, recipient, row.id)
            .await
            .expect("the dismissal runs")
            .expect("the owner's row comes back");
        assert!(once.dismissed_at.is_some(), "the row is stamped");
        let twice = dismiss(pool, recipient, row.id)
            .await
            .expect("the repeat runs")
            .expect("the row still answers");
        assert_eq!(
            twice.dismissed_at, once.dismissed_at,
            "a repeat dismissal keeps the first stamp"
        );
    }

    #[tokio::test]
    async fn clear_and_the_bulk_dismissal_only_take_the_callers_rows() {
        use crate::{
            config::TEST_BASE_URL,
            models::user::{NewUser, insert, local_ap_id},
        };
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let stranger = insert(
            pool,
            &NewUser::local(
                "notif-stranger",
                &local_ap_id(TEST_BASE_URL, "notif-stranger"),
            ),
        )
        .await
        .expect("stranger fixture");
        let other = stranger.id;
        let mine_one = create(
            pool,
            &NewNotification {
                user_id: recipient,
                from_account_id: actor,
                notification_type: TYPE_FAVOURITE,
                status_id: Some(status_id),
                emoji: None,
                report_id: None,
            },
        )
        .await
        .expect("the first notification inserts");
        let _mine_two = create(
            pool,
            &NewNotification {
                user_id: recipient,
                from_account_id: actor,
                notification_type: TYPE_REBLOG,
                status_id: Some(status_id),
                emoji: None,
                report_id: None,
            },
        )
        .await
        .expect("the second notification inserts");
        let theirs = create(
            pool,
            &NewNotification {
                user_id: other,
                from_account_id: actor,
                notification_type: TYPE_FAVOURITE,
                status_id: Some(status_id),
                emoji: None,
                report_id: None,
            },
        )
        .await
        .expect("the foreign notification inserts");

        let bulk = dismiss_ids(pool, recipient, &[mine_one.id, theirs.id, Uuid::now_v7()])
            .await
            .expect("the bulk dismissal runs");
        assert_eq!(bulk, 1, "only the one listed row of theirs changed");
        let still_live = get(pool, other, theirs.id)
            .await
            .expect("the read runs")
            .expect("the foreign row is still there");
        assert!(still_live.dismissed_at.is_none(), "and stayed undismissed");

        let cleared = dismiss_all(pool, recipient).await.expect("clear runs");
        assert_eq!(cleared, 1, "the second row goes, the first was gone");
        assert_eq!(
            dismiss_all(pool, recipient).await.expect("repeat clear"),
            0,
            "nothing undismissed is left, and that is not an error"
        );
        let untouched = get(pool, other, theirs.id)
            .await
            .expect("the read runs")
            .expect("the foreign row survives the clear too");
        assert!(untouched.dismissed_at.is_none(), "clear is scoped");
    }

    #[tokio::test]
    async fn marking_read_walks_back_from_the_cursor() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let first = create(
            pool,
            &NewNotification {
                user_id: recipient,
                from_account_id: actor,
                notification_type: TYPE_FAVOURITE,
                status_id: Some(status_id),
                emoji: None,
                report_id: None,
            },
        )
        .await
        .expect("the first notification inserts");
        let second = create(
            pool,
            &NewNotification {
                user_id: recipient,
                from_account_id: actor,
                notification_type: TYPE_REBLOG,
                status_id: Some(status_id),
                emoji: None,
                report_id: None,
            },
        )
        .await
        .expect("the second notification inserts");

        let marked = mark_read_through_id(pool, recipient, first.id)
            .await
            .expect("the marking runs");
        assert_eq!(
            marked.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![first.id],
            "the cursor itself and everything older, newest first"
        );
        assert!(marked.first().expect("one row").seen, "the row reads seen");
        let still_unseen = get(pool, recipient, second.id)
            .await
            .expect("the read runs")
            .expect("the newer row is there");
        assert!(!still_unseen.seen, "and the newer row is untouched");

        let second_pass = mark_read_through_id(pool, recipient, second.id)
            .await
            .expect("the repeat runs");
        assert_eq!(
            second_pass.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![second.id],
            "rows seen already are not marked twice"
        );
    }

    #[tokio::test]
    async fn deleting_a_status_takes_its_notifications() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let about = create(
            pool,
            &NewNotification {
                user_id: recipient,
                from_account_id: actor,
                notification_type: TYPE_FAVOURITE,
                status_id: Some(status_id),
                emoji: None,
                report_id: None,
            },
        )
        .await
        .expect("the notification inserts");
        let unrelated = create(
            pool,
            &NewNotification {
                user_id: recipient,
                from_account_id: actor,
                notification_type: "follow",
                status_id: None,
                emoji: None,
                report_id: None,
            },
        )
        .await
        .expect("the follow inserts");

        let gone = delete_for_status(pool, status_id)
            .await
            .expect("the cleanup runs");
        assert_eq!(gone, 1, "the one row about the status went");
        assert!(
            get(pool, recipient, about.id)
                .await
                .expect("the read runs")
                .is_none(),
            "and it reads as nothing"
        );
        assert!(
            get(pool, recipient, unrelated.id)
                .await
                .expect("the read runs")
                .is_some(),
            "a status-less notification survives"
        );
        assert_eq!(
            delete_for_status(pool, status_id)
                .await
                .expect("the repeat runs"),
            0,
            "cleaning twice changes nothing and is not an error"
        );
    }

    /// The `maybe_notify` fixtures' account pair: a recipient named
    /// `<tag>-recipient` and an actor named `<tag>-actor`.
    async fn pair(pool: &PgPool, tag: &str) -> (user::User, user::User) {
        use crate::{
            config::TEST_BASE_URL,
            models::user::{NewUser, insert, local_ap_id},
        };
        let recipient = insert(
            pool,
            &NewUser::local(
                &format!("{tag}-recipient"),
                &local_ap_id(TEST_BASE_URL, &format!("{tag}-recipient")),
            ),
        )
        .await
        .expect("recipient fixture");
        let actor = insert(
            pool,
            &NewUser::local(
                &format!("{tag}-actor"),
                &local_ap_id(TEST_BASE_URL, &format!("{tag}-actor")),
            ),
        )
        .await
        .expect("actor fixture");
        (recipient, actor)
    }

    /// A status-less `notification_type` notification of `recipient`
    /// caused by `actor`, for the `maybe_notify` fixtures.
    fn about<'a>(
        recipient: &'a user::User,
        actor: &'a user::User,
        notification_type: &'a str,
    ) -> NewNotification<'a> {
        NewNotification {
            user_id: recipient.id,
            from_account_id: actor.id,
            notification_type,
            status_id: None,
            emoji: None,
            report_id: None,
        }
    }

    /// Turns on the recipient's `block_from_strangers` flag.
    async fn block_strangers(pool: &PgPool, recipient: &user::User) {
        let settings = NotificationSettings {
            block_from_strangers: true,
            hide_notification_contents: false,
        };
        user::notification_settings::save(pool, recipient.id, &settings)
            .await
            .expect("the flag is stored");
    }

    /// How many notification rows the slot holds.
    async fn row_count(pool: &PgPool) -> i64 {
        let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM notifications")
            .fetch_one(pool)
            .await
            .expect("the count runs");
        count
    }

    #[tokio::test]
    async fn maybe_notify_creates_when_the_recipient_does_not_block() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor) = pair(pool, "notify-open").await;
        let row = maybe_notify(pool, &about(&recipient, &actor, "mention"))
            .await
            .expect("the notification inserts")
            .expect("the default setting notifies");
        assert_eq!(row.user_id, recipient.id, "the recipient owns the row");
        assert_eq!(row.from_account_id, actor.id, "the actor is the cause");
    }

    #[tokio::test]
    async fn blocking_strangers_withholds_the_notification_and_inserts_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor) = pair(pool, "notify-blocked").await;
        block_strangers(pool, &recipient).await;

        let withheld = maybe_notify(pool, &about(&recipient, &actor, "favourite"))
            .await
            .expect("the read of the setting runs");
        assert!(withheld.is_none(), "a stranger causes no notification");
        assert_eq!(row_count(pool).await, 0, "and nothing was inserted");
    }

    #[tokio::test]
    async fn blocking_strangers_keeps_followed_actors() {
        use crate::config::TEST_BASE_URL;
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor) = pair(pool, "notify-followed").await;
        block_strangers(pool, &recipient).await;
        let relation = follow::follow(pool, TEST_BASE_URL, &recipient, &actor)
            .await
            .expect("the accepted follow inserts");
        assert_eq!(
            relation.state,
            follow::FollowState::Accepted,
            "the fixture follow is accepted, not pending"
        );

        let row = maybe_notify(pool, &about(&recipient, &actor, "mention"))
            .await
            .expect("the read runs")
            .expect("an actor the recipient follows notifies");
        assert_eq!(
            row.from_account_id, actor.id,
            "the followed actor caused it"
        );
    }

    #[tokio::test]
    async fn a_pending_follow_does_not_unblock_the_stranger() {
        use crate::{
            config::TEST_BASE_URL,
            models::user::{Flag, NewUser, insert, local_ap_id},
        };
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, _) = pair(pool, "notify-gated").await;
        // A locked actor makes the recipient's follow a pending request,
        // not an accepted relationship.
        let actor = insert(
            pool,
            &NewUser {
                locked: Flag::new(true),
                ..NewUser::local(
                    "notify-pending-actor",
                    &local_ap_id(TEST_BASE_URL, "notify-pending-actor"),
                )
            },
        )
        .await
        .expect("locked actor fixture");
        block_strangers(pool, &recipient).await;
        let created = follow::follow(pool, TEST_BASE_URL, &recipient, &actor)
            .await
            .expect("the requested follow inserts");
        assert_eq!(
            created.state,
            follow::FollowState::Pending,
            "the locked actor keeps the request pending"
        );

        assert!(
            maybe_notify(pool, &about(&recipient, &actor, "reblog"))
                .await
                .expect("the read runs")
                .is_none(),
            "a pending follow is not following: the actor stays a stranger"
        );
    }

    #[tokio::test]
    async fn follow_requests_are_exempt_from_the_block() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor) = pair(pool, "notify-request").await;
        block_strangers(pool, &recipient).await;

        let row = maybe_notify(pool, &about(&recipient, &actor, TYPE_FOLLOW_REQUEST))
            .await
            .expect("the notification inserts")
            .expect("the exempt type reaches the recipient");
        assert_eq!(
            row.event_type, TYPE_FOLLOW_REQUEST,
            "the stored row is the follow request"
        );
    }

    #[tokio::test]
    async fn an_unknown_recipient_reaches_the_foreign_key_not_a_skip() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (_, actor) = pair(pool, "notify-nohost").await;
        let orphan = NewNotification {
            user_id: Uuid::now_v7(),
            from_account_id: actor.id,
            notification_type: "mention",
            status_id: None,
            emoji: None,
            report_id: None,
        };
        let err = maybe_notify(pool, &orphan)
            .await
            .expect_err("a recipient with no row is a database failure");
        assert!(
            matches!(err, crate::error::AppError::Database(_)),
            "the broken id surfaces as the error it is: {err}"
        );
    }

    /// Inserts a local user named `nickname`.
    async fn solo(pool: &PgPool, nickname: &str) -> user::User {
        use crate::config::TEST_BASE_URL;
        user::insert(
            pool,
            &user::NewUser::local(nickname, &user::local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("local user fixture")
    }

    /// Inserts the mirror of the remote actor `nickname` on `host`.
    async fn remote(pool: &PgPool, nickname: &str, host: &str) -> user::User {
        user::insert(
            pool,
            &user::NewUser {
                nickname: format!("{nickname}@{host}"),
                ap_id: format!("https://{host}/users/{nickname}"),
                domain: Some(host.to_owned()),
                name: nickname.to_owned(),
                ..user::NewUser::default()
            },
        )
        .await
        .expect("remote user fixture")
    }

    /// A public status caused by `author`, returning its id.
    async fn post(pool: &PgPool, author: &user::User) -> Uuid {
        use crate::{
            config::TEST_BASE_URL,
            models::status::{self, NewStatus, Visibility},
        };
        status::insert(
            pool,
            TEST_BASE_URL,
            &NewStatus {
                visibility: Visibility::Public,
                ..NewStatus::local(author, "a thread")
            },
        )
        .await
        .expect("status fixture")
        .id
    }

    /// A `notification_type` notification of `recipient` by `actor` about
    /// `status_id`.
    async fn note(
        pool: &PgPool,
        recipient: Uuid,
        actor: Uuid,
        notification_type: &str,
        status_id: Option<Uuid>,
    ) -> Notification {
        create(
            pool,
            &NewNotification {
                user_id: recipient,
                from_account_id: actor,
                notification_type,
                status_id,
                emoji: None,
                report_id: None,
            },
        )
        .await
        .expect("the notification fixture inserts")
    }

    /// The flat page's parameters with everything open but `page`.
    fn flat(user_id: Uuid, page: PageParams) -> FlatPageParams<'static> {
        FlatPageParams {
            user_id,
            filters: NotificationFilters::default(),
            with_muted: false,
            page,
        }
    }

    /// The default page from `query`, as the endpoint parses it.
    fn page(query: &str) -> PageParams {
        use crate::pagination::MAX_LIMIT;
        PageParams::from_query(Some(query), MAX_LIMIT).expect("fixture cursors parse")
    }

    /// [`page_for_user`]'s ids, newest first.
    async fn flat_ids(pool: &PgPool, params: &FlatPageParams<'_>) -> Vec<Uuid> {
        page_for_user(pool, params)
            .await
            .expect("the page reads")
            .into_iter()
            .map(|row| row.id)
            .collect()
    }

    #[tokio::test]
    async fn the_flat_page_orders_rows_and_walks_the_cursors() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let older = note(pool, recipient, actor, "favourite", Some(status_id)).await;
        let middle = note(pool, recipient, actor, "mention", Some(status_id)).await;
        let newer = note(pool, recipient, actor, "follow", None).await;
        let all = vec![newer.id, middle.id, older.id];

        let open = flat_ids(pool, &flat(recipient, page(""))).await;
        assert_eq!(open, all, "newest row first");
        let capped = flat_ids(pool, &flat(recipient, page("limit=2"))).await;
        assert_eq!(capped, vec![newer.id, middle.id], "limit cuts the page");
        let back = flat_ids(
            pool,
            &flat(recipient, page(&format!("max_id={}", middle.id))),
        )
        .await;
        assert_eq!(back, vec![older.id], "max_id keeps strictly older rows");
        let up = flat_ids(
            pool,
            &flat(recipient, page(&format!("since_id={}", middle.id))),
        )
        .await;
        assert_eq!(up, vec![newer.id], "since_id keeps strictly newer rows");
        let forward = flat_ids(
            pool,
            &flat(recipient, page(&format!("min_id={}", older.id))),
        )
        .await;
        assert_eq!(
            forward,
            vec![newer.id, middle.id],
            "a min_id page still answers newest-first"
        );
    }

    #[tokio::test]
    async fn the_flat_filters_restrict_remove_and_keep_one_actor() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let carol = solo(pool, "flat-carol").await;
        let outsider = solo(pool, "flat-outsider").await;
        let favourite = note(pool, recipient, actor, "favourite", Some(status_id)).await;
        let mention = note(pool, recipient, actor, "mention", Some(status_id)).await;
        let follow = note(pool, recipient, carol.id, "follow", None).await;
        let _foreign = note(pool, outsider.id, carol.id, "follow", None).await;

        let mut params = flat(recipient, page(""));
        let kept = NotificationFilters {
            types: vec!["favourite", "mention"],
            ..NotificationFilters::default()
        };
        params.filters = kept;
        assert_eq!(
            flat_ids(pool, &params).await,
            vec![mention.id, favourite.id],
            "types[] restricts to the named types, newest first"
        );
        params.filters = NotificationFilters {
            exclude_types: vec!["favourite"],
            ..NotificationFilters::default()
        };
        assert_eq!(
            flat_ids(pool, &params).await,
            vec![follow.id, mention.id],
            "exclude_types[] removes the named type"
        );
        params.filters = NotificationFilters {
            account_id: Some(carol.id),
            ..NotificationFilters::default()
        };
        assert_eq!(
            flat_ids(pool, &params).await,
            vec![follow.id],
            "account_id keeps one actor's rows"
        );
        params.filters = NotificationFilters {
            types: vec!["not-a-type"],
            ..NotificationFilters::default()
        };
        assert!(
            flat_ids(pool, &params).await.is_empty(),
            "a type string no row carries matches nothing"
        );
    }

    #[tokio::test]
    async fn the_flat_page_hides_dismissed_and_deactivated_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let ghost = note(pool, recipient, actor, "favourite", Some(status_id)).await;
        let live = note(pool, recipient, actor, "mention", Some(status_id)).await;

        let _dismissed = dismiss(pool, recipient, ghost.id)
            .await
            .expect("the dismissal runs");
        assert_eq!(
            flat_ids(pool, &flat(recipient, page(""))).await,
            vec![live.id],
            "dismissed rows never appear"
        );

        let _inactive = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(actor)
            .execute(pool)
            .await
            .expect("the deactivation lands");
        assert!(
            flat_ids(pool, &flat(recipient, page(""))).await.is_empty(),
            "a deactivated actor's notifications hide too"
        );
    }

    #[tokio::test]
    async fn with_muted_decides_the_mute_exclusions_but_never_the_blocks() {
        use crate::{
            config::TEST_BASE_URL,
            models::{
                block, conversation_mute, domain_block,
                mute::{self, MuteOptions},
            },
        };
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = solo(pool, "flat-alice").await;
        let muted = solo(pool, "flat-muted").await;
        let blocked = solo(pool, "flat-blocked").await;
        let timeline_only = solo(pool, "flat-timeline").await;
        let mallory = remote(pool, "mallory", "evil.example").await;
        let erin = solo(pool, "flat-erin").await;
        let thread = post(pool, &erin).await;

        let from_muted = note(pool, alice.id, muted.id, "follow", None).await;
        let from_blocked = note(pool, alice.id, blocked.id, "follow", None).await;
        let from_timeline = note(pool, alice.id, timeline_only.id, "mention", None).await;
        let from_domain = note(pool, alice.id, mallory.id, "follow", None).await;
        let in_muted_thread = note(pool, alice.id, erin.id, "favourite", Some(thread)).await;
        let clean = note(pool, alice.id, erin.id, "follow", None).await;

        let _muted = mute::mute(pool, &alice, &muted, MuteOptions::default())
            .await
            .expect("the mute fixture lands");
        let _timeline = mute::mute(
            pool,
            &alice,
            &timeline_only,
            MuteOptions {
                notifications: false,
                expires_in: None,
            },
        )
        .await
        .expect("the timeline-only mute lands");
        let _blocked = block::block(pool, TEST_BASE_URL, &alice, &blocked)
            .await
            .expect("the block fixture lands");
        domain_block::block_domain(pool, alice.id, "evil.example")
            .await
            .expect("the domain block lands");
        conversation_mute::mute(pool, alice.id, thread)
            .await
            .expect("the thread mute lands");

        let hidden = vec![clean.id, from_timeline.id];
        assert_eq!(
            flat_ids(pool, &flat(alice.id, page(""))).await,
            hidden,
            "by default the muted actor and muted thread hide; a \
             timeline-only mute never does, and so do the blocked actor \
             and the blocked domain"
        );

        let mut shown = flat(alice.id, page(""));
        shown.with_muted = true;
        let kept = flat_ids(pool, &shown).await;
        assert!(kept.contains(&from_muted.id), "the muted actor reads back");
        assert!(kept.contains(&in_muted_thread.id), "and the muted thread");
        assert!(
            !kept.contains(&from_blocked.id),
            "the blocked actor stays hidden, Pleroma's unconditional \
             `exclude_blocked`: {kept:?}"
        );
        assert!(
            !kept.contains(&from_domain.id),
            "and so does the blocked domain"
        );
    }

    #[tokio::test]
    async fn get_visible_answers_only_live_owned_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let live = note(pool, recipient, actor, "favourite", Some(status_id)).await;
        let doomed = note(pool, recipient, actor, "mention", Some(status_id)).await;

        assert_eq!(
            get_visible(pool, recipient, live.id)
                .await
                .expect("the read runs")
                .map(|row| row.id),
            Some(live.id),
            "the owner's live row reads back"
        );
        assert!(
            get_visible(pool, Uuid::now_v7(), live.id)
                .await
                .expect("the read runs")
                .is_none(),
            "a stranger's read finds nothing"
        );
        assert!(
            get_visible(pool, recipient, Uuid::now_v7())
                .await
                .expect("the read runs")
                .is_none(),
            "and so does an unknown id"
        );
        let _dismissed = dismiss(pool, recipient, doomed.id)
            .await
            .expect("the dismissal runs");
        assert!(
            get_visible(pool, recipient, doomed.id)
                .await
                .expect("the read runs")
                .is_none(),
            "a dismissed row hides"
        );
    }
}
