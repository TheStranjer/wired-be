//! The `conversation_mutes` table: one account opting one status thread out
//! of what it is shown.
//!
//! Reference: Mastodon's `conversation_mutes` (`db/schema.rb`) behind
//! `PUT /api/v1/conversations/:id/mute`, and Pleroma's muted
//! `Conversation.Participation` flag as `CommonAPI.thread_muted?`
//! (`lib/pleroma/web/common_api.ex`) reads it for
//! `Streamer.filtered_by_user?` (`lib/pleroma/web/streamer.ex`), the socket
//! filter this table exists for: [`crate::api::streaming`] loads a
//! connection's muted threads once and skips every event whose thread is one
//! of them.
//!
//! A thread is named by its [`Status::conversation_id`](crate::models::status::Status),
//! the uuid the root status mints and every reply inherits, so this table
//! holds no foreign key: the thread is a group of `statuses` rows, not a row
//! of its own, and deleting those statuses does not decide whether the
//! account still mutes the thread.
//!
//! [`mute`] and [`unmute`] are both idempotent on the ticket's
//! `(user_id, conversation_id)` key: the first mute of a thread writes the
//! row and a repeat writes nothing (`ON CONFLICT DO NOTHING`, keeping the
//! original row and its `inserted_at`), the first unmute deletes it and a
//! repeat deletes nothing. Deleting the account takes its thread mutes
//! through the foreign key's cascade.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

/// [`mute`]: the pair the caller names, with `inserted_at` left to the column
/// default. `ON CONFLICT (user_id, conversation_id) DO NOTHING` is the
/// idempotence: muting a thread that is already muted writes nothing.
const MUTE_SQL: &str = "INSERT INTO conversation_mutes (id, user_id, conversation_id)
     VALUES ($1, $2, $3)
ON CONFLICT (user_id, conversation_id) DO NOTHING";

/// [`unmute`]: one account's one thread; a never-muted or already opened
/// thread deletes nothing, so opening a thread is idempotent too.
const UNMUTE_SQL: &str =
    "DELETE FROM conversation_mutes WHERE user_id = $1 AND conversation_id = $2";

/// [`muted_ids`]: every thread one account mutes, oldest mute first on the id
/// whose text sorts in creation order (decision 4). The order is a stability
/// promise, not semantics: callers ask set membership, they do not page.
const ALL_IDS_SQL: &str =
    "SELECT conversation_id FROM conversation_mutes WHERE user_id = $1 ORDER BY id";

/// Records that `user_id` mutes the thread `conversation_id`, idempotently.
///
/// A thread that is already muted keeps its row and a repeat call writes
/// nothing; the model stores what it is handed and guards nothing itself
/// (whether the caller may see the thread is its endpoint's question).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mute(pool: &PgPool, user_id: Uuid, conversation_id: Uuid) -> AppResult<()> {
    sqlx::query(MUTE_SQL)
        .bind(new_id())
        .bind(user_id)
        .bind(conversation_id)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// Removes `user_id`'s mute of the thread `conversation_id`, idempotently.
///
/// A thread that was never muted (or already opened) deletes nothing and is
/// not an error.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unmute(pool: &PgPool, user_id: Uuid, conversation_id: Uuid) -> AppResult<()> {
    sqlx::query(UNMUTE_SQL)
        .bind(user_id)
        .bind(conversation_id)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The ids of the threads `user_id` mutes, oldest mute first.
///
/// The whole set in one statement: a streaming connection loads it once and
/// refreshes it on a timer, so it never asks the database about one thread at
/// a time. An account with no muted threads answers the empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn muted_ids(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(ALL_IDS_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

#[cfg(test)]
mod tests {
    use crate::{
        models::user::{NewUser, insert},
        test_db::TestDb,
    };

    use super::*;

    /// The id of a fresh local account named `nickname`.
    ///
    /// # Panics
    ///
    /// If the insert fails, e.g. because the nickname is taken.
    async fn account(pool: &PgPool, nickname: &str) -> Uuid {
        insert(
            pool,
            &NewUser::local(nickname, &format!("https://test.example/users/{nickname}")),
        )
        .await
        .expect("user fixture inserts")
        .id
    }

    /// A migrated, emptied database slot.
    async fn slot() -> (TestDb, PgPool) {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        (db, pool)
    }

    #[tokio::test]
    async fn muting_a_thread_records_it_once_and_is_idempotent() {
        let (_db, pool) = slot().await;
        let user_id = account(&pool, "thread-muter").await;
        let thread = Uuid::now_v7();

        mute(&pool, user_id, thread).await.unwrap();
        mute(&pool, user_id, thread)
            .await
            .expect("muting twice is not an error");

        assert_eq!(
            muted_ids(&pool, user_id).await.unwrap(),
            vec![thread],
            "the repeat wrote no second row"
        );
    }

    #[tokio::test]
    async fn muted_ids_holds_each_thread_once_and_nothing_else() {
        let (_db, pool) = slot().await;
        let user_id = account(&pool, "thread-muter").await;
        let other_user = account(&pool, "other-thread-muter").await;
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();

        mute(&pool, user_id, first).await.unwrap();
        mute(&pool, user_id, second).await.unwrap();
        mute(&pool, other_user, first).await.unwrap();

        assert_eq!(
            muted_ids(&pool, user_id).await.unwrap(),
            vec![first, second],
            "one account's mutes are its own, oldest first"
        );
    }

    #[tokio::test]
    async fn unmuting_a_thread_removes_it_once_and_never_fails() {
        let (_db, pool) = slot().await;
        let user_id = account(&pool, "thread-muter").await;
        let thread = Uuid::now_v7();
        mute(&pool, user_id, thread).await.unwrap();

        unmute(&pool, user_id, thread).await.unwrap();
        unmute(&pool, user_id, thread)
            .await
            .expect("unmuting an open thread is not an error");

        assert!(
            muted_ids(&pool, user_id).await.unwrap().is_empty(),
            "the mute is gone"
        );
    }

    #[tokio::test]
    async fn deleting_the_account_takes_its_thread_mutes() {
        let (_db, pool) = slot().await;
        let user_id = account(&pool, "thread-muter").await;
        let thread = Uuid::now_v7();
        mute(&pool, user_id, thread).await.unwrap();

        let _deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user_id)
            .execute(&pool)
            .await
            .expect("the user row deletes");

        assert!(
            muted_ids(&pool, user_id).await.unwrap().is_empty(),
            "the cascade left no orphan mute behind"
        );
    }

    #[tokio::test]
    async fn the_migration_keys_one_mute_per_account_and_thread() {
        let (_db, pool) = slot().await;

        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name FROM information_schema.columns
              WHERE table_schema = 'public' AND table_name = 'conversation_mutes'
           ORDER BY ordinal_position",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            columns,
            vec![
                "id".to_owned(),
                "user_id".to_owned(),
                "conversation_id".to_owned(),
                "inserted_at".to_owned(),
            ],
            "the migration's four columns, in its order"
        );

        let key: Vec<String> = sqlx::query_scalar(
            "SELECT att.attname
               FROM pg_constraint con
               JOIN pg_class cls ON cls.oid = con.conrelid
               JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
              WHERE con.contype = 'u' AND cls.relname = 'conversation_mutes'
           ORDER BY att.attname",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            key,
            vec!["conversation_id".to_owned(), "user_id".to_owned()],
            "the pair is the unique key the idempotent writes conflict on"
        );
    }
}
