//! The delete side of [`Participation::last_status_id`]: the gate, the
//! target finder, and the two writes behind [`repoint_after_delete`],
//! which moves every pointer off a tombstoned status or drops the
//! pointers whose thread went dark.
//!
//! [`Participation::last_status_id`]: super::Participation::last_status_id

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::error::AppResult;

/// [`repoint_after_delete`]'s gate: does any participation still name the
/// deleted status, and of which thread are they — all of them, since a
/// participation's `last_status_id` only ever names a status of its own
/// conversation.
const REPOINTED_THREAD_SQL: &str = "SELECT conversation_id FROM conversation_participations \
     WHERE last_status_id = $1 LIMIT 1";

/// [`repoint_after_delete`]'s target: the conversation's newest live direct
/// status left (id descending is creation order, decision 4). The deleted
/// row is a tombstone by the time this runs, so it cannot name itself.
const NEWEST_REMAINING_SQL: &str = "SELECT id FROM statuses
     WHERE direct_conversation_id = $1
       AND visibility = 'direct'
       AND deleted_at IS NULL
     ORDER BY id DESC
     LIMIT 1";

/// [`repoint_after_delete`]'s write: every participation that named the
/// deleted status moves to the newest one left. `updated_at` stays: a
/// repoint restates the past, it is not a new bump.
const REPOINT_SQL: &str = "UPDATE conversation_participations
      SET last_status_id = $2
    WHERE last_status_id = $1";

/// [`repoint_after_delete`]'s other arm: the conversation ran out of live
/// direct statuses, so the participations that named its last one go — and
/// their recipient rows cascade with them.
const DROP_POINTING_SQL: &str = "DELETE FROM conversation_participations WHERE last_status_id = $1";

/// Moves every participation that named the just-deleted status
/// `deleted_id` to its conversation's newest remaining direct status.
///
/// Or deletes it, when the conversation has no live direct status left
/// and its recipient rows cascade with the participation.
///
/// The soft delete of a status that is nobody's `last_status_id` (an older
/// post of the thread, a non-DM post, a reblog) is the no-op it should be:
/// no participation names it, and nothing else runs.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`] and the rollback
/// leaves the pointers exactly as the delete left them.
///
/// [`AppError::Database`]: crate::error::AppError
pub async fn repoint_after_delete(pool: &PgPool, deleted_id: Uuid) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    repoint_pointers(&mut tx, deleted_id).await?;
    tx.commit().await?;
    Ok(())
}

/// [`repoint_after_delete`]'s work on the caller's transaction: gate on
/// some participation still naming the deleted status, then take the one
/// arm the conversation's state leaves — repoint every pointer onto the
/// newest status still live, or drop the pointers whose thread went dark.
async fn repoint_pointers(tx: &mut PgConnection, deleted_id: Uuid) -> AppResult<()> {
    let pointed = sqlx::query_as::<_, (Uuid,)>(REPOINTED_THREAD_SQL)
        .bind(deleted_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((conversation_id,)) = pointed else {
        return Ok(());
    };
    let remaining = sqlx::query_as::<_, (Uuid,)>(NEWEST_REMAINING_SQL)
        .bind(conversation_id)
        .fetch_optional(&mut *tx)
        .await?;
    match remaining {
        Some((newest_id,)) => {
            sqlx::query(REPOINT_SQL)
                .bind(deleted_id)
                .bind(newest_id)
                .execute(&mut *tx)
                .await
                .map(drop)?;
        }
        None => {
            sqlx::query(DROP_POINTING_SQL)
                .bind(deleted_id)
                .execute(&mut *tx)
                .await
                .map(drop)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::models::status;

    use crate::models::conversation::test_support::{
        bump, direct, direct_reply, linked_thread, local, participation, slot,
    };

    #[tokio::test]
    async fn deleting_the_newest_status_repoints_and_emptiness_removes() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let first = direct(&pool, &alice, &[&bob], "@bob psst").await;
        bump(&pool, &first).await;
        let second = direct_reply(&pool, &bob, &first, &[&alice], "@alice back").await;
        bump(&pool, &second).await;
        let thread = linked_thread(&pool, &first).await;

        let _gone = status::soft_delete(&pool, second.id)
            .await
            .expect("the newest fixture deletes");
        repoint_after_delete(&pool, second.id)
            .await
            .expect("the repoint runs");
        let alice_row = participation(&pool, alice.id, thread)
            .await
            .expect("alice still participates");
        assert_eq!(
            alice_row.last_status_id,
            Some(first.id),
            "her pointer moved to the newest status left"
        );

        let _older_gone = status::soft_delete(&pool, first.id)
            .await
            .expect("the older fixture deletes");
        repoint_after_delete(&pool, first.id)
            .await
            .expect("the second repoint runs");
        assert!(
            participation(&pool, alice.id, thread).await.is_none(),
            "with no live status left, alice's participation is gone"
        );
        assert!(
            participation(&pool, bob.id, thread).await.is_none(),
            "and bob's with it"
        );
    }

    #[tokio::test]
    async fn deleting_a_status_nobody_points_at_moves_nothing() {
        let (_db, pool) = slot().await;
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let first = direct(&pool, &alice, &[&bob], "@bob psst").await;
        bump(&pool, &first).await;
        let second = direct_reply(&pool, &alice, &first, &[&bob], "@bob again").await;
        bump(&pool, &second).await;
        let thread = linked_thread(&pool, &first).await;

        let _gone = status::soft_delete(&pool, first.id)
            .await
            .expect("the older status deletes");
        repoint_after_delete(&pool, first.id)
            .await
            .expect("the repoint runs");

        let alice_row = participation(&pool, alice.id, thread)
            .await
            .expect("the participation survived");
        assert_eq!(
            alice_row.last_status_id,
            Some(second.id),
            "the pointer never named the deleted row"
        );
    }

    #[tokio::test]
    async fn the_migration_shapes_the_conversation_tables() {
        let (_db, pool) = slot().await;

        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name FROM information_schema.columns
              WHERE table_schema = 'public' AND table_name = 'conversation_participations'
           ORDER BY ordinal_position",
        )
        .fetch_all(&pool)
        .await
        .expect("the columns read");
        assert_eq!(
            columns,
            vec![
                "id".to_owned(),
                "user_id".to_owned(),
                "conversation_id".to_owned(),
                "read".to_owned(),
                "last_status_id".to_owned(),
                "inserted_at".to_owned(),
                "updated_at".to_owned(),
            ],
            "the migration's seven columns, in its order"
        );

        let status_column: Option<String> = sqlx::query_scalar(
            "SELECT column_name FROM information_schema.columns
              WHERE table_schema = 'public' AND table_name = 'statuses'
                AND column_name = 'direct_conversation_id'",
        )
        .fetch_optional(&pool)
        .await
        .expect("the column reads");
        assert_eq!(
            status_column.as_deref(),
            Some("direct_conversation_id"),
            "statuses grew the thread column"
        );

        let recipient_key: Vec<String> = sqlx::query_scalar(
            "SELECT att.attname
               FROM pg_constraint con
               JOIN pg_class cls ON cls.oid = con.conrelid
               JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
              WHERE con.contype = 'p' AND cls.relname = 'conversation_participation_recipients'
           ORDER BY con.conkey",
        )
        .fetch_all(&pool)
        .await
        .expect("the key reads");
        assert_eq!(
            recipient_key,
            vec!["participation_id".to_owned(), "user_id".to_owned(),],
            "the recipient table's pair primary key"
        );
    }
}
