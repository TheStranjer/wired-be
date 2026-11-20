//! Sending a chat message: [`add_message`] writes the message together with
//! both sides' chat rows and references.

use sqlx::PgConnection;

use crate::{error::AppResult, models::user::User};

use super::{
    ChatMessage, ChatMessageReference, NewChatMessage,
    chats::{bump_or_create, local_recipient_chat},
    references::reference_in_chat,
    write::{check_length, write_message},
};

/// Writes one message from `sender` to `recipient` on `conn`, together with
/// both sides' chat rows and references.
///
/// The answer is the message, the sender's reference and — when the recipient
/// is a local account — the recipient's.
///
/// `conn` is the caller's transaction, on purpose: sending a chat message
/// means federating it too, so a caller that wants the write and its
/// delivery job to stand or fall together holds both on one transaction
/// (the `federator.publish` queue keeps the wire off the request either
/// way).
///
/// What it writes, in order:
///
/// - `sender`'s chat of `recipient`: created if the pair has none, bumped to the
///   front of its owner's list;
/// - `recipient`'s chat of `sender`, created and bumped the same way — **unless
///   `recipient` is remote**, whose chat rows live on their own instance; then
///   nothing at all is stored for them and the third half of the answer is
///   `None`;
/// - the [`ChatMessage`] row, from `new_message` — its `ActivityPub` object
///   id is the one the message arrived with, or `{base_url}/objects/{uuid}`
///   of the minted row id when `new_message` carries none (decision 6);
/// - the sender's reference, already read: you have read what you wrote;
/// - the recipient's reference, unread.
///
/// # Errors
///
/// [`AppError::Unprocessable`] when the message's source text is longer than
/// [`MAX_CHAT_MESSAGE_CHARS`] characters, before anything is written; any
/// database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError) — an unknown sender or
/// recipient is the foreign-key violation on `users(id)`, an attachment that is
/// not a media row the one on `media_attachments(id)`, and a reused `ap_id` the
/// unique violation on `chat_messages.ap_id`.
///
/// [`AppError::Unprocessable`]: crate::error::AppError::Unprocessable
/// [`MAX_CHAT_MESSAGE_CHARS`]: super::MAX_CHAT_MESSAGE_CHARS
pub async fn add_message(
    conn: &mut PgConnection,
    base_url: &str,
    sender: &User,
    recipient: &User,
    new_message: &NewChatMessage,
) -> AppResult<(
    ChatMessage,
    ChatMessageReference,
    Option<ChatMessageReference>,
)> {
    check_length(&new_message.raw_text)?;
    let sender_chat = bump_or_create(conn, sender.id, recipient.id).await?;
    let recipient_chat = local_recipient_chat(conn, sender, recipient).await?;
    let message = write_message(conn, base_url, sender.id, new_message).await?;
    let sender_reference = reference_in_chat(conn, sender_chat.id, &message, false).await?;
    let recipient_reference = match recipient_chat {
        Some(chat) => Some(reference_in_chat(conn, chat.id, &message, true).await?),
        None => None,
    };
    Ok((message, sender_reference, recipient_reference))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::chat::{
            MAX_CHAT_MESSAGE_CHARS,
            test_support::{
                backdate_chat, chat_ids, chats, count, local, message_ids, messages, remote, send,
                stored_chat,
            },
            unread_count,
        },
    };

    #[tokio::test]
    async fn add_message_writes_both_sides_with_their_read_flags() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;

        let (message, alice_ref, bob_ref) = send(&pool, &alice, &bob, "hi bob").await;
        let bob_ref = bob_ref.expect("bob is local, so his reference exists");

        assert!(message.id.get_version_num() == 7, "the id is a UUIDv7");
        assert_eq!(message.sender_id, alice.id, "alice wrote it");
        assert_eq!(message.raw_text, "hi bob", "the source text is stored");
        assert_eq!(message.content, "<p>hi bob</p>", "and the rendered HTML");
        assert!(message.attachment_media_id.is_none(), "no attachment");
        assert_eq!(
            message.ap_id.as_deref(),
            Some(format!("{TEST_BASE_URL}/objects/{}", message.id).as_str()),
            "a message posted here owns this instance's object id"
        );
        assert!(message.deleted_at.is_none(), "the message is live");

        let alice_chat = stored_chat(&pool, alice_ref.chat_id).await;
        let bob_chat = stored_chat(&pool, bob_ref.chat_id).await;
        assert_ne!(alice_chat.id, bob_chat.id, "each side got its own chat row");
        assert_eq!(alice_chat.user_id, alice.id, "one belongs to alice");
        assert_eq!(alice_chat.recipient_id, bob.id, "about bob");
        assert_eq!(bob_chat.user_id, bob.id, "the other to bob");
        assert_eq!(bob_chat.recipient_id, alice.id, "about alice");

        assert_eq!(
            alice_ref.message_id, message.id,
            "alice's reference points at the message"
        );
        assert_eq!(bob_ref.message_id, message.id, "and so does bob's");
        assert_eq!(alice_ref.sender_id, alice.id, "the join carries the writer");
        assert!(!alice_ref.unread, "alice has read what she wrote");
        assert!(bob_ref.unread, "bob has not read it yet");
        assert!(
            message.idempotency_key.is_none(),
            "a plain send carries no idempotency key"
        );
        assert_eq!(alice_ref.content, message.content, "and the text");
        assert_eq!(
            alice_ref.message_inserted_at, message.inserted_at,
            "and the message's creation time"
        );

        assert_eq!(
            unread_count(&pool, alice_ref.chat_id).await.unwrap(),
            0,
            "alice's chat has nothing to read"
        );
        assert_eq!(
            unread_count(&pool, bob_ref.chat_id).await.unwrap(),
            1,
            "bob's has one message"
        );
        assert_eq!(
            message_ids(&messages(&pool, alice_ref.chat_id).await),
            vec![message.id],
            "alice lists the message"
        );
        assert_eq!(
            message_ids(&messages(&pool, bob_ref.chat_id).await),
            vec![message.id],
            "and so does bob"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM chat_message_references").await,
            2,
            "exactly one reference per side"
        );
    }

    #[tokio::test]
    async fn a_message_bumps_both_sides_of_the_conversation() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let (first, alice_ref, bob_ref) = send(&pool, &alice, &bob, "first").await;
        let (_, bobs_other_chat, _) = send(&pool, &bob, &carol, "elsewhere").await;
        let stale_alice_chat = backdate_chat(&pool, alice_ref.chat_id).await;
        let bobs_chat = bob_ref.expect("bob is local");
        let stale_bob_chat = backdate_chat(&pool, bobs_chat.chat_id).await;

        let (second, new_alice_ref, new_bob_ref) = send(&pool, &alice, &bob, "second").await;
        let new_bob_ref = new_bob_ref.expect("bob's side of the new message");

        assert_eq!(
            new_alice_ref.chat_id, stale_alice_chat.id,
            "alice's chat is the same row, not a new one"
        );
        assert_eq!(new_bob_ref.chat_id, stale_bob_chat.id, "and so is bob's");
        assert_eq!(
            new_alice_ref.message_id, second.id,
            "the new message is linked to both sides"
        );
        assert_eq!(new_bob_ref.message_id, second.id, "under one message id");
        assert!(
            stored_chat(&pool, alice_ref.chat_id).await.updated_at > stale_alice_chat.updated_at,
            "alice's chat moved to now"
        );
        assert!(
            stored_chat(&pool, bobs_chat.chat_id).await.updated_at > stale_bob_chat.updated_at,
            "bob's moved too, though he wrote nothing"
        );
        assert_eq!(
            chat_ids(&chats(&pool, alice.id).await),
            vec![alice_ref.chat_id],
            "alice lists one chat"
        );
        assert_eq!(
            chat_ids(&chats(&pool, bob.id).await),
            vec![bobs_chat.chat_id, bobs_other_chat.chat_id],
            "the conversation with news jumps ahead of bob's other chat"
        );
        assert_eq!(
            message_ids(&messages(&pool, alice_ref.chat_id).await),
            vec![second.id, first.id],
            "and the chat lists both messages, newest first"
        );
    }

    #[tokio::test]
    async fn a_remote_recipient_gets_no_chat_and_no_reference() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let mallory = remote(&pool, "mallory").await;

        let (message, alice_ref, mallory_ref) = send(&pool, &alice, &mallory, "hello there").await;

        assert!(
            mallory_ref.is_none(),
            "nothing is stored for a remote account"
        );
        assert_eq!(
            alice_ref.message_id, message.id,
            "alice's reference is hers"
        );
        assert!(!alice_ref.unread, "and she has read what she sent");
        let alice_chat = stored_chat(&pool, alice_ref.chat_id).await;
        assert_eq!(
            alice_chat.recipient_id, mallory.id,
            "in the one chat row this call wrote, about the remote account"
        );
        assert_eq!(
            unread_count(&pool, alice_ref.chat_id).await.unwrap(),
            0,
            "her chat has nothing unread"
        );
        assert_eq!(
            message_ids(&messages(&pool, alice_ref.chat_id).await),
            vec![message.id],
            "and it shows her own message"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM chats").await,
            1,
            "one chat row for the pair, not two"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM chat_message_references").await,
            1,
            "and one reference"
        );
        assert!(
            chats(&pool, mallory.id).await.is_empty(),
            "the remote account lists nothing here"
        );
    }

    #[tokio::test]
    async fn an_overlong_message_is_refused_before_anything_is_written() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let too_long = "x".repeat(MAX_CHAT_MESSAGE_CHARS + 1);
        let mut conn = pool.begin().await.unwrap();

        let err = add_message(
            &mut conn,
            TEST_BASE_URL,
            &alice,
            &bob,
            &NewChatMessage::text(&too_long),
        )
        .await
        .expect_err("one character over the limit is refused");

        assert_eq!(
            err.to_string(),
            format!(
                "Chat messages are limited to {MAX_CHAT_MESSAGE_CHARS} characters (got {})",
                MAX_CHAT_MESSAGE_CHARS + 1
            ),
            "the refusal names the limit and what it saw"
        );
        drop(conn);
        assert!(
            chats(&pool, alice.id).await.is_empty(),
            "the refusal created no chat"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM chat_messages").await,
            0,
            "and no message"
        );

        // The limit itself is allowed, and it counts characters, not bytes:
        // five thousand of the three-byte euro sign fit.
        let exactly_limit = "€".repeat(MAX_CHAT_MESSAGE_CHARS);
        let (message, _, _) = send(&pool, &alice, &bob, &exactly_limit).await;
        assert_eq!(
            message.raw_text.chars().count(),
            MAX_CHAT_MESSAGE_CHARS,
            "so {MAX_CHAT_MESSAGE_CHARS} characters go through"
        );
    }
}
