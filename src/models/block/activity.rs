//! The `Block` activity id a block row carries: recording an inbound
//! activity's id and finding a block by it.

use sqlx::PgPool;
use uuid::Uuid;

use super::Block;
#[cfg(doc)]
use super::write::block;
use crate::error::AppResult;

/// [`get_by_ap_id`]: the one block behind a `Block` activity id — the
/// `ap_id` unique index answers the lookup directly.
const GET_BY_AP_ID_SQL: &str = "SELECT * FROM blocks WHERE ap_id = $1";

/// [`get_by_blocker_and_ap_id`]: the block of one blocker whose `Block`
/// activity carries one id (decision 6: a local block's `ap_id` is
/// `{BASE_URL}/activities/{uuid}`). Scoping the lookup to the blocker keeps
/// somebody else's activity id unfindable here, so an `Undo` can only lift
/// its own maker's block.
const GET_BY_BLOCKER_AND_AP_ID_SQL: &str =
    "SELECT * FROM blocks WHERE blocker_id = $1 AND ap_id = $2";

/// [`set_ap_id`]: the mirror write of an inbound `Block` — the row keeps its
/// identity, only the activity id it stands for arrives. Unknown ids match
/// no row and change nothing.
const SET_AP_ID_SQL: &str = "UPDATE blocks SET ap_id = $2 WHERE id = $1";

/// Records `ap_id` as the `Block` activity the stored block `id` stands
/// for: the federation mirror's write (decision 6).
///
/// [`block`] leaves a remote blocker's row without an id because that id
/// is the one the blocker's own instance minted, and this is where the
/// inbound `Block` activity's id lands on it. The unique `ap_id` index
/// refuses a second block under one id, and an unknown `id` matches no row
/// and changes nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding, an `ap_id` another row
/// already carries) arrives as [`AppError::Database`](crate::error::AppError).
pub async fn set_ap_id(pool: &PgPool, id: Uuid, ap_id: &str) -> AppResult<()> {
    sqlx::query(SET_AP_ID_SQL)
        .bind(id)
        .bind(ap_id)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The block whose `Block` activity carries `ap_id`, `None` when no stored
/// block does: how the federation `Undo` that names only the activity id
/// finds the pair to lift.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_ap_id(pool: &PgPool, ap_id: &str) -> AppResult<Option<Block>> {
    let block = sqlx::query_as::<_, Block>(GET_BY_AP_ID_SQL)
        .bind(ap_id)
        .fetch_optional(pool)
        .await?;
    Ok(block)
}

/// The [`Block`] of `blocker_id` whose `Block` activity carries `ap_id`,
/// `None` when the account has no such block.
///
/// Scoping the lookup to the blocker is the point: somebody else quoting
/// the id finds nothing, so a C2S `Undo` can only lift the maker's own
/// block.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_blocker_and_ap_id(
    pool: &PgPool,
    blocker_id: Uuid,
    ap_id: &str,
) -> AppResult<Option<Block>> {
    let block = sqlx::query_as::<_, Block>(GET_BY_BLOCKER_AND_AP_ID_SQL)
        .bind(blocker_id)
        .bind(ap_id)
        .fetch_optional(pool)
        .await?;
    Ok(block)
}

#[cfg(test)]
mod tests {
    use super::super::{
        test_support::{local_user, remote_user, seed_block, stored_block},
        write::block,
    };
    use super::*;
    use crate::{config::TEST_BASE_URL, error::AppError, util::id::new_id};

    /// The federation mirror's pair of writes: [`set_ap_id`] lands the remote
    /// activity id on the row [`block`] left without one, and [`get_by_ap_id`]
    /// reads that row back by the id — the path an `Undo` of a bare activity
    /// id resolves through.
    #[tokio::test]
    async fn an_activity_id_lands_on_the_row_and_reads_it_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = remote_user(&pool, "carol").await;
        let alice = local_user(&pool, "alice").await;
        let row = block(&pool, TEST_BASE_URL, &carol, &alice)
            .await
            .expect("the fixture block stores");
        assert!(row.ap_id.is_none(), "the remote blocker's row starts bare");

        let activity = "https://remote.example/activities/block-77";
        set_ap_id(&pool, row.id, activity).await.unwrap();
        let mirrored = stored_block(&pool, &carol, &alice).await;
        assert_eq!(
            mirrored.ap_id.as_deref(),
            Some(activity),
            "the delivered activity id is the row's now"
        );
        assert_eq!(mirrored.id, row.id, "and the row itself did not move");
        let found = get_by_ap_id(&pool, activity).await.unwrap();
        assert_eq!(
            found
                .as_ref()
                .map(|found| (found.id, found.ap_id.as_deref())),
            Some((row.id, Some(activity))),
            "the id reads the block back, now carrying that id"
        );

        set_ap_id(&pool, row.id, activity)
            .await
            .expect("restating the same id is no error");
        assert_eq!(
            stored_block(&pool, &carol, &alice).await.ap_id.as_deref(),
            Some(activity),
            "and writes the same value"
        );
        assert!(
            get_by_ap_id(&pool, "https://remote.example/activities/never")
                .await
                .unwrap()
                .is_none(),
            "an id no block carries resolves to nothing"
        );
        set_ap_id(&pool, new_id(), "https://remote.example/activities/orphan")
            .await
            .expect("an unknown block id changes nothing and is no error");
    }

    /// One `Block` activity ids one block: the unique `ap_id` refuses a
    /// second row under the same id.
    #[tokio::test]
    async fn one_activity_ids_one_block() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = remote_user(&pool, "carol").await;
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let row = block(&pool, TEST_BASE_URL, &carol, &alice)
            .await
            .expect("the fixture block stores");
        let activity = "https://remote.example/activities/block-78";
        set_ap_id(&pool, row.id, activity).await.unwrap();

        let second = block(&pool, TEST_BASE_URL, &carol, &bob)
            .await
            .expect("the second pair blocks");
        let taken = set_ap_id(&pool, second.id, activity)
            .await
            .expect_err("the id is spent on one row");
        assert!(
            matches!(taken, AppError::Database(_)),
            "the unique index is the guard: {taken}"
        );
        assert_eq!(
            stored_block(&pool, &carol, &bob).await.ap_id,
            None,
            "and the refused write stored nothing"
        );
    }

    /// [`get_by_blocker_and_ap_id`]: the C2S `Undo` read — the blocker's own
    /// row answers its minted activity id, and the same id quoted by another
    /// account (or a stranger id) finds nothing.
    #[tokio::test]
    async fn a_block_answers_its_maker_by_activity_id_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let carol = local_user(&pool, "carol").await;
        let bob = local_user(&pool, "bob").await;
        seed_block(&pool, &alice, &bob).await;

        let row = stored_block(&pool, &alice, &bob).await;
        let ap_id = row.ap_id.expect("a local block carries an activity id");
        let found = get_by_blocker_and_ap_id(&pool, alice.id, &ap_id)
            .await
            .expect("the read runs")
            .expect("the maker finds its own block");
        assert_eq!(found.id, row.id, "and reads the row behind the id");
        assert!(
            get_by_blocker_and_ap_id(&pool, carol.id, &ap_id)
                .await
                .expect("the read runs")
                .is_none(),
            "somebody else quoting the id names none of their own blocks"
        );
    }
}
