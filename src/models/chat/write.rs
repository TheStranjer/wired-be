//! The write of one `chat_messages` row: the length check in front of it and
//! the `INSERT` itself, which [`add_message`](super::add_message) calls.

use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    util::id::new_id,
};

use super::{ChatMessage, MAX_CHAT_MESSAGE_CHARS, NewChatMessage};

/// The `INSERT` behind [`add_message`]: the id this module mints (decision 4)
/// and what the caller decided. `inserted_at` is the caller's when a mirrored
/// remote message arrived with a `published` time, and the column default
/// otherwise — `COALESCE` is both of those in one statement. `RETURNING *`
/// brings the row back with the timestamps the database stamped.
///
/// [`add_message`]: super::add_message
const INSERT_MESSAGE_SQL: &str = "INSERT INTO chat_messages (
     id, sender_id, content, raw_text, attachment_media_id, ap_id, idempotency_key, inserted_at
) VALUES ($1, $2, $3, $4, $5, $6, $7, COALESCE($8, now())) RETURNING *";

/// Refuses a message whose source text is longer than
/// [`MAX_CHAT_MESSAGE_CHARS`] characters, counted the way a client counts them:
/// Unicode scalar values, not bytes.
pub(super) fn check_length(raw_text: &str) -> AppResult<()> {
    let length = raw_text.chars().count();
    if length > MAX_CHAT_MESSAGE_CHARS {
        return Err(AppError::Unprocessable(format!(
            "Chat messages are limited to {MAX_CHAT_MESSAGE_CHARS} characters (got {length})"
        )));
    }
    Ok(())
}

/// [`INSERT_MESSAGE_SQL`]'s write: the id this module mints and the object id
/// that names the row — the one the mirrored message arrived with, or the
/// instance's own `{base_url}/objects/{id}` minted beside the row id when it
/// carried none.
pub(super) async fn write_message(
    conn: &mut PgConnection,
    base_url: &str,
    sender_id: Uuid,
    new_message: &NewChatMessage,
) -> AppResult<ChatMessage> {
    let id = new_id();
    let ap_id = new_message
        .ap_id
        .clone()
        .unwrap_or_else(|| format!("{base_url}/objects/{id}"));
    let message = sqlx::query_as::<_, ChatMessage>(INSERT_MESSAGE_SQL)
        .bind(id)
        .bind(sender_id)
        .bind(new_message.content.as_str())
        .bind(new_message.raw_text.as_str())
        .bind(new_message.attachment_media_id)
        .bind(&ap_id)
        .bind(new_message.idempotency_key.as_deref())
        .bind(new_message.inserted_at)
        .fetch_one(conn)
        .await?;
    Ok(message)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::chat::{
            add_message,
            test_support::{
                insert_attachment, local, message_ids, messages, send, send_with, sqlstate_of,
                stored_message, with_extras,
            },
        },
    };

    #[tokio::test]
    async fn a_message_can_carry_an_attachment_and_an_ap_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let attachment = insert_attachment(&pool, &alice).await;
        let ap_id = format!("{TEST_BASE_URL}/objects/{}", new_id());

        let (message, alice_ref, bob_ref) = {
            let mut conn = pool.begin().await.unwrap();
            let written = add_message(
                &mut conn,
                TEST_BASE_URL,
                &alice,
                &bob,
                &with_extras("look at this", attachment, &ap_id),
            )
            .await
            .expect("a message with an attachment and an id is storable");
            conn.commit().await.unwrap();
            written
        };

        assert_eq!(
            message.attachment_media_id,
            Some(attachment),
            "the attachment is remembered"
        );
        assert_eq!(message.ap_id.as_deref(), Some(ap_id.as_str()), "and the id");
        assert_eq!(message.raw_text, "look at this", "with its text");
        assert_eq!(
            alice_ref.attachment_media_id,
            Some(attachment),
            "the joined view carries the attachment too"
        );
        assert_eq!(
            bob_ref.expect("bob is local").ap_id.as_deref(),
            Some(ap_id.as_str()),
            "and the id, on both sides"
        );
        assert!(
            alice_ref.raw_text.starts_with("look"),
            "the reference row reports the message, not the reference: {}",
            alice_ref.raw_text
        );
    }

    #[tokio::test]
    async fn an_empty_message_is_storable() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let mut conn = pool.begin().await.unwrap();

        let (message, alice_ref, _) = add_message(
            &mut conn,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewChatMessage {
                attachment_media_id: Some(insert_attachment(&pool, &alice).await),
                ..NewChatMessage::default()
            },
        )
        .await
        .expect("the column defaults let a message carry only an attachment");
        conn.commit().await.unwrap();

        assert_eq!(message.content, "", "empty content stays empty");
        assert_eq!(message.raw_text, "", "and so does the source text");
        assert_eq!(alice_ref.content, "", "which the listing shows as it is");
    }

    #[tokio::test]
    async fn a_reused_message_id_is_refused() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let ap_id = format!("{TEST_BASE_URL}/objects/{}", new_id());
        let attachment = insert_attachment(&pool, &alice).await;

        // A reused message id is how a redelivered remote message would come
        // back twice, and only the unique column can stop it.
        let mut first = pool.begin().await.unwrap();
        let stored = add_message(
            &mut first,
            TEST_BASE_URL,
            &alice,
            &bob,
            &with_extras("first use", attachment, &ap_id),
        )
        .await
        .expect("the first use of a message id is stored");
        first.commit().await.unwrap();
        assert_eq!(
            stored.0.ap_id.as_deref(),
            Some(ap_id.as_str()),
            "the fixture message carries the id"
        );

        let mut retry = pool.begin().await.unwrap();
        let duplicate = add_message(
            &mut retry,
            TEST_BASE_URL,
            &alice,
            &bob,
            &with_extras("again", attachment, &ap_id),
        )
        .await
        .expect_err("one message id is one message");
        drop(retry);
        assert_eq!(
            sqlstate_of(duplicate),
            "23505",
            "the refusal is the unique chat_messages.ap_id"
        );
    }

    #[tokio::test]
    async fn media_that_is_not_a_row_cannot_be_attached() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;

        let mut conn = pool.begin().await.unwrap();
        let foreign = add_message(
            &mut conn,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewChatMessage {
                attachment_media_id: Some(new_id()),
                ..NewChatMessage::text("attach this")
            },
        )
        .await
        .expect_err("media that does not exist cannot be attached");
        drop(conn);
        assert_eq!(
            sqlstate_of(foreign),
            "23503",
            "the refusal is the foreign key on media_attachments"
        );
    }

    #[tokio::test]
    async fn deleting_an_attachment_leaves_the_message_standing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let attachment = insert_attachment(&pool, &alice).await;
        let (message, alice_ref, _) = {
            let mut conn = pool.begin().await.unwrap();
            let written = add_message(
                &mut conn,
                TEST_BASE_URL,
                &alice,
                &bob,
                &with_extras("look at this", attachment, "https://remote.example/cm/1"),
            )
            .await
            .expect("the message stores with its attachment");
            conn.commit().await.unwrap();
            written
        };
        assert_eq!(
            alice_ref.attachment_media_id,
            Some(attachment),
            "the listing starts out with the attachment"
        );

        sqlx::query("DELETE FROM media_attachments WHERE id = $1")
            .bind(attachment)
            .execute(&pool)
            .await
            .map(drop)
            .expect("the attachment is deleted");

        let orphaned = stored_message(&pool, message.id)
            .await
            .expect("the message survives its attachment");
        assert!(
            orphaned.attachment_media_id.is_none(),
            "and is left without it"
        );
        assert_eq!(orphaned.raw_text, "look at this", "its text is intact");
        let listed = messages(&pool, alice_ref.chat_id).await;
        assert_eq!(
            message_ids(&listed),
            vec![message.id],
            "the message is still in the listing"
        );
        assert!(
            listed
                .first()
                .expect("one message is listed")
                .attachment_media_id
                .is_none(),
            "only its attachment is gone"
        );
    }

    #[tokio::test]
    async fn a_mirrored_message_keeps_the_published_time_it_arrived_with() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let published = DateTime::parse_from_rfc3339("2024-05-01T10:00:00Z")
            .expect("the fixture time")
            .with_timezone(&Utc);
        let new = NewChatMessage {
            ap_id: Some("https://remote.example/objects/old-chat".to_owned()),
            inserted_at: Some(published),
            ..NewChatMessage::text("an old thread surfaces")
        };

        let (mirrored, _, _) = send_with(&pool, &alice, &bob, &new).await;
        assert_eq!(
            mirrored.inserted_at, published,
            "the remote `published` timestamp is the creation time"
        );

        let (fresh, _, _) = send(&pool, &alice, &bob, "brand new").await;
        assert!(
            fresh.inserted_at <= Utc::now(),
            "a message that states no time is stamped by the database"
        );
    }
}
