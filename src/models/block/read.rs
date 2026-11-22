//! The block model's reads: the pair questions, the flat id sets a
//! timeline or an inbox filters by, and the page behind
//! `GET /api/v1/blocks`.

use sqlx::{Executor, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

#[cfg(doc)]
use crate::models::follow;

use super::{Block, BlockUserRow};
use crate::{
    error::{AppError, AppResult},
    models::user::User,
    pagination::PageParams,
};

/// [`get`]: the one block of an ordered pair.
const GET_SQL: &str = "SELECT * FROM blocks WHERE blocker_id = $1 AND blocked_id = $2";

/// [`is_blocking`]: the one directional fact, asked as an existence check so a
/// blocker never costs more than a `true`.
const IS_BLOCKING_SQL: &str = "SELECT EXISTS (
    SELECT 1 FROM blocks WHERE blocker_id = $1 AND blocked_id = $2
)";

/// [`blocked_between`]: the same question in either direction, in one
/// statement — the pair is blocked as soon as one of the two rows is there.
const BLOCKED_BETWEEN_SQL: &str = "SELECT EXISTS (
    SELECT 1 FROM blocks
     WHERE (blocker_id = $1 AND blocked_id = $2)
        OR (blocker_id = $2 AND blocked_id = $1)
)";

/// [`blocked_ids`]: every account one blocker has blocked, oldest block first
/// (the `UUIDv7` text sorts in creation order, decision 4). The order is a
/// stability promise, not semantics: callers filter by membership and page
/// through [`blocking_page`].
const BLOCKED_IDS_SQL: &str = "SELECT blocked_id FROM blocks WHERE blocker_id = $1 ORDER BY id";

/// [`blocker_ids`]: every account that blocked one blocked account, oldest
/// block first — the incoming side of the same table, which is what the
/// `blocked_id` index serves.
const BLOCKER_IDS_SQL: &str = "SELECT blocker_id FROM blocks WHERE blocked_id = $1 ORDER BY id";

/// The leading text of [`blocking_page`]: the blocked accounts of one blocker,
/// each entry led by the columns of the `blocks` row that put them on the page
/// (aliased, because [`User`] has its own `id`, `ap_id` and `inserted_at`),
/// joined to the account on the other end. The statement stops where its one
/// placeholder belongs, because that is where [`blocking_page`] pushes its bind
/// and the builder is what numbers it `$1`; the keyset
/// [`PageParams::apply_keyset`] then appends runs on the block row's own `id`.
const PAGE_SQL: &str = "SELECT b.id AS block_id, b.blocker_id, b.blocked_id,
                               b.ap_id AS block_ap_id,
                               b.inserted_at AS block_inserted_at, u.*
                          FROM blocks b
                          JOIN users u ON u.id = b.blocked_id
                         WHERE b.blocker_id = ";

/// Whether `blocker` has blocked `target`.
///
/// The one directional fact the guards ask for: the follow guard in
/// [`crate::models::follow`] uses it to tell whose block refused a follow, and
/// a pair that blocked each other both ways answers it `true` in both
/// directions.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn is_blocking(pool: &PgPool, blocker: Uuid, target: Uuid) -> AppResult<bool> {
    let row = sqlx::query_scalar::<_, bool>(IS_BLOCKING_SQL)
        .bind(blocker)
        .bind(target)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Whether `first` and `second` have blocked each other, in either direction.
///
/// The question [`follow`] refuses on and the later delivery filters ask: one
/// existence check over the pair's two possible rows, so neither direction is
/// privileged. An unblocked pair — including one account with itself, which
/// has no row to find — answers `false`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn blocked_between(pool: &PgPool, first: Uuid, second: Uuid) -> AppResult<bool> {
    let row = sqlx::query_scalar::<_, bool>(BLOCKED_BETWEEN_SQL)
        .bind(first)
        .bind(second)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// The ids of the accounts `blocker` has blocked, oldest block first.
///
/// The whole set in one statement: [`crate::models::relationship`] answers
/// `blocking` from the same table, and the timeline and delivery filters that
/// hide a blocked account's posts take this flat form. A blocker with nothing
/// blocked answers the empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn blocked_ids(pool: &PgPool, blocker: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(BLOCKED_IDS_SQL)
        .bind(blocker)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(blocked_id,)| blocked_id).collect())
}

/// The ids of the accounts that have blocked `blocked`, oldest block first.
///
/// The incoming side of [`blocked_ids`], served by the table's `blocked_id`
/// index: what the `blocked_by` flag, the follow guard and (later) the inbox
/// that must not deliver a blocker's posts ask. An account nobody blocked
/// answers the empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn blocker_ids(pool: &PgPool, blocked: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(BLOCKER_IDS_SQL)
        .bind(blocked)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(blocker_id,)| blocker_id).collect())
}

/// The accounts `blocker` blocks, newest block first and `page`'s `limit` at a
/// time, each entry the [`Block`] row next to the [`User`] it names.
///
/// Like the relationship pages, the cursor and the `Link` header key on the
/// *block row* id and not on the accounts (decision 4), which is why the row
/// comes back beside the user. [`PageParams::finish`] flips a `min_id` fetch
/// back to newest-first, so every page is answered newest block first.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn blocking_page(
    pool: &PgPool,
    blocker: Uuid,
    page: PageParams,
) -> AppResult<Vec<(Block, User)>> {
    let mut builder = QueryBuilder::<Postgres>::new(PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(blocker);
    page.apply_keyset(&mut builder, "b.id");
    let rows = builder
        .build_query_as::<BlockUserRow>()
        .fetch_all(pool)
        .await?;
    Ok(page.finish(rows).into_iter().map(into_entry).collect())
}

/// The block of the ordered pair (`blocker`, `target`), whatever wrote it:
/// [`block`]'s read before and after its own write, on the pool or on the open
/// transaction it runs in.
pub(super) async fn get<'e, E>(executor: E, blocker: Uuid, target: Uuid) -> AppResult<Option<Block>>
where
    E: Executor<'e, Database = Postgres>,
{
    let block = sqlx::query_as::<_, Block>(GET_SQL)
        .bind(blocker)
        .bind(target)
        .fetch_optional(executor)
        .await?;
    Ok(block)
}

/// Splits [`BlockUserRow`] back into the pair [`blocking_page`] answers: the
/// [`Block`] of the aliased columns, next to the [`User`] the join flattened.
fn into_entry(row: BlockUserRow) -> (Block, User) {
    let block = Block {
        id: row.block_id,
        blocker_id: row.blocker_id,
        blocked_id: row.blocked_id,
        ap_id: row.block_ap_id,
        inserted_at: row.block_inserted_at,
    };
    (block, row.user)
}

/// Turns a row a statement did not guarantee into a result, so a block a
/// concurrent write removed answers like any other absent record: 404.
pub(super) fn required(block: Option<Block>) -> AppResult<Block> {
    block.map_or_else(|| Err(AppError::NotFound), Ok)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::super::{
        test_support::{
            activity_id, blocked_ids_of, local_user, page, row_ids_of, seed_block, seed_unblock,
            stored_block, stored_rows,
        },
        write::block,
    };
    use super::*;
    use crate::{config::TEST_BASE_URL, util::id::parse_id};

    #[tokio::test]
    async fn a_block_is_written_and_read_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let row = block(&pool, TEST_BASE_URL, &alice, &bob)
            .await
            .expect("the fixture block stores");

        assert!(row.id.get_version_num() == 7, "the id is a UUIDv7");
        assert_eq!(row.blocker_id, alice.id, "who blocks");
        assert_eq!(row.blocked_id, bob.id, "who is blocked");
        assert!(row.inserted_at <= Utc::now(), "the database stamps it");
        let tail = activity_id(&row);
        assert!(parse_id(&tail).is_some(), "the minted id is a UUID: {tail}");
        assert_eq!(
            stored_block(&pool, &alice, &bob).await,
            row,
            "get() reads back the row it wrote"
        );

        assert!(
            is_blocking(&pool, alice.id, bob.id).await.unwrap(),
            "alice blocks bob"
        );
        assert!(
            !is_blocking(&pool, bob.id, alice.id).await.unwrap(),
            "blocking is directional"
        );
        assert!(
            blocked_between(&pool, alice.id, bob.id).await.unwrap(),
            "either direction answers the pair blocked"
        );
        assert!(
            blocked_between(&pool, bob.id, alice.id).await.unwrap(),
            "and the question does not care which way it is asked"
        );
        assert_eq!(
            blocked_ids(&pool, alice.id).await.unwrap(),
            vec![bob.id],
            "bob is on alice's list"
        );
        assert!(
            blocked_ids(&pool, bob.id).await.unwrap().is_empty(),
            "and alice is on nobody's"
        );
        assert_eq!(
            blocker_ids(&pool, bob.id).await.unwrap(),
            vec![alice.id],
            "alice is on bob's blockers"
        );
        assert!(
            blocker_ids(&pool, alice.id).await.unwrap().is_empty(),
            "and nobody blocks alice"
        );

        let entries = blocking_page(&pool, alice.id, page("")).await.unwrap();
        assert_eq!(
            blocked_ids_of(&entries),
            vec![bob.id],
            "the page names the blocked account"
        );
        assert_eq!(
            row_ids_of(&entries),
            vec![row.id],
            "and carries the block row beside it"
        );
        assert_eq!(entries[0].1.id, bob.id, "the user is the blocked one");
        assert_eq!(entries[0].0, row, "and the row is the stored one");
        assert!(
            blocking_page(&pool, bob.id, page(""))
                .await
                .unwrap()
                .is_empty(),
            "bob's own page holds nothing"
        );
    }

    /// Deleting either account takes the block through the foreign key's
    /// cascade, from both ends of the pair.
    #[tokio::test]
    async fn deleting_either_side_takes_the_block() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        seed_block(&pool, &alice, &bob).await;
        seed_block(&pool, &carol, &alice).await;

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the blocked account deletes");
        assert_eq!(deleted.rows_affected(), 1, "one account went");
        assert_eq!(
            blocked_ids(&pool, alice.id).await.unwrap(),
            Vec::<Uuid>::new(),
            "alice's block of the deleted account went with it"
        );

        let gone = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the blocking account deletes");
        assert_eq!(gone.rows_affected(), 1, "and so did the other blocker");
        assert_eq!(
            blocker_ids(&pool, alice.id).await.unwrap(),
            Vec::<Uuid>::new(),
            "carol's block of the deleted account cascaded too"
        );
    }

    #[tokio::test]
    async fn blocks_are_each_account_s_own() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        seed_block(&pool, &alice, &carol).await;
        seed_block(&pool, &bob, &carol).await;

        assert!(
            blocked_ids(&pool, carol.id).await.unwrap().is_empty(),
            "carol holds nothing of the blocks made against her"
        );
        assert_eq!(
            blocker_ids(&pool, carol.id).await.unwrap(),
            stored_rows(&pool)
                .await
                .into_iter()
                .map(|(_, blocker, _)| blocker)
                .rev()
                .collect::<Vec<Uuid>>(),
            "both blockers are listed, oldest block first"
        );

        seed_unblock(&pool, &alice, &carol).await;
        assert!(
            blocked_ids(&pool, alice.id).await.unwrap().is_empty(),
            "alice's withdrawal left her own set empty"
        );
        assert_eq!(
            blocked_ids(&pool, bob.id).await.unwrap(),
            vec![carol.id],
            "and bob's alone"
        );
    }

    #[tokio::test]
    async fn a_page_of_blocks_newest_first_walks_by_row_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let erin = local_user(&pool, "erin").await;
        for nickname in ["bob", "carol", "dave"] {
            let blocked = local_user(&pool, nickname).await;
            seed_block(&pool, &alice, &blocked).await;
        }
        let stored = stored_rows(&pool).await;
        assert_eq!(stored.len(), 3, "three fixture blocks");

        let first = blocking_page(&pool, alice.id, page("limit=2"))
            .await
            .expect("the first page reads");
        assert_eq!(
            row_ids_of(&first),
            vec![stored[0].0, stored[1].0],
            "the two newest rows"
        );
        assert_eq!(
            blocked_ids_of(&first),
            vec![stored[0].2, stored[1].2],
            "and their blocked accounts, newest block first"
        );

        let older = blocking_page(&pool, alice.id, page(&format!("max_id={}", stored[1].0)))
            .await
            .expect("the continuation reads");
        assert_eq!(
            row_ids_of(&older),
            vec![stored[2].0],
            "max_id continues where the page ended"
        );

        let newer = blocking_page(&pool, alice.id, page(&format!("min_id={}", stored[2].0)))
            .await
            .expect("the forward page reads");
        assert_eq!(
            row_ids_of(&newer),
            vec![stored[0].0, stored[1].0],
            "a min_id page is answered newest-first too"
        );

        let tail = blocking_page(&pool, alice.id, page(&format!("max_id={}", stored[2].0)))
            .await
            .expect("walking off the end is no error");
        assert!(tail.is_empty(), "and answers nothing");

        let other = blocking_page(&pool, erin.id, page(""))
            .await
            .expect("a blocker with nothing blocked pages");
        assert!(other.is_empty(), "and holds nothing of alice's blocks");
    }
}
