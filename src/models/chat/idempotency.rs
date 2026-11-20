//! The replay of an `Idempotency-Key` a chat message was created under:
//! [`sender_reference_for_key`].

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

use super::ChatMessageReference;

/// [`sender_reference_for_key`]: the replay of a key inside its window — the
/// sender's own reference to the newest live message they created under
/// `idempotency_key` within the hour. Pleroma caches that key→id mapping in a
/// TTL cache for `Pipeline.common_pipeline`'s `idempotency_key` option; here
/// the key rides on the message row and the window is its `inserted_at`. The
/// `chats` join is what picks the *sender's* side out of a message's two
/// references, and the tombstone plus time guards leave an expired or
/// withdrawn answer without a row, which reads as "no replay".
const IDEMPOTENT_REPLAY_SQL: &str = "SELECT r.id, r.chat_id, r.message_id, r.unread, r.inserted_at,
           m.sender_id, m.content, m.raw_text, m.attachment_media_id, m.ap_id,
           m.idempotency_key, m.inserted_at AS message_inserted_at
      FROM chat_message_references r
      JOIN chat_messages m ON m.id = r.message_id
      JOIN chats c ON c.id = r.chat_id
     WHERE c.user_id = $1 AND m.sender_id = $1 AND m.idempotency_key = $2
       AND m.deleted_at IS NULL AND m.inserted_at > now() - interval '1 hour'
     ORDER BY m.inserted_at DESC
     LIMIT 1";

/// The sender's own view of the message their `key` created, `None` when no
/// live message of `sender_id` carries that key within the replay window —
/// the one hour its lookup statement measures.
///
/// The answer is the reference [`add_message`] wrote for the sender — already
/// read, in the chat the message was posted to — so a replayed `POST` renders
/// exactly what the original one answered with. A key that is unknown, older
/// than the hour, or whose message the sender has since withdrawn all answer
/// `None`, and the caller posts as if the key were fresh.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
///
/// [`add_message`]: super::add_message
pub async fn sender_reference_for_key(
    pool: &PgPool,
    sender_id: Uuid,
    key: &str,
) -> AppResult<Option<ChatMessageReference>> {
    let reference = sqlx::query_as::<_, ChatMessageReference>(IDEMPOTENT_REPLAY_SQL)
        .bind(sender_id)
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(reference)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::chat::{
        NewChatMessage, delete_message_for,
        test_support::{local, send, send_with},
    };

    /// Moves message `id`'s `inserted_at` two hours into the past — outside
    /// the idempotency replay window, whatever the machine's clock speed.
    async fn expire_message(pool: &PgPool, id: Uuid) {
        sqlx::query(
            "UPDATE chat_messages SET inserted_at = now() - interval '2 hours' WHERE id = $1",
        )
        .bind(id)
        .execute(pool)
        .await
        .map(drop)
        .expect("the backdate runs");
    }

    #[tokio::test]
    async fn an_idempotency_key_rides_the_row_and_replays_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;

        let (message, alice_ref, bob_ref) = send_with(
            &pool,
            &alice,
            &bob,
            &NewChatMessage {
                idempotency_key: Some("key-1".to_owned()),
                ..NewChatMessage::text("once")
            },
        )
        .await;

        assert_eq!(
            message.idempotency_key.as_deref(),
            Some("key-1"),
            "the column keeps the key"
        );
        assert_eq!(
            alice_ref.idempotency_key.as_deref(),
            Some("key-1"),
            "the joined view carries it"
        );
        assert_eq!(
            bob_ref.expect("bob is local").idempotency_key.as_deref(),
            Some("key-1"),
            "on both sides"
        );

        let replay = sender_reference_for_key(&pool, alice.id, "key-1")
            .await
            .unwrap()
            .expect("the key replays");
        assert_eq!(replay.id, alice_ref.id, "as the sender's own reference");
        assert_eq!(replay.chat_id, alice_ref.chat_id, "in its own chat");
        assert!(!replay.unread, "which the sender has read");

        assert!(
            sender_reference_for_key(&pool, alice.id, "other-key")
                .await
                .unwrap()
                .is_none(),
            "an unknown key replays nothing"
        );
        assert!(
            sender_reference_for_key(&pool, bob.id, "key-1")
                .await
                .unwrap()
                .is_none(),
            "and never for another user, holder of the same key or not"
        );

        // A keyless message stores no key, and NULL never answers a lookup —
        // not even for the empty key a client might send.
        let (keyless, _, _) = send(&pool, &alice, &bob, "no key").await;
        assert!(keyless.idempotency_key.is_none(), "no key on the row");
        assert!(
            sender_reference_for_key(&pool, alice.id, "")
                .await
                .unwrap()
                .is_none(),
            "and the empty key matches nothing"
        );

        expire_message(&pool, message.id).await;
        assert!(
            sender_reference_for_key(&pool, alice.id, "key-1")
                .await
                .unwrap()
                .is_none(),
            "an hour out of the window replays nothing"
        );
    }

    #[tokio::test]
    async fn a_withdrawn_message_is_not_replayed_and_a_dropped_reference_goes_with_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let (message, alice_ref, _) = send_with(
            &pool,
            &alice,
            &bob,
            &NewChatMessage {
                idempotency_key: Some("gone".to_owned()),
                ..NewChatMessage::text("take it back")
            },
        )
        .await;

        assert!(
            sender_reference_for_key(&pool, alice.id, "gone")
                .await
                .unwrap()
                .is_some(),
            "the key starts out replayable"
        );
        assert!(
            delete_message_for(&pool, alice.id, alice_ref.id)
                .await
                .unwrap(),
            "alice withdraws the message"
        );
        assert!(
            sender_reference_for_key(&pool, alice.id, "gone")
                .await
                .unwrap()
                .is_none(),
            "gone is no longer a replayable key: tombstone and sender's \
             reference both went, so the next same-key post writes afresh"
        );
        assert_eq!(
            message.idempotency_key.as_deref(),
            Some("gone"),
            "the key itself stays on the row — it is the message that is gone"
        );
    }
}
