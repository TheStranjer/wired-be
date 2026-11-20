//! Removing notification rows for good: [`delete_for_status`] and
//! [`delete_for_chat_message`] retire the tellings of subjects that
//! became tombstones, [`delete_matching`] takes one three-part telling
//! back, and [`delete_all_for_user`] is the account-deletion step.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

/// Removes every notification about `status_id`.
///
/// A deleted status leaves its favourites', boosts', reactions' and
/// mentions' notifications nothing to render, and this is what takes them
/// with it (the status row it names is a tombstone, so the schema's
/// `ON DELETE CASCADE` never fires). Returns how many rows went.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn delete_for_status(pool: &PgPool, status_id: Uuid) -> AppResult<u64> {
    let outcome = sqlx::query("DELETE FROM notifications WHERE status_id = $1")
        .bind(status_id)
        .execute(pool)
        .await?;
    Ok(outcome.rows_affected())
}

/// Removes every notification about the chat message `chat_message_id`.
///
/// The cleanup the federated sender's `Delete` runs beside the message's
/// withdrawal ([`chat::withdraw`](crate::models::chat::withdraw)): the
/// `pleroma:chat_mention` telling of a withdrawn message is the one
/// notification whose whole subject just lost its text, and — like
/// [`delete_for_status`] — the row survives as a tombstone, so the schema's
/// `ON DELETE CASCADE` never fires for it either. Returns how many rows went.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn delete_for_chat_message(pool: &PgPool, chat_message_id: Uuid) -> AppResult<u64> {
    let outcome = sqlx::query("DELETE FROM notifications WHERE chat_message_id = $1")
        .bind(chat_message_id)
        .execute(pool)
        .await?;
    Ok(outcome.rows_affected())
}

/// Removes every notification naming exactly this recipient, actor, type
/// and status, returning how many rows went.
///
/// The withdrawal read and write [`crate::services::notify`] pairs with
/// [`maybe_notify`](super::maybe_notify)]: the undo of a favourite or a reblog takes back the
/// telling it caused, and the grant of a follow request takes back the
/// `follow_request` row the request told with. `status_id` matches by
/// `IS NOT DISTINCT FROM`, so `None` names the status-less rows — a
/// follow or a request — rather than everything. Dismissed rows go like
/// live ones: a withdrawal retires the telling whatever its state.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn delete_matching(
    pool: &PgPool,
    user_id: Uuid,
    from_account_id: Uuid,
    notification_type: &str,
    status_id: Option<Uuid>,
) -> AppResult<u64> {
    let outcome = sqlx::query(
        "DELETE FROM notifications \
          WHERE user_id = $1 AND from_account_id = $2 AND type = $3 \
            AND status_id IS NOT DISTINCT FROM $4",
    )
    .bind(user_id)
    .bind(from_account_id)
    .bind(notification_type)
    .bind(status_id)
    .execute(pool)
    .await?;
    Ok(outcome.rows_affected())
}

/// The `DELETE` behind [`delete_all_for_user`]: every notification naming
/// the account, recipient or actor.
const DELETE_ALL_FOR_USER_SQL: &str =
    "DELETE FROM notifications WHERE user_id = $1 OR from_account_id = $1";

/// Removes every notification that names user `user_id`: the notification
/// step of the [`users.delete`](crate::jobs::deletion::KIND_DELETE) job.
///
/// Both sides go — the account's own inbox and the entries the account
/// caused as actor — because both foreign keys point at the account
/// (Pleroma's user delete cascades them with the row): an entry whose
/// actor has just been purged says nothing anyone can still render.
///
/// Returns how many rows went.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_all_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<u64> {
    let deleted = sqlx::query(DELETE_ALL_FOR_USER_SQL)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::models::{
        notification::{
            NewNotification, TYPE_CHAT_MENTION, TYPE_FAVOURITE, TYPE_FOLLOW_REQUEST, create, get,
            test_support::{note, solo, trio},
        },
        user,
    };

    #[tokio::test]
    async fn deleting_a_status_takes_its_notifications() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let about = create(
            pool,
            &NewNotification {
                user_id: recipient,
                from_account_id: actor,
                notification_type: TYPE_FAVOURITE,
                status_id: Some(status_id),
                emoji: None,
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the notification inserts");
        let unrelated = create(
            pool,
            &NewNotification {
                user_id: recipient,
                from_account_id: actor,
                notification_type: "follow",
                status_id: None,
                emoji: None,
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the follow inserts");

        let gone = delete_for_status(pool, status_id)
            .await
            .expect("the cleanup runs");
        assert_eq!(gone, 1, "the one row about the status went");
        assert!(
            get(pool, recipient, about.id)
                .await
                .expect("the read runs")
                .is_none(),
            "and it reads as nothing"
        );
        assert!(
            get(pool, recipient, unrelated.id)
                .await
                .expect("the read runs")
                .is_some(),
            "a status-less notification survives"
        );
        assert_eq!(
            delete_for_status(pool, status_id)
                .await
                .expect("the repeat runs"),
            0,
            "cleaning twice changes nothing and is not an error"
        );
    }

    #[tokio::test]
    async fn delete_matching_takes_one_telling_and_leaves_the_rest() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let carol = solo(pool, "matching-carol").await;
        let favourite = note(pool, recipient, actor, "favourite", Some(status_id)).await;
        let foreign = note(pool, carol.id, actor, "favourite", Some(status_id)).await;
        let request = note(pool, recipient, actor, TYPE_FOLLOW_REQUEST, None).await;
        let request_with_status =
            note(pool, recipient, actor, TYPE_FOLLOW_REQUEST, Some(status_id)).await;

        let gone = delete_matching(pool, recipient, actor, "favourite", Some(status_id))
            .await
            .expect("the withdrawal runs");
        assert_eq!(gone, 1, "exactly the recipient's own telling went");
        assert!(
            get(pool, recipient, favourite.id)
                .await
                .expect("the read runs")
                .is_none(),
            "and it reads as nothing"
        );
        assert!(
            get(pool, carol.id, foreign.id)
                .await
                .expect("the read runs")
                .is_some(),
            "the same telling of another recipient stays"
        );
        assert!(
            get(pool, recipient, request.id)
                .await
                .expect("the read runs")
                .is_some(),
            "a status-less row is not this status's telling"
        );

        let went = delete_matching(pool, recipient, actor, TYPE_FOLLOW_REQUEST, None)
            .await
            .expect("the request withdrawal runs");
        assert_eq!(went, 1, "the `None` status matches the status-less row");
        assert!(
            get(pool, recipient, request_with_status.id)
                .await
                .expect("the read runs")
                .is_some(),
            "and the `None` matched NULL only, never a row that names a status"
        );
        assert_eq!(
            delete_matching(pool, recipient, actor, "favourite", Some(status_id))
                .await
                .expect("the repeat runs"),
            0,
            "withdrawing twice changes nothing and is not an error"
        );
    }

    /// A stored chat message from a local `chat-mention-sender` to a local
    /// `chat-mention-recipient`, beside its two accounts: the subject pair
    /// of the chat-mention fixtures.
    async fn chat_mention_subject(pool: &PgPool) -> (user::User, user::User, Uuid) {
        use crate::{
            config::TEST_BASE_URL,
            models::{
                chat::{self, NewChatMessage},
                user::{NewUser, insert, local_ap_id},
            },
        };
        let sender = insert(
            pool,
            &NewUser::local(
                "chat-mention-sender",
                &local_ap_id(TEST_BASE_URL, "chat-mention-sender"),
            ),
        )
        .await
        .expect("chat sender fixture");
        let recipient = insert(
            pool,
            &NewUser::local(
                "chat-mention-recipient",
                &local_ap_id(TEST_BASE_URL, "chat-mention-recipient"),
            ),
        )
        .await
        .expect("chat recipient fixture");
        let mut conn = pool.begin().await.expect("a transaction begins");
        let (message, _, _) = chat::add_message(
            &mut conn,
            TEST_BASE_URL,
            &sender,
            &recipient,
            &NewChatMessage::text("a delivered chat message"),
        )
        .await
        .expect("the chat fixture writes");
        conn.commit().await.expect("the fixture commits");
        (sender, recipient, message.id)
    }

    #[tokio::test]
    async fn a_chat_mention_names_its_message_and_stays_ungrouped() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (sender, recipient, message_id) = chat_mention_subject(pool).await;

        let row = create(
            pool,
            &NewNotification {
                user_id: recipient.id,
                from_account_id: sender.id,
                notification_type: TYPE_CHAT_MENTION,
                status_id: None,
                emoji: None,
                report_id: None,
                chat_message_id: Some(message_id),
            },
        )
        .await
        .expect("the chat mention inserts");
        assert_eq!(
            row.chat_message_id,
            Some(message_id),
            "the message rides on the row"
        );
        assert_eq!(row.status_id, None, "and the notification names no status");
        assert_eq!(row.group_key, None, "the type is not groupable");

        let gone = delete_for_chat_message(pool, message_id)
            .await
            .expect("the cleanup runs");
        assert_eq!(gone, 1, "the telling goes with the message");
        assert_eq!(
            delete_for_chat_message(pool, message_id)
                .await
                .expect("the rerun runs"),
            0,
            "and cleaning twice changes nothing, as with a deleted status"
        );
    }
}
