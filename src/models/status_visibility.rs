//! The one source of truth for who may see a status.
//!
//! Every timeline, list and lookup reads statuses through this module, so
//! the visibility rules are stated exactly once and in two shapes that
//! agree by construction:
//!
//! - [`can_view`] answers the question for one already-loaded [`Status`],
//!   and
//! - [`push_visibility_clause`] appends the same answer to a
//!   [`QueryBuilder`] statement as a single `AND ( ... )` group of
//!   `EXISTS` subqueries over `follows`, `status_mentions` and `blocks`,
//!   so a page of statuses is narrowed in the database instead of row by
//!   row; [`filter_visible`] runs that clause over an already-loaded batch
//!   in one statement.
//!
//! The rules, mirroring Pleroma's `visible_for_user?`
//! (`lib/pleroma/web/activity_pub/visibility.ex`) and the timeline filters
//! of `lib/pleroma/activity/queries.ex`, restated over decision 6's
//! relational statuses:
//!
//! 1. a deleted status (a tombstone) is visible to nobody, not even its
//!    author;
//! 2. an author who blocked the viewer sees nothing of theirs, and sees
//!    none of it, shown to the viewer — except their own posts;
//! 3. `public` and `unlisted` are visible to anyone, anonymous included;
//! 4. `local` (Pleroma's never-federated visibility) is visible only to an
//!    authenticated viewer this instance owns — remote users never log in,
//!    so an anonymous request and a remote mirror both lose;
//! 5. `private` is visible to the author, to accounts with an
//!    **accepted** follow of the author (a pending request is not one), and
//!    to the accounts it @-mentions (`status_mentions`), as in Mastodon's
//!    `status_policy.rb:10` and Pleroma's `visibility.ex:99-107`;
//! 6. `direct` is visible to the author and to the accounts the status
//!    @-mentions (`status_mentions`);
//! 7. a reblog row is visible only if its own visibility allows it **and**
//!    the original it reblogs is itself visible to the viewer — rules 1
//!    and 2 apply to the original too, so a tombstoned or
//!    author-blocked original hides every reblog of it.
//!
//! In the SQL shape, rule 7 recurses one level (the reblogged original is
//! checked with the same clause, minus its own original check): a reblog
//! of a reblog is folded to a reblog of the original when clients post it
//! (Mastodon's `reblog` resolves through), so a chain longer than this one
//! link does not occur in stored data. [`can_view`] walks the chain as far
//! as it goes, which keeps the two shapes equivalent for every row this
//! repository writes.
//!
//! The `alias` argument names the table or alias `statuses` is reached
//! under in the caller's statement (`s`, `statuses`, a join's alias): it
//! is pasted into the SQL verbatim, like [`PageParams::apply_keyset`]'s
//! column name, so callers pass one of their own literals, never request
//! data. Inside a reblog's `EXISTS` subquery the original is always
//! aliased `rb`.
//!
//! [`PageParams::apply_keyset`]: crate::pagination::PageParams::apply_keyset

use std::collections::HashSet;

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        block, follow, mention,
        status::{self, Status, Visibility},
        user::User,
    },
};

/// The active-author predicate every status read appends, over statuses
/// aliased `s`: the author's account is still active.
///
/// It is Pleroma's `Activity.restrict_deactivated_users`
/// (`lib/pleroma/activity.ex`): a suspended or deactivated account's posts
/// leave the timelines and the status search while the flag stands, and
/// come back with it. Starts with ` AND `, so it is pushed onto a statement
/// that already has a `WHERE`.
pub const ACTIVE_AUTHOR: &str =
    " AND EXISTS (SELECT 1 FROM users au WHERE au.id = s.user_id AND au.is_active)";

/// The prefix of [`filter_visible`]'s statement: the candidate ids come as
/// one array bind, then [`push_visibility_clause`] closes the parenthesis
/// and appends the clause under alias `s`.
const FILTER_IDS_SQL: &str = "SELECT s.id FROM statuses s WHERE s.id = ANY(";

/// Whether `viewer` may see `status`, per the [module docs](self).
///
/// `viewer` is `None` for an anonymous request. The status is judged from
/// the row in hand plus what the database knows about the viewer: the
/// author's block of them, the accepted follow, the mention link, and —
/// for a reblog — the original row, read with
/// [`status::get_by_id_including_deleted`] so its tombstone is visible to
/// this check (and rule 1 then applies to it). Rule 7 is then applied to
/// that original in turn: the chain is walked in a loop, each link
/// admitted by the same three tests, until a link fails, a link's
/// original row is gone, or the chain runs out.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn can_view(pool: &PgPool, status: &Status, viewer: Option<&User>) -> AppResult<bool> {
    let mut current = status.clone();
    loop {
        if !one_status_admits(pool, &current, viewer).await? {
            return Ok(false);
        }
        let Some(original_id) = current.reblog_of_id else {
            return Ok(true);
        };
        let Some(original) = status::get_by_id_including_deleted(pool, original_id).await? else {
            return Ok(false);
        };
        current = original;
    }
}

/// Rules 1 to 6 for the one status in hand: not a tombstone, the author
/// has not blocked the viewer, and the status's own visibility admits
/// them. Rule 7's chain is [`can_view`]'s loop around this.
async fn one_status_admits(
    pool: &PgPool,
    status: &Status,
    viewer: Option<&User>,
) -> AppResult<bool> {
    if status.deleted_at.is_some() {
        return Ok(false);
    }
    if author_blocked_viewer(pool, status, viewer).await? {
        return Ok(false);
    }
    visibility_allows(pool, status, viewer).await
}

/// Appends the [`can_view`] rules, in SQL, to the statement `qb` already
/// holds: one `AND ( ... )` group over `alias`, binding the viewer's id
/// where the `EXISTS` subqueries need it.
///
/// The group answers the rules of the [module docs](self) for every row of
/// the aliased `statuses` table: `deleted_at IS NULL` (rule 1), a `NOT
/// EXISTS` over `blocks` (rule 2), the visibility `OR` arms (rules 3 to 6,
/// the `local` arm only for a viewer this instance owns) and, for a
/// reblog, an `EXISTS` over the original row under the same group minus
/// its own original check (rule 7). Anonymous requests — `viewer` is
/// `None` — get just `visibility IN ('public','unlisted')` and the
/// `deleted_at` and reblog conditions, the only rules an anonymous viewer
/// can ever pass.
///
/// See the [module docs](self) for what `alias` may be.
pub fn push_visibility_clause(qb: &mut QueryBuilder<Postgres>, viewer: Option<&User>, alias: &str) {
    let _and: &mut _ = qb.push(" AND ");
    push_visible_expression(qb, viewer, alias, true);
}

/// Keeps exactly the statuses [`can_view`] admits for `viewer`.
///
/// One batched statement instead of a query per status: the candidate ids
/// go into `WHERE s.id = ANY( ... )`, [`push_visibility_clause`] narrows
/// them the same way every list endpoint will, and the surviving ids
/// decide which of the given rows come back, in the input order.
///
/// Rows a clause never lets stand — tombstones, and a reblog whose
/// original fell out of view — are filtered out like everything else. An
/// empty `statuses` answers an empty `Vec` without touching the database.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn filter_visible(
    pool: &PgPool,
    statuses: Vec<Status>,
    viewer: Option<&User>,
) -> AppResult<Vec<Status>> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = statuses.iter().map(|status| status.id).collect();
    let mut builder = QueryBuilder::<Postgres>::new(FILTER_IDS_SQL);
    let _candidates: &mut _ = builder.push_bind(ids).push(")");
    push_visibility_clause(&mut builder, viewer, "s");
    let rows = builder.build_query_as::<(Uuid,)>().fetch_all(pool).await?;
    let visible: HashSet<Uuid> = rows.into_iter().map(|(id,)| id).collect();
    Ok(statuses
        .into_iter()
        .filter(|status| visible.contains(&status.id))
        .collect())
}

/// Rule 2: whether `status`'s author blocked `viewer`. An anonymous
/// request cannot be blocked by anybody.
async fn author_blocked_viewer(
    pool: &PgPool,
    status: &Status,
    viewer: Option<&User>,
) -> AppResult<bool> {
    let Some(viewer) = viewer else {
        return Ok(false);
    };
    block::is_blocking(pool, status.user_id, viewer.id).await
}

/// Rules 3 to 6: whether this status's own visibility admits `viewer`
/// (rule 7's original check is [`can_view`]'s step after this one).
async fn visibility_allows(
    pool: &PgPool,
    status: &Status,
    viewer: Option<&User>,
) -> AppResult<bool> {
    Ok(match status.visibility {
        Visibility::Public | Visibility::Unlisted => true,
        Visibility::Local => viewer.is_some_and(|viewer| viewer.local.get()),
        Visibility::Private => author_follower_or_mentioned(pool, status, viewer).await?,
        Visibility::Direct => author_or_mentioned(pool, status, viewer).await?,
    })
}

/// Rule 5: `viewer` is the author, has an accepted follow of them, or is
/// among the mentioned accounts — Mastodon's `status_policy.rb:10`
/// (`owned? || following_author? || mention_exists?`) and Pleroma's
/// `visibility.ex:99-107` (the viewer is in `to`/`cc`).
async fn author_follower_or_mentioned(
    pool: &PgPool,
    status: &Status,
    viewer: Option<&User>,
) -> AppResult<bool> {
    let Some(viewer) = viewer else {
        return Ok(false);
    };
    if viewer.id == status.user_id {
        return Ok(true);
    }
    if follow::has_accepted_follow(pool, viewer.id, status.user_id).await? {
        return Ok(true);
    }
    mention::is_mentioned(pool, viewer.id, status.id).await
}

/// Rule 6: `viewer` is the author, or is among the mentioned accounts.
async fn author_or_mentioned(
    pool: &PgPool,
    status: &Status,
    viewer: Option<&User>,
) -> AppResult<bool> {
    let Some(viewer) = viewer else {
        return Ok(false);
    };
    if viewer.id == status.user_id {
        return Ok(true);
    }
    mention::is_mentioned(pool, viewer.id, status.id).await
}

/// Pushes the whole parenthesised visibility expression for `alias` that
/// [`push_visibility_clause`] wraps its ` AND ` around: the tombstone
/// condition (rule 1), the author's block of the viewer (rule 2, bound
/// viewers only), the visibility arms ([`push_visibility_arms`]) and —
/// when `check_original` — rule 7 as an `EXISTS` over the original row
/// under alias `rb`, checked with the same expression one level down.
fn push_visible_expression(
    qb: &mut QueryBuilder<Postgres>,
    viewer: Option<&User>,
    alias: &str,
    check_original: bool,
) {
    let _tombstone: &mut _ = qb.push("( ").push(alias).push(".deleted_at IS NULL");
    if let Some(viewer) = viewer {
        let _unblocked: &mut _ = qb
            .push(
                " AND NOT EXISTS (SELECT 1 FROM blocks \
                    WHERE blocker_id = ",
            )
            .push(alias)
            .push(".user_id AND blocked_id = ")
            .push_bind(viewer.id)
            .push(")");
    }
    let _arms: &mut _ = qb.push(" AND ");
    push_visibility_arms(qb, viewer, alias);
    if check_original {
        let _reblog: &mut _ = qb
            .push(" AND ( ")
            .push(alias)
            .push(
                ".reblog_of_id IS NULL \
                   OR EXISTS (SELECT 1 FROM statuses rb WHERE rb.id = ",
            )
            .push(alias)
            .push(".reblog_of_id AND ");
        push_visible_expression(qb, viewer, "rb", false);
        let _close_reblog: &mut _ = qb.push("))");
    }
    let _close: &mut _ = qb.push(" )");
}

/// Pushes the visibility `OR` arms (rules 3 to 6) for `alias` and
/// `viewer` as one parenthesised group. Anonymous viewers only ever pass
/// the `public`/`unlisted` arm; a local authenticated viewer also passes
/// the `local` arm; `private` and `direct` add their author and `EXISTS`
/// conditions with the viewer's id bound where needed. The `private` arm
/// admits the author, an accepted follower, or a mentioned account (Mastodon
/// `status_policy.rb:10`, Pleroma `visibility.ex:99-107`); `direct` admits
/// the author or a mentioned account.
fn push_visibility_arms(qb: &mut QueryBuilder<Postgres>, viewer: Option<&User>, alias: &str) {
    let Some(viewer) = viewer else {
        let _anonymous: &mut _ = qb
            .push("( ")
            .push(alias)
            .push(".visibility IN ('public','unlisted') )");
        return;
    };
    let _public: &mut _ = qb
        .push("( ")
        .push(alias)
        .push(".visibility IN ('public','unlisted')");
    if viewer.local.get() {
        let _local: &mut _ = qb.push(" OR ").push(alias).push(".visibility = 'local'");
    }
    let _private: &mut _ = qb
        .push(" OR ( ")
        .push(alias)
        .push(".visibility = 'private' AND ( ")
        .push(alias)
        .push(".user_id = ")
        .push_bind(viewer.id)
        .push(
            " OR EXISTS (SELECT 1 FROM follows \
                WHERE follower_id = ",
        )
        .push_bind(viewer.id)
        .push(" AND followee_id = ")
        .push(alias)
        .push(".user_id AND state = 'accepted') OR EXISTS (SELECT 1 FROM status_mentions WHERE user_id = ")
        .push_bind(viewer.id)
        .push(" AND status_id = ")
        .push(alias)
        .push(".id) ) )");
    let _direct: &mut _ = qb
        .push(" OR ( ")
        .push(alias)
        .push(".visibility = 'direct' AND ( ")
        .push(alias)
        .push(".user_id = ")
        .push_bind(viewer.id)
        .push(
            " OR EXISTS (SELECT 1 FROM status_mentions \
                WHERE user_id = ",
        )
        .push_bind(viewer.id)
        .push(" AND status_id = ")
        .push(alias)
        .push(".id) ) )");
    let _close: &mut _ = qb.push(" )");
}

#[cfg(test)]
mod tests;
