//! The message rows and their reads: the HTML a message stores, the paged
//! list of a chat's messages, the lookup by `ActivityPub` id and the
//! withdrawal that turns a message into a tombstone.

use sqlx::{Executor, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, pagination::PageParams, util::format::text_to_html};

use super::{ChatMessage, ChatMessageReference, references::REFERENCE_SELECT_SQL};

/// [`message_by_ap_id`]: the stored message under an `ActivityPub` object id,
/// tombstone included — the row a redelivered `Create` must not duplicate and
/// the one a federated `Delete` withdraws.
const MESSAGE_BY_AP_ID_SQL: &str = "SELECT * FROM chat_messages WHERE ap_id = $1";

/// [`delete_message_for`] when the caller wrote the message: a tombstone with
/// its text cleared and its `ap_id` reserved, exactly like
/// [`statuses::soft_delete`](crate::models::status::soft_delete). Every side's
/// reference survives and points at nothing live, which is what hides the
/// message from all of them.
///
/// [`delete_message_for`]: super::delete_message_for
const SOFT_DELETE_MESSAGE_SQL: &str = "UPDATE chat_messages
      SET content = '', raw_text = '', deleted_at = now()
    WHERE id = $1 AND deleted_at IS NULL";

/// The HTML a chat message stores for the source text `text`.
///
/// Chat content is escaped plain text with linkified URLs and nothing else:
/// [`text_to_html`] and never
/// [`render_markdown`](crate::util::format::render_markdown), matching
/// Pleroma's `CommonAPI.format_chat_content` (`lib/pleroma/web/common_api.ex`),
/// which runs `Formatter.html_escape` and `Formatter.linkify` over the text and
/// no markdown parser.
#[must_use]
pub fn render_chat_content(text: &str) -> String {
    text_to_html(text)
}

/// The messages of chat `chat_id` as its owner sees them, newest first, `page`'s
/// `limit` at a time, a withdrawn message left out: its reference may well still
/// be there, but its tombstone has no text to show.
///
/// Pleroma reads the same list from `MessageReference.for_chat_query`, ordered
/// `desc: :id` on the reference; a message's two references are written by the
/// same [`add_message`] call, so paging a chat by reference id is paging it by
/// message.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
///
/// [`add_message`]: super::add_message
pub async fn messages_page(
    pool: &PgPool,
    chat_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<ChatMessageReference>> {
    let mut builder = QueryBuilder::<Postgres>::new(REFERENCE_SELECT_SQL);
    let _live: &mut _ = builder.push("m.deleted_at IS NULL AND r.chat_id = ");
    let _bound: &mut _ = builder.push_bind(chat_id);
    page.apply_keyset(&mut builder, "r.id");
    let references = builder
        .build_query_as::<ChatMessageReference>()
        .fetch_all(pool)
        .await?;
    Ok(page.finish(references))
}

/// The stored message whose [`ap_id`](ChatMessage::ap_id) is `ap_id`,
/// tombstone included, `None` when this instance never imported the object.
///
/// This is the federation dedupe read: a live row means a second `Create` of
/// the same object restates a delivered message, and a tombstone means a
/// withdrawn one is never re-imported (its `ap_id` stays reserved, see the
/// column's note in the migration).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn message_by_ap_id(pool: &PgPool, ap_id: &str) -> AppResult<Option<ChatMessage>> {
    let message = sqlx::query_as::<_, ChatMessage>(MESSAGE_BY_AP_ID_SQL)
        .bind(ap_id)
        .fetch_optional(pool)
        .await?;
    Ok(message)
}

/// Withdraws message `message_id` — the sender's federated `Delete`: the row
/// becomes a tombstone exactly like [`delete_message_for`]'s sender arm does.
///
/// The answer says whether *this* call struck (`false` when the message is
/// withdrawn already, which keeps a redelivered `Delete` idempotent). Both
/// participants' references survive and read the absence through
/// [`messages_page`]'s live filter.
///
/// Runs on the caller's executor (a pool, or a transaction's connection), so
/// [`delete_message_for`] can strike inside its own transaction.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
///
/// [`delete_message_for`]: super::delete_message_for
pub async fn withdraw<'e, E>(executor: E, message_id: Uuid) -> AppResult<bool>
where
    E: Executor<'e, Database = Postgres>,
{
    let stamped = sqlx::query(SOFT_DELETE_MESSAGE_SQL)
        .bind(message_id)
        .execute(executor)
        .await?;
    Ok(stamped.rows_affected() > 0)
}

/// The `UPDATE` behind [`withdraw_all_from_sender`]: the tombstone of
/// [`SOFT_DELETE_MESSAGE_SQL`] for every live message one account sent.
const WITHDRAW_ALL_FROM_SENDER_SQL: &str = "UPDATE chat_messages
      SET content = '', raw_text = '', deleted_at = now()
    WHERE sender_id = $1 AND deleted_at IS NULL";

/// Turns every live message user `sender_id` sent into a tombstone, the chat
/// step of the account-deletion jobs.
///
/// The user row is purged or kept, never deleted, so no foreign key removes
/// the messages; without this the other participant keeps reading the text of
/// everything the departed account said. Withdrawn messages are left as they
/// are, which makes a rerun a no-op. Answers how many messages this call
/// struck.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn withdraw_all_from_sender(pool: &PgPool, sender_id: Uuid) -> AppResult<u64> {
    let stamped = sqlx::query(WITHDRAW_ALL_FROM_SENDER_SQL)
        .bind(sender_id)
        .execute(pool)
        .await?;
    Ok(stamped.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::chat::{
            NewChatMessage,
            test_support::{ALL, local, newest_by_id, page, reference_ids, send, send_with},
        },
        util::id::new_id,
    };

    #[tokio::test]
    async fn message_pages_walk_by_reference_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let mut sent = Vec::new();
        for index in 0..5 {
            let (message, reference, _) =
                send(&pool, &alice, &bob, &format!("message {index}")).await;
            sent.push((message, reference));
        }
        let chat_id = sent.first().expect("five messages").1.chat_id;
        let ids = newest_by_id(
            &sent
                .iter()
                .map(|(_, reference)| reference.id)
                .collect::<Vec<Uuid>>(),
        );
        let newest = ids.first().expect("five ids").to_string();
        let oldest = ids.last().expect("five ids").to_string();

        let first_page = messages_page(&pool, chat_id, page("limit=2"))
            .await
            .unwrap();
        assert_eq!(first_page.len(), 2, "the limit holds the page down");
        assert_eq!(
            reference_ids(&first_page),
            vec![ids[0], ids[1]],
            "the newest two references, newest first"
        );

        let second_page =
            messages_page(&pool, chat_id, page(&format!("limit=2&max_id={}", ids[1])))
                .await
                .unwrap();
        assert_eq!(
            reference_ids(&second_page),
            vec![ids[2], ids[3]],
            "max_id walks back through the chat"
        );

        let since = messages_page(&pool, chat_id, page(&format!("since_id={oldest}")))
            .await
            .unwrap();
        assert_eq!(
            reference_ids(&since),
            vec![ids[0], ids[1], ids[2], ids[3]],
            "since_id skips the cursor and everything older"
        );

        let previous = messages_page(&pool, chat_id, page(&format!("limit=2&min_id={}", ids[3])))
            .await
            .unwrap();
        assert_eq!(
            reference_ids(&previous),
            vec![ids[1], ids[2]],
            "min_id answers the page just after the cursor, newest first again"
        );

        assert!(
            messages_page(&pool, chat_id, page(&format!("max_id={oldest}")))
                .await
                .unwrap()
                .is_empty(),
            "nothing is older than the oldest reference"
        );
        assert!(
            messages_page(&pool, chat_id, page(&format!("min_id={newest}")))
                .await
                .unwrap()
                .is_empty(),
            "nothing is newer than the newest one"
        );
        assert!(
            messages_page(&pool, new_id(), page(ALL))
                .await
                .unwrap()
                .is_empty(),
            "a chat nobody has written in has no messages"
        );
    }

    #[test]
    fn chat_content_is_escaped_text_with_links() {
        assert_eq!(
            render_chat_content("a <b>tag</b> & an &amp;"),
            "<p>a &lt;b&gt;tag&lt;/b&gt; &amp; an &amp;amp;</p>",
            "markup is escaped, not kept"
        );
        assert_eq!(
            render_chat_content("see https://example.com/a?b=c now"),
            "<p>see <a href=\"https://example.com/a?b=c\" rel=\"ugc nofollow noopener noreferrer\">\
             example.com/a?b=c</a> now</p>",
            "bare urls are linkified, with the rel the sanitizer wants"
        );
        assert_eq!(
            render_chat_content("# heading\n\nsecond"),
            "<p># heading</p><p>second</p>",
            "markdown stays plain text"
        );
        assert_eq!(
            render_chat_content("one\ntwo"),
            "<p>one<br>two</p>",
            "a newline breaks the line"
        );
        assert_eq!(
            NewChatMessage::text("hi *there*").raw_text,
            "hi *there*",
            "the source text is kept verbatim"
        );
        assert_eq!(
            NewChatMessage::text("hi *there*").content,
            "<p>hi *there*</p>",
            "and the emphasis marker is not a tag"
        );
        assert_eq!(
            NewChatMessage::default().content,
            "",
            "a default new message carries nothing"
        );
        assert!(
            NewChatMessage::text("x").idempotency_key.is_none(),
            "and neither text() nor the default sets an idempotency key"
        );
        assert!(
            NewChatMessage::default().idempotency_key.is_none(),
            "on either construction"
        );
    }

    #[tokio::test]
    async fn message_by_ap_id_reads_the_live_row_and_its_tombstone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        assert!(
            message_by_ap_id(&pool, "https://remote.example/objects/missing")
                .await
                .unwrap()
                .is_none(),
            "an object this instance never imported is no message"
        );

        let new = NewChatMessage {
            ap_id: Some("https://remote.example/objects/twice".to_owned()),
            ..NewChatMessage::text("mirrored")
        };
        let (message, _, _) = send_with(&pool, &alice, &bob, &new).await;
        let found = message_by_ap_id(&pool, "https://remote.example/objects/twice")
            .await
            .unwrap()
            .expect("the mirrored row answers its ActivityPub id");
        assert_eq!(found.id, message.id, "the same row add_message wrote");
        assert!(found.deleted_at.is_none(), "and live so far");

        assert!(
            withdraw(&pool, message.id).await.unwrap(),
            "the withdrawal strikes a live row"
        );
        assert!(
            !withdraw(&pool, message.id).await.unwrap(),
            "and finds nothing to do the second time"
        );
        let tombstone = message_by_ap_id(&pool, "https://remote.example/objects/twice")
            .await
            .unwrap()
            .expect("the id stays reserved past the delete, so a redelivery cannot re-import");
        assert!(
            tombstone.deleted_at.is_some(),
            "the row survives as a tombstone"
        );
        assert!(
            tombstone.content.is_empty() && tombstone.raw_text.is_empty(),
            "with its text cleared"
        );
    }

    /// Reads whether message `id` is a tombstone and what text it still holds.
    async fn stored(pool: &PgPool, id: Uuid) -> (bool, String, String) {
        sqlx::query_as::<_, (bool, String, String)>(
            "SELECT deleted_at IS NOT NULL, content, raw_text FROM chat_messages WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the fixture message reads")
    }

    #[tokio::test]
    async fn withdrawing_all_from_a_sender_tombstones_only_their_messages() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let (first, _, bobs_view) = send(&pool, &alice, &bob, "first from alice").await;
        let (second, _, _) = send(&pool, &alice, &bob, "second from alice").await;
        let (reply, _, _) = send(&pool, &bob, &alice, "bob answers").await;
        let bobs_chat = bobs_view
            .expect("bob is local, so he has a reference")
            .chat_id;

        assert_eq!(
            withdraw_all_from_sender(&pool, alice.id).await.unwrap(),
            2,
            "both of alice's live messages are struck"
        );
        for id in [first.id, second.id] {
            assert_eq!(
                stored(&pool, id).await,
                (true, String::new(), String::new()),
                "alice's message {id} is a tombstone with its text cleared"
            );
        }
        let (tombstoned, content, raw_text) = stored(&pool, reply.id).await;
        assert!(!tombstoned, "bob's message is not alice's to withdraw");
        assert_eq!(content, "<p>bob answers</p>", "and keeps its content");
        assert_eq!(raw_text, "bob answers", "and its source text");
        let bobs_page = messages_page(&pool, bobs_chat, page(ALL)).await.unwrap();
        assert_eq!(
            bobs_page
                .iter()
                .map(|reference| reference.message_id)
                .collect::<Vec<Uuid>>(),
            vec![reply.id],
            "bob's page lists only his own message"
        );

        assert_eq!(
            withdraw_all_from_sender(&pool, alice.id).await.unwrap(),
            0,
            "a second call finds nothing live"
        );
        assert_eq!(
            withdraw_all_from_sender(&pool, new_id()).await.unwrap(),
            0,
            "a sender with no messages matches nothing"
        );
    }
}
