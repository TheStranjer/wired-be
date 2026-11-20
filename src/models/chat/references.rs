//! The `chat_message_references` rows: one side's view of a message, the
//! reads that name a reference, and the per-side delete.

use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

use super::{ChatMessage, ChatMessageReference, messages::withdraw_message};

/// The joined shape of a reference and its message: the reference's own
/// columns first — its `id` is what a client pages a chat by, and what the
/// entity reports as the message's id — then the message's, with `m.inserted_at`
/// aliased so the message's creation time survives next to the reference's.
///
/// The text stops at `WHERE`, where each caller pushes the literal condition it
/// needs and binds the ids it names: [`messages_page`] (`m.deleted_at IS NULL
/// AND r.chat_id = …`, then the keyset), [`reference_in_chat`]
/// (`r.chat_id = … AND r.message_id = …`), [`message_reference`]
/// (`r.id = … AND r.chat_id = …`) and [`reference_by_id`] (`r.id = …`).
/// Only ids are bound, so nothing supplied by a client ever reaches the text.
///
/// [`messages_page`]: super::messages_page
pub(super) const REFERENCE_SELECT_SQL: &str =
    "SELECT r.id, r.chat_id, r.message_id, r.unread, r.inserted_at,
           m.sender_id, m.content, m.raw_text, m.attachment_media_id, m.ap_id,
           m.idempotency_key, m.inserted_at AS message_inserted_at
      FROM chat_message_references r
      JOIN chat_messages m ON m.id = r.message_id
     WHERE ";

/// [`message_reference_for_user`]: one participant's view of a message named
/// by the message's own id rather than by the reference's — Pleroma's
/// `MessageReference.for_chat_and_object`, whose `chat` is read as "the
/// viewer's chat holding this message" instead of by id. The columns are
/// [`REFERENCE_SELECT_SQL`]'s, with the `chats` join added for the owner.
const USER_REFERENCE_SQL: &str = "SELECT r.id, r.chat_id, r.message_id, r.unread, r.inserted_at,
           m.sender_id, m.content, m.raw_text, m.attachment_media_id, m.ap_id,
           m.idempotency_key, m.inserted_at AS message_inserted_at
      FROM chat_message_references r
      JOIN chat_messages m ON m.id = r.message_id
      JOIN chats uc ON uc.id = r.chat_id
     WHERE uc.user_id = $1 AND r.message_id = $2";

/// One side's link between a chat and a message. `ON CONFLICT DO NOTHING` keeps
/// a redelivered message from being listed twice in the same chat; the row is
/// read back by the `(chat_id, message_id)` key either way, so a race answers
/// with the winner's reference.
const INSERT_REFERENCE_SQL: &str = "INSERT INTO chat_message_references (
     id, chat_id, message_id, unread
) VALUES ($1, $2, $3, $4)
ON CONFLICT (chat_id, message_id) DO NOTHING";

/// [`delete_message_for`]: who may see a reference and what it points at — the
/// chat's owner, the message's writer and the message's id — which is what
/// decides whether a message is hidden from one list or withdrawn from both.
const REFERENCE_OWNERSHIP_SQL: &str = "SELECT c.user_id, m.sender_id, m.id
      FROM chat_message_references r
      JOIN chats c ON c.id = r.chat_id
      JOIN chat_messages m ON m.id = r.message_id
     WHERE r.id = $1";

/// [`delete_message_for`]: the caller's own reference, and nobody else's.
const DELETE_REFERENCE_SQL: &str = "DELETE FROM chat_message_references WHERE id = $1";

/// The one reference `ref_id` of chat `chat_id`, `None` when no reference
/// has that id **in that chat**.
///
/// This is how a per-chat route tells a foreign chat's reference from the
/// caller's own: reference ids are drawn from one space (decision 4), so
/// one of somebody else's chat simply is not here — Pleroma's
/// `delete_message` `with` clause answers the same way.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn message_reference(
    pool: &PgPool,
    chat_id: Uuid,
    ref_id: Uuid,
) -> AppResult<Option<ChatMessageReference>> {
    let mut builder = QueryBuilder::<Postgres>::new(REFERENCE_SELECT_SQL);
    let _pair: &mut _ = builder
        .push("r.id = ")
        .push_bind(ref_id)
        .push(" AND r.chat_id = ")
        .push_bind(chat_id);
    let reference = builder
        .build_query_as::<ChatMessageReference>()
        .fetch_optional(pool)
        .await?;
    Ok(reference)
}

/// Hides one message from `user_id`, the owner of the reference `ref_id`, and
/// reports whether there was such a reference to hide.
///
/// Two different things happen, which is what makes a reference per participant
/// worth having:
///
/// - **taking a message out of one's own list** deletes only that side's
///   reference: the message stays live and the other participant keeps seeing
///   it. There is no way to delete somebody else's copy of a chat message;
/// - **the sender withdrawing their message** soft-deletes it, which hides it
///   from both listings at once, and deletes their own reference with it, since
///   they asked for it out of their list too.
///
/// An unknown `ref_id`, and one belonging to another participant's chat, are
/// both answered `false` and change nothing; so is a second call for a reference
/// the first one already removed.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_message_for(pool: &PgPool, user_id: Uuid, ref_id: Uuid) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    let Some(owner) = reference_owner(&mut tx, ref_id).await? else {
        return Ok(false);
    };
    if owner.for_user != user_id {
        return Ok(false);
    }
    remove_reference(&mut tx, ref_id).await?;
    if owner.sender_id == user_id {
        withdraw_message(&mut tx, owner.message_id).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// `user_id`'s own view of `message_id`: the reference their chat holds to the
/// message, `None` when the pair has no chat or the user took the message out
/// of their list.
///
/// The id the reference answers by is the *reader's*, which is why
/// [`crate::entities::notification`] expands a `pleroma:chat_mention` through
/// this read rather than through any id the notification itself stores —
/// Pleroma's `put_chat_message` asks the same `for_chat_and_object` question of
/// the reading user.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn message_reference_for_user(
    pool: &PgPool,
    user_id: Uuid,
    message_id: Uuid,
) -> AppResult<Option<ChatMessageReference>> {
    let reference = sqlx::query_as::<_, ChatMessageReference>(USER_REFERENCE_SQL)
        .bind(user_id)
        .bind(message_id)
        .fetch_optional(pool)
        .await?;
    Ok(reference)
}

/// Links `message` to the chat `chat_id` as unread (`false` for the side that
/// wrote it) and returns that side's view of the message.
///
/// The link is written idempotently and read back through the same join the
/// listing uses, so what [`add_message`] answers is exactly what a later
/// [`messages_page`] of the same chat answers.
///
/// [`add_message`]: super::add_message
/// [`messages_page`]: super::messages_page
pub(super) async fn reference_in_chat(
    conn: &mut PgConnection,
    chat_id: Uuid,
    message: &ChatMessage,
    unread: bool,
) -> AppResult<ChatMessageReference> {
    sqlx::query(INSERT_REFERENCE_SQL)
        .bind(new_id())
        .bind(chat_id)
        .bind(message.id)
        .bind(unread)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    let mut builder = QueryBuilder::<Postgres>::new(REFERENCE_SELECT_SQL);
    let _pair: &mut _ = builder
        .push("r.chat_id = ")
        .push_bind(chat_id)
        .push(" AND r.message_id = ")
        .push_bind(message.id);
    let reference = builder
        .build_query_as::<ChatMessageReference>()
        .fetch_one(&mut *conn)
        .await?;
    Ok(reference)
}

/// The one reference `ref_id` joined with its message.
pub(super) async fn reference_by_id(
    pool: &PgPool,
    ref_id: Uuid,
) -> AppResult<Option<ChatMessageReference>> {
    let mut builder = QueryBuilder::<Postgres>::new(REFERENCE_SELECT_SQL);
    let _id: &mut _ = builder.push("r.id = ").push_bind(ref_id);
    let reference = builder
        .build_query_as::<ChatMessageReference>()
        .fetch_optional(pool)
        .await?;
    Ok(reference)
}

/// Whose reference `ref_id` is, who wrote its message, and which message that
/// is; `None` when no reference has that id, or its message is gone.
async fn reference_owner(
    conn: &mut PgConnection,
    ref_id: Uuid,
) -> AppResult<Option<ReferenceOwner>> {
    let row = sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(REFERENCE_OWNERSHIP_SQL)
        .bind(ref_id)
        .fetch_optional(conn)
        .await?;
    Ok(row.map(|(for_user, sender_id, message_id)| ReferenceOwner {
        for_user,
        sender_id,
        message_id,
    }))
}

/// The three ids [`delete_message_for`] decides from, named.
#[derive(Debug)]
struct ReferenceOwner {
    /// The owner of the chat the reference belongs to.
    for_user: Uuid,
    /// Who wrote the message.
    sender_id: Uuid,
    /// What it points at.
    message_id: Uuid,
}

/// Deletes reference `ref_id`, which the caller has established belongs to the
/// user asking.
async fn remove_reference(conn: &mut PgConnection, ref_id: Uuid) -> AppResult<()> {
    sqlx::query(DELETE_REFERENCE_SQL)
        .bind(ref_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::chat::{
        get_owned,
        test_support::{count, local, message_ids, messages, send, stored_message},
        unread_count,
    };

    #[tokio::test]
    async fn removing_a_reference_hides_a_message_from_one_side_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let (message, alice_ref, recipient_ref) = send(&pool, &alice, &bob, "alice says").await;
        let bobs_ref = recipient_ref.expect("bob is local");

        assert!(
            !delete_message_for(&pool, alice.id, bobs_ref.id)
                .await
                .unwrap(),
            "alice has no reference of bob's to delete"
        );
        assert!(
            !delete_message_for(&pool, new_id(), alice_ref.id)
                .await
                .unwrap(),
            "nor has a stranger"
        );
        assert!(
            delete_message_for(&pool, bob.id, bobs_ref.id)
                .await
                .unwrap(),
            "bob's own reference goes"
        );

        assert!(
            messages(&pool, bobs_ref.chat_id).await.is_empty(),
            "bob's list no longer shows it"
        );
        assert_eq!(
            message_ids(&messages(&pool, alice_ref.chat_id).await),
            vec![message.id],
            "alice still sees the message she wrote"
        );
        assert_eq!(
            stored_message(&pool, message.id).await,
            Some(message.clone()),
            "and it was not withdrawn: hiding a copy is not deleting the message"
        );
        assert!(
            get_owned(&pool, bob.id, bobs_ref.chat_id)
                .await
                .unwrap()
                .is_some(),
            "bob keeps his chat row, empty of messages"
        );
        assert_eq!(
            unread_count(&pool, alice_ref.chat_id).await.unwrap(),
            0,
            "alice has nothing unread to begin with"
        );
        assert!(
            !delete_message_for(&pool, bob.id, bobs_ref.id)
                .await
                .unwrap(),
            "deleting the same reference again finds nothing"
        );
    }

    #[tokio::test]
    async fn a_sender_withdrawing_a_message_takes_it_from_everyone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let (message, alice_ref, recipient_ref) = send(&pool, &alice, &bob, "take it back").await;
        let bobs_ref = recipient_ref.expect("bob is local");

        assert!(
            delete_message_for(&pool, alice.id, alice_ref.id)
                .await
                .unwrap(),
            "alice withdraws her message"
        );

        let tombstone = stored_message(&pool, message.id)
            .await
            .expect("the row is still there");
        assert!(tombstone.deleted_at.is_some(), "it is a tombstone now");
        assert_eq!(tombstone.content, "", "its content is cleared");
        assert_eq!(tombstone.raw_text, "", "and its source text");
        assert_eq!(tombstone.sender_id, alice.id, "everything else survives");
        assert!(
            messages(&pool, bobs_ref.chat_id).await.is_empty(),
            "bob no longer sees it: {:?}",
            message_ids(&messages(&pool, bobs_ref.chat_id).await)
        );
        assert!(
            messages(&pool, alice_ref.chat_id).await.is_empty(),
            "nor does alice, whose own reference went with it"
        );
        assert_eq!(
            unread_count(&pool, bobs_ref.chat_id).await.unwrap(),
            1,
            "the dangling reference is counted until bob clears it"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM chat_message_references").await,
            1,
            "and it is the only reference left"
        );
        assert!(
            !delete_message_for(&pool, alice.id, alice_ref.id)
                .await
                .unwrap(),
            "withdrawing twice finds nothing to withdraw"
        );
    }

    #[tokio::test]
    async fn a_reference_is_only_its_own_chats() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let (message, alice_ref, bob_ref) = send(&pool, &alice, &bob, "pair").await;
        let bobs = bob_ref.expect("bob is local");

        let own = message_reference(&pool, alice_ref.chat_id, alice_ref.id)
            .await
            .unwrap()
            .expect("a chat holds its own reference");
        assert_eq!(own, alice_ref, "as the joined view it is");
        assert_eq!(own.message_id, message.id, "pointing at the message");
        assert!(
            message_reference(&pool, bobs.chat_id, alice_ref.id)
                .await
                .unwrap()
                .is_none(),
            "alice's reference is not in bob's chat"
        );
        assert!(
            message_reference(&pool, alice_ref.chat_id, bobs.id)
                .await
                .unwrap()
                .is_none(),
            "nor his in hers"
        );
        assert!(
            message_reference(&pool, new_id(), alice_ref.id)
                .await
                .unwrap()
                .is_none(),
            "and an unknown chat holds nothing"
        );
    }

    #[tokio::test]
    async fn message_reference_for_user_reads_each_sides_own_view() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;

        let (message, alice_ref, bob_ref) = send(&pool, &alice, &bob, "hi bob").await;
        let bob_ref = bob_ref.expect("bob is local, so his reference exists");

        let as_bob = message_reference_for_user(&pool, bob.id, message.id)
            .await
            .unwrap()
            .expect("bob's side holds the message");
        assert_eq!(as_bob.id, bob_ref.id, "the reader's own reference id");
        assert!(as_bob.unread, "which on his side is the unread one");

        let as_alice = message_reference_for_user(&pool, alice.id, message.id)
            .await
            .unwrap()
            .expect("alice's side holds it too");
        assert_eq!(
            as_alice.id, alice_ref.id,
            "the sender's reference, not the recipient's"
        );
        assert!(!as_alice.unread, "and already read");

        assert!(
            message_reference_for_user(&pool, carol.id, message.id)
                .await
                .unwrap()
                .is_none(),
            "a bystander has no view of somebody else's conversation"
        );
        assert!(
            message_reference_for_user(&pool, bob.id, Uuid::now_v7())
                .await
                .unwrap()
                .is_none(),
            "and an unknown message answers nothing"
        );
    }
}
