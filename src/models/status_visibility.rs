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
        status::{self, Status, Visibility},
        user::User,
    },
};

/// Whether the status's author blocked the viewer: the two ids, in the
/// block's direction, into [`AUTHOR_BLOCKED_SQL`].
const AUTHOR_BLOCKED_SQL: &str = "SELECT EXISTS (SELECT 1 FROM blocks \
     WHERE blocker_id = $1 AND blocked_id = $2)";

/// Whether the first id has an **accepted** follow of the second: exactly
/// the follower relation rule 5 grants, and a pending or rejected row is
/// not it.
const ACCEPTED_FOLLOWER_SQL: &str = "SELECT EXISTS (SELECT 1 FROM follows \
     WHERE follower_id = $1 AND followee_id = $2 AND state = 'accepted')";

/// Whether the first id is among the second's @-mentions: the `direct`
/// audience of rule 6 and the mentionee arm of rule 5, as
/// [`crate::models::mention`] stores it.
const VIEWER_MENTIONED_SQL: &str = "SELECT EXISTS (SELECT 1 FROM status_mentions \
     WHERE user_id = $1 AND status_id = $2)";

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
    exists_between(pool, AUTHOR_BLOCKED_SQL, status.user_id, viewer.id).await
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
    if exists_between(pool, ACCEPTED_FOLLOWER_SQL, viewer.id, status.user_id).await? {
        return Ok(true);
    }
    exists_between(pool, VIEWER_MENTIONED_SQL, viewer.id, status.id).await
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
    exists_between(pool, VIEWER_MENTIONED_SQL, viewer.id, status.id).await
}

/// Runs one two-argument `SELECT EXISTS` statement — the three
/// `*_SQL` constants above — and answers whether it found the edge.
async fn exists_between(
    pool: &PgPool,
    sql: &'static str,
    first: Uuid,
    second: Uuid,
) -> AppResult<bool> {
    let row: (bool,) = sqlx::query_as(sql)
        .bind(first)
        .bind(second)
        .fetch_one(pool)
        .await?;
    Ok(row.0)
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
mod tests {
    use sqlx::{PgPool, Postgres, QueryBuilder};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            block, follow,
            status::{NewStatus, Visibility, insert as insert_status, soft_delete},
            user::{Flag, NewUser, insert as insert_user, local_ap_id},
        },
    };

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = TEST_BASE_URL;

    /// Inserts a local user named `nickname`.
    async fn local(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// Inserts a local user named `nickname` whose account is locked, so a
    /// follow of it waits for an answer and never grants one.
    async fn locked_local(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser {
                locked: Flag::new(true),
                ..NewUser::local(nickname, &local_ap_id(BASE, nickname))
            },
        )
        .await
        .expect("the locked fixture inserts")
    }

    /// Inserts a mirror of the remote actor `nickname` on `remote.example`.
    async fn remote(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser {
                nickname: format!("{nickname}@remote.example"),
                ap_id: format!("https://remote.example/users/{nickname}"),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// Inserts `text` as `author`'s status of `visibility`.
    async fn post(pool: &PgPool, author: &User, visibility: Visibility, text: &str) -> Status {
        insert_status(
            pool,
            BASE,
            &NewStatus {
                visibility,
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("a plain local post inserts")
    }

    /// Inserts a `direct` status of `author` mentioning `mentioned`.
    async fn direct(pool: &PgPool, author: &User, mentioned: &[&User]) -> Status {
        let mentioned_user_ids = mentioned.iter().map(|user| user.id).collect();
        insert_status(
            pool,
            BASE,
            &NewStatus {
                visibility: Visibility::Direct,
                mentioned_user_ids,
                ..NewStatus::local(author, "a direct message")
            },
        )
        .await
        .expect("a direct post with mentions inserts")
    }

    /// Inserts a `private` status of `author` mentioning `mentioned`.
    async fn private_mentioning(pool: &PgPool, author: &User, mentioned: &[&User]) -> Status {
        let mentioned_user_ids = mentioned.iter().map(|user| user.id).collect();
        insert_status(
            pool,
            BASE,
            &NewStatus {
                visibility: Visibility::Private,
                mentioned_user_ids,
                ..NewStatus::local(author, "a followers-only reply")
            },
        )
        .await
        .expect("a private post with mentions inserts")
    }

    /// Inserts `reblogger`'s reblog of `original`, itself public.
    async fn reblog(pool: &PgPool, reblogger: &User, original: &Status) -> Status {
        insert_status(
            pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(original.id),
                ..NewStatus::local(reblogger, "")
            },
        )
        .await
        .expect("a reblog inserts")
    }

    /// Every stored row, tombstone included, oldest id first.
    async fn stored_statuses(pool: &PgPool) -> Vec<Status> {
        sqlx::query_as::<_, Status>("SELECT * FROM statuses ORDER BY id")
            .fetch_all(pool)
            .await
            .expect("the table reads")
    }

    /// The ids of `statuses`, in order.
    fn ids_of(statuses: &[&Status]) -> Vec<Uuid> {
        statuses.iter().map(|status| status.id).collect()
    }

    /// A set of ids, for comparing one reader's answer against another's
    /// whatever order each came back in.
    fn id_set(ids: &[Uuid]) -> HashSet<Uuid> {
        ids.iter().copied().collect()
    }

    /// The ids among `statuses` that [`can_view`] admits for `viewer`.
    async fn can_view_ids(pool: &PgPool, statuses: &[Status], viewer: Option<&User>) -> Vec<Uuid> {
        let mut visible = Vec::new();
        for status in statuses {
            if can_view(pool, status, viewer).await.unwrap() {
                visible.push(status.id);
            }
        }
        visible
    }

    /// The ids [`push_visibility_clause`] admits for `viewer`, selected
    /// straight off the table, oldest first.
    async fn clause_ids(pool: &PgPool, viewer: Option<&User>) -> Vec<Uuid> {
        let mut builder = QueryBuilder::<Postgres>::new("SELECT s.id FROM statuses s WHERE true");
        push_visibility_clause(&mut builder, viewer, "s");
        let rows = builder
            .build_query_as::<(Uuid,)>()
            .fetch_all(pool)
            .await
            .expect("the clause selects");
        rows.into_iter().map(|(id,)| id).collect()
    }

    /// The seeded matrix: five accounts and twelve statuses, each one a
    /// case of the rules — see [`seed`] for which is which.
    #[derive(Debug)]
    struct Matrix {
        /// The local author of everything blockable; blocks carol.
        alice: User,
        /// The local accepted follower of alice, pending follower of dave.
        bob: User,
        /// The local account alice blocked; author of two of her own.
        carol: User,
        /// The locked local author of one private status.
        dave: User,
        /// The remote mirror, tested as a viewer struct only: remote
        /// users never log in (decision 7).
        mallory: User,
        /// alice's `public` post.
        public: Status,
        /// alice's `unlisted` post.
        unlisted: Status,
        /// alice's `local` post: instance viewers only.
        local_post: Status,
        /// alice's `private` post: alice and bob only.
        private_post: Status,
        /// alice's `private` post mentioning mallory, a non-follower.
        private_to_mallory: Status,
        /// alice's `direct` post mentioning bob.
        dm_to_bob: Status,
        /// alice's `direct` post mentioning nobody but its author.
        dm_to_nobody: Status,
        /// dave's `private` post, with bob's request still pending.
        dave_private: Status,
        /// bob's reblog of alice's private post.
        reblog_of_private: Status,
        /// alice's public post, soft-deleted: visible to nobody.
        deleted_public: Status,
        /// carol's public post, seen past alice's block of her.
        carol_public: Status,
        /// carol's reblog of the deleted public post: hidden by rule 7.
        reblog_of_deleted: Status,
    }

    /// Writes the matrix: the accounts, their follow and block edges, and
    /// one status per interesting case (the [`Matrix`] fields name them).
    async fn seed(pool: &PgPool) -> Matrix {
        let alice = local(pool, "alice").await;
        let bob = local(pool, "bob").await;
        let carol = local(pool, "carol").await;
        let dave = locked_local(pool, "dave").await;
        let mallory = remote(pool, "mallory").await;

        let following = follow::follow(pool, BASE, &bob, &alice)
            .await
            .expect("bob's follow of open alice stores");
        assert_eq!(
            following.state,
            follow::FollowState::Accepted,
            "alice is unlocked, so the follow is granted"
        );
        let pending = follow::follow(pool, BASE, &bob, &dave)
            .await
            .expect("bob's request of locked dave stores");
        assert_eq!(
            pending.state,
            follow::FollowState::Pending,
            "dave is locked, so the request waits"
        );
        let blocked = block::block(pool, BASE, &alice, &carol)
            .await
            .expect("alice's block of carol stores");
        assert_eq!(
            blocked.blocked_id, carol.id,
            "the fixture block is alice's on carol"
        );

        let public = post(pool, &alice, Visibility::Public, "public").await;
        let unlisted = post(pool, &alice, Visibility::Unlisted, "unlisted").await;
        let local_post = post(pool, &alice, Visibility::Local, "local").await;
        let private_post = post(pool, &alice, Visibility::Private, "private").await;
        let private_to_mallory = private_mentioning(pool, &alice, &[&mallory]).await;
        let dm_to_bob = direct(pool, &alice, &[&bob]).await;
        let dm_to_nobody = post(pool, &alice, Visibility::Direct, "to nobody").await;
        let dave_private = post(pool, &dave, Visibility::Private, "dave private").await;
        let reblog_of_private = reblog(pool, &bob, &private_post).await;
        let tombstoned = post(pool, &alice, Visibility::Public, "deleted").await;
        let deleted_public = soft_delete(pool, tombstoned.id)
            .await
            .expect("the soft delete runs")
            .expect("the post was live");
        let carol_public = post(pool, &carol, Visibility::Public, "carol public").await;
        let reblog_of_deleted = reblog(pool, &carol, &deleted_public).await;

        Matrix {
            alice,
            bob,
            carol,
            dave,
            mallory,
            public,
            unlisted,
            local_post,
            private_post,
            private_to_mallory,
            dm_to_bob,
            dm_to_nobody,
            dave_private,
            reblog_of_private,
            deleted_public,
            carol_public,
            reblog_of_deleted,
        }
    }

    /// The full matrix, answered through `can_view`, the SQL clause and
    /// `filter_visible` for five viewers — author, follower, blocked,
    /// remote mirror and anonymous — with every row and every rule in
    /// play, including pending follows, mention-scoped directs,
    /// tombstones, and reblogs whose original fell out of view.
    #[tokio::test]
    async fn the_matrix_agrees_through_every_reader() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let m = seed(&pool).await;
        let all = stored_statuses(&pool).await;
        assert_eq!(all.len(), 12, "every fixture row is stored");
        assert!(
            !can_view(&pool, &m.deleted_public, Some(&m.alice))
                .await
                .unwrap(),
            "not even the author sees a tombstone"
        );
        assert!(
            can_view(&pool, &m.dave_private, Some(&m.dave))
                .await
                .unwrap(),
            "dave sees his own private post"
        );
        assert!(
            !can_view(&pool, &m.dave_private, Some(&m.bob))
                .await
                .unwrap(),
            "bob's pending request of locked dave is not a follower"
        );
        assert!(
            !can_view(&pool, &m.reblog_of_deleted, Some(&m.carol))
                .await
                .unwrap(),
            "carol's reblog falls with its deleted original (rule 7)"
        );

        let for_alice = ids_of(&[
            &m.public,
            &m.unlisted,
            &m.local_post,
            &m.private_post,
            &m.private_to_mallory,
            &m.dm_to_bob,
            &m.dm_to_nobody,
            &m.reblog_of_private,
            &m.carol_public,
        ]);
        let for_bob = ids_of(&[
            &m.public,
            &m.unlisted,
            &m.local_post,
            &m.private_post,
            &m.private_to_mallory,
            &m.dm_to_bob,
            &m.reblog_of_private,
            &m.carol_public,
        ]);
        let for_carol = ids_of(&[&m.carol_public]);
        let for_mallory = ids_of(&[
            &m.public,
            &m.unlisted,
            &m.private_to_mallory,
            &m.carol_public,
        ]);
        let for_anonymous = ids_of(&[&m.public, &m.unlisted, &m.carol_public]);

        for (name, viewer, want) in [
            ("alice", Some(&m.alice), for_alice),
            ("bob", Some(&m.bob), for_bob),
            ("carol", Some(&m.carol), for_carol),
            ("mallory", Some(&m.mallory), for_mallory),
            ("anonymous", None, for_anonymous),
        ] {
            let via_can_view = can_view_ids(&pool, &all, viewer).await;
            assert_eq!(id_set(&via_can_view), id_set(&want), "can_view, {name}");
            let via_clause = clause_ids(&pool, viewer).await;
            assert_eq!(id_set(&via_clause), id_set(&want), "clause, {name}");
            let filtered = filter_visible(&pool, all.clone(), viewer)
                .await
                .expect("the batch filter runs");
            let via_filter: Vec<Uuid> = filtered.iter().map(|status| status.id).collect();
            assert_eq!(id_set(&via_filter), id_set(&want), "filter_visible, {name}");
            let mut in_id_order = via_filter.clone();
            in_id_order.sort();
            assert_eq!(
                via_filter, in_id_order,
                "filter_visible, {name}, keeps the input (id) order"
            );
        }

        let empty = filter_visible(&pool, Vec::new(), Some(&m.alice))
            .await
            .expect("the empty batch needs no database");
        assert!(empty.is_empty(), "no rows in answers no rows out");
    }

    /// Rule 7 through both readers: bob's public reblog of alice's
    /// private post travels exactly as far as the original does — to
    /// alice and bob, and nowhere else.
    #[tokio::test]
    async fn a_reblog_is_visible_only_with_its_original() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let following = follow::follow(&pool, BASE, &bob, &alice)
            .await
            .expect("bob follows alice");
        assert_eq!(
            following.state,
            follow::FollowState::Accepted,
            "the fixture follow is granted"
        );
        let secret = post(&pool, &alice, Visibility::Private, "followers only").await;
        let reblogged = reblog(&pool, &bob, &secret).await;

        for viewer in [Some(&bob), Some(&alice)] {
            assert!(
                can_view(&pool, &reblogged, viewer).await.unwrap(),
                "whoever may see the original may see the reblog"
            );
        }
        for viewer in [Some(&carol), None] {
            assert!(
                !can_view(&pool, &reblogged, viewer).await.unwrap(),
                "whoever may not see the original may not see the reblog"
            );
        }

        assert_eq!(
            id_set(&clause_ids(&pool, Some(&bob)).await),
            id_set(&[secret.id, reblogged.id]),
            "the clause admits both rows to the follower"
        );
        assert_eq!(
            id_set(&clause_ids(&pool, Some(&alice)).await),
            id_set(&[secret.id, reblogged.id]),
            "and both to the original's author"
        );
        assert!(
            clause_ids(&pool, Some(&carol)).await.is_empty(),
            "the stranger gets neither row"
        );
        assert!(
            clause_ids(&pool, None).await.is_empty(),
            "and neither does the anonymous request"
        );
    }

    /// Rule 4 through both readers: a `local` status answers a local
    /// viewer, and loses to the anonymous request — remote users never
    /// log in, so that is the audience the rule can ever be asked about
    /// (a remote mirror passed as a viewer struct is refused the same
    /// way).
    #[tokio::test]
    async fn local_visibility_needs_a_local_viewer() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let mallory = remote(&pool, "mallory").await;
        let kept_local = post(&pool, &alice, Visibility::Local, "instance only").await;

        assert!(
            can_view(&pool, &kept_local, Some(&bob)).await.unwrap(),
            "a local member of the audience sees it"
        );
        assert!(
            !can_view(&pool, &kept_local, Some(&mallory)).await.unwrap(),
            "a remote mirror does not, even as a viewer struct"
        );
        assert!(
            !can_view(&pool, &kept_local, None).await.unwrap(),
            "the anonymous request does not"
        );

        assert_eq!(
            clause_ids(&pool, Some(&bob)).await,
            vec![kept_local.id],
            "the clause lets the local viewer through"
        );
        assert!(
            clause_ids(&pool, Some(&mallory)).await.is_empty(),
            "and shuts the remote viewer out"
        );
        assert!(
            clause_ids(&pool, None).await.is_empty(),
            "and shuts the anonymous request out"
        );
    }

    /// Rule 2 through both readers: alice's block shuts carol out of
    /// everything alice posted — public posts and a mention-scoped direct
    /// alike, because the block is judged before the visibility — while
    /// carol keeps her own posts and alice keeps carol's.
    #[tokio::test]
    async fn a_blocked_viewer_sees_nothing_of_the_author() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let blocked = block::block(&pool, BASE, &alice, &carol)
            .await
            .expect("alice's block of carol stores");
        assert_eq!(
            blocked.blocked_id, carol.id,
            "the fixture block is alice's on carol"
        );
        let open_post = post(&pool, &alice, Visibility::Public, "public anyway").await;
        let dm = direct(&pool, &alice, &[&carol]).await;
        let hers = post(&pool, &carol, Visibility::Public, "her own").await;

        assert!(
            !can_view(&pool, &open_post, Some(&carol)).await.unwrap(),
            "the block outranks public visibility"
        );
        assert!(
            !can_view(&pool, &dm, Some(&carol)).await.unwrap(),
            "and it outranks the mention that would have admitted her"
        );
        assert!(
            can_view(&pool, &hers, Some(&carol)).await.unwrap(),
            "she still sees her own"
        );
        assert!(
            can_view(&pool, &hers, Some(&alice)).await.unwrap(),
            "the rule hides the author from the blocked, not the author's view back"
        );

        assert_eq!(
            id_set(&clause_ids(&pool, Some(&carol)).await),
            id_set(&[hers.id]),
            "the clause keeps carol to her own row"
        );
        assert_eq!(
            id_set(&clause_ids(&pool, Some(&alice)).await),
            id_set(&[open_post.id, dm.id, hers.id]),
            "alice's own view is narrowed by nothing here"
        );
    }

    /// Rule 5's mentionee arm through every reader: bob does not follow
    /// alice, but her followers-only reply mentions him.
    #[tokio::test]
    async fn a_private_status_is_visible_to_a_mentioned_non_follower() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let reply = private_mentioning(&pool, &alice, &[&bob]).await;

        assert!(
            can_view(&pool, &reply, Some(&bob)).await.unwrap(),
            "the mentioned account sees it without following"
        );
        let kept = filter_visible(&pool, vec![reply.clone()], Some(&bob))
            .await
            .expect("the batch filter runs");
        assert_eq!(ids_of(&kept.iter().collect::<Vec<_>>()), vec![reply.id]);
        assert_eq!(clause_ids(&pool, Some(&bob)).await, vec![reply.id]);
        assert!(
            can_view(&pool, &reply, Some(&alice)).await.unwrap(),
            "the author still sees it"
        );
        assert!(
            !can_view(&pool, &reply, None).await.unwrap(),
            "an anonymous request is not mentioned"
        );
    }

    /// Regression: a private status that mentions someone else stays hidden
    /// from a non-follower, in both readers.
    #[tokio::test]
    async fn a_private_status_stays_hidden_from_an_unmentioned_non_follower() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let reply = private_mentioning(&pool, &alice, &[&bob]).await;

        assert!(
            !can_view(&pool, &reply, Some(&carol)).await.unwrap(),
            "carol is neither a follower nor mentioned"
        );
        let kept = filter_visible(&pool, vec![reply.clone()], Some(&carol))
            .await
            .expect("the batch filter runs");
        assert!(kept.is_empty(), "the batch filter drops it too");
        assert!(clause_ids(&pool, Some(&carol)).await.is_empty());
    }

    /// Rule 2 still outranks the mention: alice blocked bob, so her private
    /// post mentioning him is invisible to him.
    #[tokio::test]
    async fn a_mentioned_viewer_blocked_by_the_author_still_sees_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let _blocked = block::block(&pool, BASE, &alice, &bob)
            .await
            .expect("alice's block of bob stores");
        let reply = private_mentioning(&pool, &alice, &[&bob]).await;

        assert!(!can_view(&pool, &reply, Some(&bob)).await.unwrap());
        let kept = filter_visible(&pool, vec![reply], Some(&bob))
            .await
            .expect("the batch filter runs");
        assert!(kept.is_empty(), "the block wins in the batch filter too");
    }

    /// Rule 7 with a mentioned viewer: carol's public reblog of alice's
    /// private post, which mentions bob, reaches bob and nobody unmentioned.
    #[tokio::test]
    async fn a_reblog_rule_applies_to_a_mentioned_viewer() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let original = private_mentioning(&pool, &alice, &[&bob]).await;
        let boosted = reblog(&pool, &carol, &original).await;

        assert!(can_view(&pool, &boosted, Some(&bob)).await.unwrap());
        let for_bob = filter_visible(&pool, vec![boosted.clone()], Some(&bob))
            .await
            .expect("the batch filter runs");
        assert_eq!(
            ids_of(&for_bob.iter().collect::<Vec<_>>()),
            vec![boosted.id]
        );
        assert_eq!(
            id_set(&clause_ids(&pool, Some(&bob)).await),
            id_set(&[original.id, boosted.id])
        );

        assert!(!can_view(&pool, &boosted, Some(&dave)).await.unwrap());
        let for_dave = filter_visible(&pool, vec![boosted], Some(&dave))
            .await
            .expect("the batch filter runs");
        assert!(
            for_dave.is_empty(),
            "an unmentioned viewer loses the reblog"
        );
    }
}
