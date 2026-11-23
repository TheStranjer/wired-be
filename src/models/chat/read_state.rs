//! What each participant has read of a chat: the unread count and the two
//! ways of marking references read.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

use super::{ChatMessageReference, references::reference_by_id};

/// [`unread_count`]: how many messages of one participant's chat they have still
/// to read (Pleroma's `MessageReference.unread_count_for_chat`). A withdrawn
/// message leaves its references behind as tombstones, but Pleroma deletes them,
/// so the join leaves every reference whose message is a tombstone out.
const UNREAD_COUNT_SQL: &str = "SELECT count(*) FROM chat_message_references r
      JOIN chat_messages m ON m.id = r.message_id
     WHERE r.chat_id = $1 AND r.unread AND m.deleted_at IS NULL";

/// [`mark_read`]: every unread message of one chat up to a point, Pleroma's
/// `set_all_seen_for_chat`. A `None` `last_read_id` arrives as SQL `NULL` in
/// `$2`, which the first half of the parenthesized condition turns into "no
/// marker, so the whole chat".
const MARK_READ_SQL: &str = "UPDATE chat_message_references
      SET unread = false
    WHERE chat_id = $1 AND unread AND ($2::uuid IS NULL OR id <= $2)";

/// [`mark_message_read`]: one message of one chat. The `chat_id` condition is
/// what keeps a route that names *the caller's* chat from marking a reference
/// that merely shares its id space: one whose chat is another's matches no
/// row here, exactly as Pleroma's `mark_message_as_read` `with` clause fails
/// its `^chat_id <- to_string(cm_ref.chat_id)` match. No `unread` condition,
/// so marking a read reference is the no-op update Pleroma's `mark_as_read`
/// makes of it, and the statement still reports the row it touched.
const MARK_MESSAGE_READ_SQL: &str = "UPDATE chat_message_references
      SET unread = false
    WHERE id = $1 AND chat_id = $2";

/// How many messages of chat `chat_id` its owner has not read yet, which is what
/// the `Chat` entity renders as `unread` (Pleroma's
/// `MessageReference.unread_count_for_chat`).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn unread_count(pool: &PgPool, chat_id: Uuid) -> AppResult<i64> {
    let row = sqlx::query_as::<_, (i64,)>(UNREAD_COUNT_SQL)
        .bind(chat_id)
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

/// Marks the messages of chat `chat_id` read, up to and including reference
/// `last_read_id`, and returns how many references went from unread to read.
///
/// `None` marks every unread message of the chat, as Pleroma's
/// `set_all_seen_for_chat` does when the client sends no marker. Already-read
/// references are not touched, so calling it again with the same marker reads
/// nothing new, and a message that arrives after the marker keeps its unread
/// flag.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mark_read(pool: &PgPool, chat_id: Uuid, last_read_id: Option<Uuid>) -> AppResult<u64> {
    let updated = sqlx::query(MARK_READ_SQL)
        .bind(chat_id)
        .bind(last_read_id)
        .execute(pool)
        .await?;
    Ok(updated.rows_affected())
}

/// Marks the message behind `ref_id` in chat `chat_id` read and returns it.
///
/// `None` when no reference has that id **in that chat** — the one statement
/// answering both the unknown id and the id of a reference living in somebody
/// else's chat.
///
/// Marking an already read reference is idempotent: it comes back unchanged, as
/// Pleroma's `MessageReference.mark_as_read` leaves it.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mark_message_read(
    pool: &PgPool,
    chat_id: Uuid,
    ref_id: Uuid,
) -> AppResult<Option<ChatMessageReference>> {
    let marked = sqlx::query(MARK_MESSAGE_READ_SQL)
        .bind(ref_id)
        .bind(chat_id)
        .execute(pool)
        .await?;
    if marked.rows_affected() == 0 {
        return Ok(None);
    }
    reference_by_id(pool, ref_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::chat::{
            test_support::{local, newest_by_id, send},
            withdraw,
        },
        util::id::new_id,
    };

    #[tokio::test]
    async fn marking_a_chat_read_walks_the_unread_flags() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let mut references = Vec::new();
        for index in 0..3 {
            let (_, _, recipient_ref) = send(&pool, &alice, &bob, &format!("ping {index}")).await;
            references.push(recipient_ref.expect("bob is local"));
        }
        let chat_id = references.first().expect("three messages").chat_id;
        let ids = newest_by_id(
            &references
                .iter()
                .map(|reference| reference.id)
                .collect::<Vec<Uuid>>(),
        );
        let second_oldest = ids[ids.len() - 2];

        assert_eq!(
            unread_count(&pool, chat_id).await.unwrap(),
            3,
            "three unread messages"
        );
        assert_eq!(
            mark_read(&pool, chat_id, Some(second_oldest))
                .await
                .unwrap(),
            2,
            "the two oldest messages are read off"
        );
        assert_eq!(
            unread_count(&pool, chat_id).await.unwrap(),
            1,
            "the newest one is left"
        );
        assert_eq!(
            mark_read(&pool, chat_id, Some(second_oldest))
                .await
                .unwrap(),
            0,
            "marking the same point again reads nothing"
        );
        assert_eq!(
            mark_read(&pool, chat_id, None).await.unwrap(),
            1,
            "with no marker the whole chat is read"
        );
        assert_eq!(
            unread_count(&pool, chat_id).await.unwrap(),
            0,
            "nothing is unread now"
        );

        let (_, _, late_ref) = send(&pool, &alice, &bob, "late").await;
        assert!(
            late_ref.expect("bob is local").unread,
            "a message after the marker is unread"
        );
        assert_eq!(
            mark_read(&pool, chat_id, Some(second_oldest))
                .await
                .unwrap(),
            0,
            "and the old marker does not reach it"
        );
        assert_eq!(
            unread_count(&pool, chat_id).await.unwrap(),
            1,
            "so it stays unread"
        );
        assert_eq!(
            mark_read(&pool, new_id(), None).await.unwrap(),
            0,
            "an unknown chat has nothing to mark"
        );
    }

    #[tokio::test]
    async fn marking_one_message_read_returns_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let (message, alice_ref, recipient_ref) = send(&pool, &alice, &bob, "read me").await;
        let unread = recipient_ref.expect("bob is local");

        assert!(
            mark_message_read(&pool, alice_ref.chat_id, unread.id)
                .await
                .unwrap()
                .is_none(),
            "a reference of bob's chat is nothing for alice's chat to mark"
        );
        assert!(
            reference_still_unread(&pool, unread.id).await,
            "and the refused mark left it unread"
        );

        let read = mark_message_read(&pool, unread.chat_id, unread.id)
            .await
            .unwrap()
            .expect("the reference exists in its own chat");

        assert_eq!(read.id, unread.id, "the same reference");
        assert!(!read.unread, "now read");
        assert_eq!(read.message_id, message.id, "still pointing at the message");
        assert_eq!(read.content, unread.content, "its text is as it was");
        assert_eq!(read.chat_id, unread.chat_id, "and its chat");
        assert_eq!(
            unread_count(&pool, read.chat_id).await.unwrap(),
            0,
            "so the chat counts nothing unread"
        );
        let again = mark_message_read(&pool, read.chat_id, read.id)
            .await
            .unwrap()
            .expect("marking it twice still finds it");
        assert_eq!(again, read, "and changes nothing");
        assert!(
            mark_message_read(&pool, unread.chat_id, new_id())
                .await
                .unwrap()
                .is_none(),
            "an unknown reference is nothing to mark"
        );
        assert!(
            mark_message_read(&pool, new_id(), unread.id)
                .await
                .unwrap()
                .is_none(),
            "and so is a known reference under an unknown chat"
        );
    }

    #[tokio::test]
    async fn a_withdrawn_message_is_not_counted_unread() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let (first, _, first_ref) = send(&pool, &alice, &bob, "one").await;
        let (second, _, _) = send(&pool, &alice, &bob, "two").await;
        let chat_id = first_ref.expect("bob is local").chat_id;

        assert_eq!(
            unread_count(&pool, chat_id).await.unwrap(),
            2,
            "both unread"
        );
        assert!(
            withdraw(&pool, first.id).await.unwrap(),
            "first is withdrawn"
        );
        assert_eq!(
            unread_count(&pool, chat_id).await.unwrap(),
            1,
            "the withdrawn message is not counted"
        );
        assert!(
            withdraw(&pool, second.id).await.unwrap(),
            "second is withdrawn"
        );
        assert_eq!(
            unread_count(&pool, chat_id).await.unwrap(),
            0,
            "withdrawing the last unread message leaves nothing unread"
        );
    }

    #[tokio::test]
    async fn marking_read_still_flips_tombstoned_references() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let (first, _, first_ref) = send(&pool, &alice, &bob, "one").await;
        let _live = send(&pool, &alice, &bob, "two").await;
        let chat_id = first_ref.expect("bob is local").chat_id;
        assert!(
            withdraw(&pool, first.id).await.unwrap(),
            "first is withdrawn"
        );

        assert_eq!(
            mark_read(&pool, chat_id, None).await.unwrap(),
            2,
            "the tombstoned reference is flipped along with the live one"
        );
        assert_eq!(
            unread_count(&pool, chat_id).await.unwrap(),
            0,
            "nothing unread"
        );
    }

    /// Whether reference `ref_id` still carries its unread flag.
    async fn reference_still_unread(pool: &PgPool, ref_id: Uuid) -> bool {
        let row: (bool,) =
            sqlx::query_as("SELECT unread FROM chat_message_references WHERE id = $1")
                .bind(ref_id)
                .fetch_one(pool)
                .await
                .unwrap();
        row.0
    }
}
