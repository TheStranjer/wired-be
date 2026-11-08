//! The `conversation_mutes` table: one account opting one status thread out
//! of what it is shown.
//!
//! Reference: Mastodon's `conversation_mutes` (`db/schema.rb`) behind
//! `PUT /api/v1/conversations/:id/mute`, and Pleroma's `ThreadMute`
//! (`lib/pleroma/thread_mute.ex`) behind `POST /api/v1/statuses/:id/mute`
//! and its `…/unmute` twin — Pleroma's `CommonAPI.thread_muted?`
//! (`lib/pleroma/web/common_api.ex`) reads it for
//! `Streamer.filtered_by_user?` (`lib/pleroma/web/streamer.ex`), the socket
//! filter this table exists for: [`crate::api::streaming`] loads a
//! connection's muted threads once and skips every event whose thread is one
//! of them.
//!
//! This table is the instance's one thread-mute fact, read and written from
//! three sides:
//!
//! - [`crate::api::statuses`]' `POST /api/v1/statuses/{id}/mute` and
//!   `…/unmute` write it through [`mute`] and [`unmute`], after
//!   [`participates_in`] has said the caller is part of the thread
//!   (Mastodon's rule for these two routes: only an author of, or a
//!   mention in, one of its statuses may mute it);
//! - [`crate::services::render_status`] answers the Status entity's `muted`
//!   and `pleroma.thread_muted` flags from one batched
//!   [`muted_conversation_ids`] read per rendered page;
//! - the notification tickets suppress notifications about a muted thread
//!   through [`is_muted`] — the single-thread membership read those writes
//!   run before they record a notification about a status's thread.
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

use std::collections::HashSet;

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

/// [`muted_conversation_ids`]: which of the asked-for threads one account
/// mutes. `= ANY($2)` is the batch form the page renderer needs — one read
/// per rendered page, whatever its size (decision 3's fixed query set).
const MUTED_IN_SQL: &str = "SELECT conversation_id FROM conversation_mutes \
     WHERE user_id = $1 AND conversation_id = ANY($2)";

/// [`is_muted`]: the one (account, thread) pair the mute table is keyed on,
/// asked as a yes or a no.
const IS_MUTED_SQL: &str = "SELECT EXISTS (SELECT 1 FROM conversation_mutes \
     WHERE user_id = $1 AND conversation_id = $2)";

/// [`participates_in`]: whether `user_id` authored a status of the thread,
/// or is mentioned in one of its statuses — the two facts that make an
/// account *part of* a conversation. Tombstones count: a delete does not
/// undo the authorship, exactly as Pleroma's participation rows outlive the
/// activities behind them.
const PARTICIPATES_SQL: &str = "SELECT EXISTS (SELECT 1 FROM statuses \
     WHERE conversation_id = $2 AND user_id = $1) \
    OR EXISTS (SELECT 1 FROM status_mentions m JOIN statuses s ON s.id = m.status_id \
     WHERE s.conversation_id = $2 AND m.user_id = $1)";

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

/// Which of `conversation_ids` `user_id` mutes, as a membership set.
///
/// The Status renderer's one read per page (see the
/// [module docs](self)): every rendered row's `muted` and
/// `pleroma.thread_muted` flag is membership in the set this answers, keyed
/// by the row's own `conversation_id`. An empty ask skips the database — a
/// page mutes nothing it never asked about — and a non-muted id is simply
/// absent from the set.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn muted_conversation_ids(
    pool: &PgPool,
    user_id: Uuid,
    conversation_ids: &[Uuid],
) -> AppResult<HashSet<Uuid>> {
    if conversation_ids.is_empty() {
        return Ok(HashSet::new());
    }
    let rows = sqlx::query_as::<_, (Uuid,)>(MUTED_IN_SQL)
        .bind(user_id)
        .bind(conversation_ids)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Whether `user_id` mutes the thread `conversation_id`.
///
/// The single-thread form of [`muted_conversation_ids`], for the writes that
/// must know a thread's mute state before they act on it: the notification
/// tickets call it to suppress notifications about muted threads (see the
/// [module docs](self)). A thread never muted is `false`, never an error.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn is_muted(pool: &PgPool, user_id: Uuid, conversation_id: Uuid) -> AppResult<bool> {
    let (muted,): (bool,) = sqlx::query_as(IS_MUTED_SQL)
        .bind(user_id)
        .bind(conversation_id)
        .fetch_one(pool)
        .await?;
    Ok(muted)
}

/// Whether `user_id` is *part of* the thread `conversation_id`: authored
/// one of its statuses or is mentioned in one of them.
///
/// Those are the two facts Mastodon demands of an account muting a
/// conversation (its `POST /api/v1/statuses/:id/mute` refuses a stranger).
/// [`crate::api::statuses`]' mute route asks this behind its visibility gate
/// and answers the shared 404 when it says no. An unknown thread has no
/// participants here, and neither does a thread nobody authored or
/// mentioned into existence.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn participates_in(
    pool: &PgPool,
    user_id: Uuid,
    conversation_id: Uuid,
) -> AppResult<bool> {
    let (found,): (bool,) = sqlx::query_as(PARTICIPATES_SQL)
        .bind(user_id)
        .bind(conversation_id)
        .fetch_one(pool)
        .await?;
    Ok(found)
}

#[cfg(test)]
mod tests {
    use crate::{
        config::TEST_BASE_URL,
        models::{
            status,
            user::{NewUser, insert},
        },
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

    #[tokio::test]
    async fn muted_conversation_ids_answers_membership_of_the_ask_only() {
        let (_db, pool) = slot().await;
        let user_id = account(&pool, "thread-muter").await;
        let other_user = account(&pool, "other-thread-muter").await;
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        let third = Uuid::now_v7();
        mute(&pool, user_id, first).await.unwrap();
        mute(&pool, user_id, second).await.unwrap();
        mute(&pool, other_user, third).await.unwrap();

        let asked = [first, second, third, Uuid::now_v7()];
        let muted = muted_conversation_ids(&pool, user_id, &asked)
            .await
            .unwrap();
        assert_eq!(
            asked
                .iter()
                .filter(|id| muted.contains(*id))
                .copied()
                .collect::<Vec<Uuid>>(),
            vec![first, second],
            "the muted pair comes back, another account's mute and the \
             never-muted id do not"
        );

        assert!(
            muted_conversation_ids(&pool, user_id, &[])
                .await
                .unwrap()
                .is_empty(),
            "an empty ask is the empty set"
        );
    }

    #[tokio::test]
    async fn is_muted_follows_the_pair_through_the_mute_and_the_unmute() {
        let (_db, pool) = slot().await;
        let user_id = account(&pool, "thread-muter").await;
        let bystander = account(&pool, "thread-bystander").await;
        let thread = Uuid::now_v7();
        let other_thread = Uuid::now_v7();

        assert!(
            !is_muted(&pool, user_id, thread).await.unwrap(),
            "a never-muted thread is not muted"
        );
        mute(&pool, user_id, thread).await.unwrap();
        assert!(
            is_muted(&pool, user_id, thread).await.unwrap(),
            "the mute reads back"
        );
        assert!(
            !is_muted(&pool, user_id, other_thread).await.unwrap(),
            "a thread the account never muted stays open"
        );
        assert!(
            !is_muted(&pool, bystander, thread).await.unwrap(),
            "one account's mute is nobody else's fact"
        );
        unmute(&pool, user_id, thread).await.unwrap();
        assert!(
            !is_muted(&pool, user_id, thread).await.unwrap(),
            "the withdrawal reads back too"
        );
    }

    #[tokio::test]
    async fn participation_names_authors_mentioned_accounts_and_repliers() {
        let (_db, pool) = slot().await;
        let alice = user_row(&pool, "alice").await;
        let bob = user_row(&pool, "bob").await;
        let carol = account(&pool, "carol").await;
        let stranger = account(&pool, "stranger").await;
        let root = status::insert(
            &pool,
            TEST_BASE_URL,
            &status::NewStatus {
                mentioned_user_ids: vec![carol],
                ..status::NewStatus::local(&alice, "@carol look at this thread")
            },
        )
        .await
        .expect("the fixture root inserts");
        let reply = status::insert(
            &pool,
            TEST_BASE_URL,
            &status::NewStatus {
                in_reply_to_id: Some(root.id),
                in_reply_to_account_id: Some(alice.id),
                ..status::NewStatus::local(&bob, "an answer")
            },
        )
        .await
        .expect("the fixture reply inserts");

        for who in [alice.id, bob.id, carol] {
            assert!(
                participates_in(&pool, who, root.conversation_id)
                    .await
                    .unwrap(),
                "the author, the replier and the mentioned account all \
                 belong to the thread"
            );
        }
        assert!(
            !participates_in(&pool, stranger, root.conversation_id)
                .await
                .unwrap(),
            "a stranger to the thread does not"
        );
        assert!(
            !participates_in(&pool, alice.id, Uuid::now_v7())
                .await
                .unwrap(),
            "an unknown thread has no participants"
        );

        // The reply rode the root's conversation, so muting through either
        // id names the same thread — the pair the participation rule reads.
        assert_eq!(reply.conversation_id, root.conversation_id);
    }

    #[tokio::test]
    async fn a_deleted_status_still_makes_its_author_a_participant() {
        let (_db, pool) = slot().await;
        let alice = user_row(&pool, "alice").await;
        let posted = status::insert(
            &pool,
            TEST_BASE_URL,
            &status::NewStatus::local(&alice, "one post, later deleted"),
        )
        .await
        .expect("the fixture post inserts");
        let _tombstone = status::soft_delete(&pool, posted.id)
            .await
            .expect("the fixture delete runs");

        assert!(
            participates_in(&pool, alice.id, posted.conversation_id)
                .await
                .unwrap(),
            "a tombstone does not undo the authorship it recorded"
        );
    }

    /// A fresh local account named `nickname` as its full row, for the
    /// status fixtures that build on it.
    ///
    /// # Panics
    ///
    /// If the insert fails, e.g. because the nickname is taken.
    async fn user_row(pool: &PgPool, nickname: &str) -> crate::models::user::User {
        insert(
            pool,
            &NewUser::local(nickname, &format!("https://test.example/users/{nickname}")),
        )
        .await
        .expect("user fixture inserts")
    }
}
