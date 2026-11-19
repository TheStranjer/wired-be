//! Walking a thread: the raw recursive reads up ([`ancestor_chain`]) and
//! down ([`descendant_tree`]) a reply chain, and the write that closes a
//! deferred reply link once its parent is stored
//! ([`link_backfilled_reply`]).

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

use super::{Counter, Status, counters::step_counter};

/// [`ancestor_chain`]: the walk *up* from `$1` along `in_reply_to_id`, one
/// hop per row, stopped at depth `$2`. `$3` is again the starting id, which
/// the answer must not carry; the base row sits at depth `0` so a cap of
/// `1` still admits the direct parent. `ORDER BY chain.depth DESC` puts the
/// farthest link — the thread's root — first: a parent was stored before
/// its reply could name it, so id order is creation order (decision 4) and
/// walking up walks back in time. The walk is raw on purpose: a tombstone
/// mid-chain is a link the walk must pass through and keep, and whether any
/// row is *viewable* is the caller's [`filter_visible`] step.
///
/// [`filter_visible`]: crate::models::status_visibility::filter_visible
const ANCESTOR_CHAIN_SQL: &str = "WITH RECURSIVE chain(id, depth) AS (
    SELECT $1::uuid, 0
  UNION ALL
    SELECT s.in_reply_to_id, chain.depth + 1
      FROM chain
      JOIN statuses s ON s.id = chain.id
     WHERE s.in_reply_to_id IS NOT NULL
       AND chain.depth < $2
)
SELECT s.* FROM chain
  JOIN statuses s ON s.id = chain.id
 WHERE chain.id <> $3
 ORDER BY chain.depth DESC";

/// [`descendant_tree`]: the walk *down* from `$1` along replies, one hop per
/// row, stopped at depth `$2` and cut to `$3` rows. A reply was stored after
/// its parent, so `ORDER BY s.id` is the flat chronological thread order
/// (decision 4) Pleroma's context read answers. The base arm is the direct
/// replies at depth `1`, kept only when the cap admits at least one hop;
/// the cut runs before any visibility narrowing, so the caller's filters
/// can only shrink what `$3` kept. As in [`ancestor_chain`], tombstones and
/// unreadable rows stay in the walk — they are links below them the walk
/// must pass — and are the caller's to drop.
const DESCENDANT_TREE_SQL: &str = "WITH RECURSIVE tree(id, depth) AS (
    SELECT s.id, 1
      FROM statuses s
     WHERE s.in_reply_to_id = $1
       AND $2 >= 1
  UNION ALL
    SELECT c.id, tree.depth + 1
      FROM tree
      JOIN statuses c ON c.in_reply_to_id = tree.id
     WHERE tree.depth < $2
)
SELECT s.* FROM tree
  JOIN statuses s ON s.id = tree.id
 ORDER BY s.id
    LIMIT $3";

/// Every status *above* `status_id`: the walk up its
/// [`in_reply_to_id`](Status::in_reply_to_id) chain to the thread's root,
/// oldest first (root first), `status_id` itself excluded and the walk cut
/// at `max_depth` hops.
///
/// The answer is the raw chain, not a visible page: tombstones and rows no
/// viewer may read are kept (they are links the walk passes *through*, and
/// their own visibility is decided by the caller's
/// [`filter_visible`](crate::models::status_visibility::filter_visible)
/// step), and no parent of the chain is dropped when a row beside it falls
/// out. A status that replies to nothing answers the empty `Vec`, as does
/// one whose parent link dangles at its first hop.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn ancestor_chain(
    pool: &PgPool,
    status_id: Uuid,
    max_depth: i32,
) -> AppResult<Vec<Status>> {
    let statuses = sqlx::query_as::<_, Status>(ANCESTOR_CHAIN_SQL)
        .bind(status_id)
        .bind(max_depth)
        .bind(status_id)
        .fetch_all(pool)
        .await?;
    Ok(statuses)
}

/// Every status *below* `status_id`, across all branches, in flat
/// chronological thread order (id ascending, decision 4).
///
/// The walk descends through every reply of `status_id`, of every reply of
/// those, and so on — the order Pleroma's context read answers. `status_id`
/// itself is never in the answer; a status nobody replied to is the empty
/// `Vec`.
///
/// The walk is cut at `max_depth` hops (at least one for any row to come
/// back) and to `max_rows` rows before the caller has said which of them
/// are visible, so a caller that narrows afterwards may answer fewer than
/// `max_rows`.
///
/// The raw-read contract is [`ancestor_chain`]'s: tombstones and unreadable
/// rows stay in the walk so it continues past them, and the caller drops
/// them with `filter_visible`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn descendant_tree(
    pool: &PgPool,
    status_id: Uuid,
    max_depth: i32,
    max_rows: i64,
) -> AppResult<Vec<Status>> {
    let statuses = sqlx::query_as::<_, Status>(DESCENDANT_TREE_SQL)
        .bind(status_id)
        .bind(max_depth)
        .bind(max_rows)
        .fetch_all(pool)
        .await?;
    Ok(statuses)
}

/// [`link_backfilled_reply`]: closes a deferred reply link — the shape
/// [`crate::services::ingest`] leaves behind when a remote note answers an
/// object this instance does not store yet. The live-and-unlinked guard is
/// what makes the write idempotent: a second pass matches no row and steps
/// no counter, and a tombstoned child is not re-linked at all.
const LINK_BACKFILLED_SQL: &str = "UPDATE statuses
      SET in_reply_to_id = $2,
          in_reply_to_account_id = $3,
          conversation_id = $4,
          in_reply_to_ap_id = NULL,
          updated_at = now()
    WHERE id = $1 AND deleted_at IS NULL AND in_reply_to_id IS NULL
  RETURNING id";

/// Closes the deferred reply link of `child_id` once the answered status
/// `parent` is stored.
///
/// [`crate::services::ingest`] stores a remote reply answering an object
/// id it does not have; when that parent arrives, the row's
/// `in_reply_to_id`, `in_reply_to_account_id` and `conversation_id` take
/// the parent's values, its `in_reply_to_ap_id` clears, and the parent's
/// `replies_count` steps up, all on one transaction.
///
/// The write is idempotent by construction: the update only matches a
/// live row that is still unlinked (see `LINK_BACKFILLED_SQL`), so a
/// repeat pass — a redelivered backfill job, a second activity naming the
/// same parent — links nothing and steps nothing. `true` says this call
/// wrote the link.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); the rollback leaves the
/// child deferred and the counter untouched.
pub async fn link_backfilled_reply(
    pool: &PgPool,
    child_id: Uuid,
    parent: &Status,
) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    let linked = sqlx::query_scalar::<_, Uuid>(LINK_BACKFILLED_SQL)
        .bind(child_id)
        .bind(parent.id)
        .bind(parent.user_id)
        .bind(parent.conversation_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
    if linked {
        step_counter(&mut tx, parent.id, Counter::Replies, 1).await?;
    }
    tx.commit().await?;
    Ok(linked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::status::{soft_delete, test_support::*};

    /// The walk up answers the whole chain above the requested status,
    /// root first, the status itself never; one hop and no hops cut the
    /// walk to what the cap admits; and a tombstone mid-chain stays in the
    /// raw answer because the walk passes *through* it — narrowing to the
    /// visible chain is the caller's step.
    #[tokio::test]
    async fn ancestor_chain_walks_up_oldest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let root = post(&pool, &alice, "root").await;
        let middle = reply(&pool, &alice, &root, "middle").await;
        let leaf = reply(&pool, &alice, &middle, "leaf").await;
        assert_eq!(
            ids(&ancestor_chain(&pool, leaf.id, 100).await.unwrap()),
            vec![root.id, middle.id],
            "root first, then the direct parent, and never the requested \
             status itself"
        );
        assert!(
            ancestor_chain(&pool, root.id, 100)
                .await
                .unwrap()
                .is_empty(),
            "a root has nothing above it"
        );
        assert_eq!(
            ids(&ancestor_chain(&pool, leaf.id, 1).await.unwrap()),
            vec![middle.id],
            "one hop keeps only the direct parent"
        );
        assert!(
            ancestor_chain(&pool, leaf.id, 0).await.unwrap().is_empty(),
            "no hops walk nowhere"
        );
        let gone = soft_delete(&pool, middle.id)
            .await
            .unwrap()
            .expect("the middle reply was live");
        assert_eq!(
            ids(&ancestor_chain(&pool, leaf.id, 100).await.unwrap()),
            vec![root.id, gone.id],
            "the deleted link stays in the raw chain so the walk reaches \
             past it to the root"
        );
    }

    /// The walk down answers every branch in flat chronological (id-
    /// ascending) order, the requested status never; the depth cap stops
    /// the walk after that many hops and the row cap keeps the oldest rows
    /// of what it walked; a leaf has no tree, and a tombstone inside the
    /// tree stays in the raw answer for the caller to drop.
    #[tokio::test]
    async fn descendant_tree_walks_down_every_branch_in_id_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let root = post(&pool, &alice, "root").await;
        let left = reply(&pool, &alice, &root, "left").await;
        let right = reply(&pool, &alice, &root, "right").await;
        let deep = reply(&pool, &alice, &left, "deep").await;
        assert_eq!(
            ids(&descendant_tree(&pool, root.id, 100, 1000).await.unwrap()),
            vec![left.id, right.id, deep.id],
            "flat chronological order across branches: the second hop \
             follows its place in time, not its place in the tree"
        );
        assert!(
            descendant_tree(&pool, deep.id, 100, 1000)
                .await
                .unwrap()
                .is_empty(),
            "a leaf has nobody answering it"
        );
        assert_eq!(
            ids(&descendant_tree(&pool, root.id, 1, 1000).await.unwrap()),
            vec![left.id, right.id],
            "one hop keeps the direct replies and stops before the second"
        );
        assert!(
            descendant_tree(&pool, root.id, 0, 1000)
                .await
                .unwrap()
                .is_empty(),
            "no hops walk nowhere"
        );
        assert_eq!(
            ids(&descendant_tree(&pool, root.id, 100, 2).await.unwrap()),
            vec![left.id, right.id],
            "the row cap keeps the oldest rows of the walk"
        );
        let gone = soft_delete(&pool, right.id)
            .await
            .unwrap()
            .expect("the right reply was live");
        assert_eq!(
            ids(&descendant_tree(&pool, root.id, 100, 1000).await.unwrap()),
            vec![left.id, gone.id, deep.id],
            "the deleted reply is still a row of the raw tree"
        );
    }
}
