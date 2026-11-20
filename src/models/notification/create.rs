//! Telling a recipient something happened: the one raw insert
//! ([`create`]) computing its [`grouped_key`] on the way in,
//! [`maybe_notify`] — the insert that honours the recipient's
//! `block_from_strangers` setting — and [`already_notified`], the
//! three-part deduplication read [`crate::services::notify`] asks
//! before telling a recipient the same fact twice.

use sqlx::{Executor, PgPool, Postgres};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{follow, user, user::notification_settings::NotificationSettings},
    util::id::new_id,
};

use super::{NewNotification, Notification, TYPE_FOLLOW_REQUEST, grouped_key};

/// Inserts one notification — its [`grouped_key`] computed from the parts
/// of `new`, `None` for an ungroupable type — and returns the stored row.
///
/// The executor is the pool or the caller's open transaction, so a telling
/// that belongs with another write commits together with it — the federation
/// chat handler inserts the delivered message, its references and this
/// notification on one [`chat::add_message`](crate::models::chat::add_message)
/// transaction.
///
/// # Errors
///
/// Any database failure (connection, encoding, a foreign key naming no
/// user, status, report or chat message) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create<'e, E>(executor: E, new: &NewNotification<'_>) -> AppResult<Notification>
where
    E: Executor<'e, Database = Postgres>,
{
    let group_key = grouped_key(new.notification_type, new.status_id, new.emoji);
    let row = sqlx::query_as::<_, Notification>(
        "INSERT INTO notifications
             (id, user_id, from_account_id, type, status_id, emoji, report_id, chat_message_id,
              group_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING *",
    )
    .bind(new_id())
    .bind(new.user_id)
    .bind(new.from_account_id)
    .bind(new.notification_type)
    .bind(new.status_id)
    .bind(new.emoji)
    .bind(new.report_id)
    .bind(new.chat_message_id)
    .bind(group_key)
    .fetch_one(executor)
    .await?;
    Ok(row)
}

/// Creates the notification of `new` unless the recipient's
/// [`block_from_strangers`](NotificationSettings::block_from_strangers)
/// setting withholds it.
///
/// A recipient that set the flag is not notified by actors it does not
/// follow — no accepted follow of the actor, no notification — while
/// [`TYPE_FOLLOW_REQUEST`] is exempt and reaches the recipient whatever
/// the setting (Pleroma's `Notification.skip?` `:block_from_strangers`
/// arm, which the ticket keeps narrower: exempting the follow request
/// alone).
///
/// [`create`] does the insert when the notification is not withheld, and
/// the answer distinguishes the two outcomes: `Some` carries the stored
/// row, `None` says the setting skipped it — which is not an error, the
/// causing endpoint moves on. A recipient or actor with no row is left to
/// [`create`]'s foreign key, so this never hides a broken id behind a
/// silent skip.
///
/// # Errors
///
/// Any database failure (connection, encoding, a foreign key naming no
/// user or status) arrives as [`AppError::Database`](crate::error::AppError).
pub async fn maybe_notify(
    pool: &PgPool,
    new: &NewNotification<'_>,
) -> AppResult<Option<Notification>> {
    if withholds_stranger(pool, new).await? {
        return Ok(None);
    }
    create(pool, new).await.map(Some)
}

/// Whether the recipient's setting withholds `new`: `true` only when the
/// flag is set, the type is not the exempt follow request, and the
/// recipient has no accepted follow of the actor. An unknown recipient
/// never withholds (see [`maybe_notify`]).
async fn withholds_stranger(pool: &PgPool, new: &NewNotification<'_>) -> AppResult<bool> {
    if new.notification_type == TYPE_FOLLOW_REQUEST {
        return Ok(false);
    }
    let Some(recipient) = user::get_by_id(pool, new.user_id).await? else {
        return Ok(false);
    };
    if !NotificationSettings::for_user(&recipient).block_from_strangers {
        return Ok(false);
    }
    let followed = follow::has_accepted_follow(pool, recipient.id, new.from_account_id).await?;
    Ok(!followed)
}

/// Whether `user_id` already holds a live (undismissed) notification of
/// `notification_type` caused by `from_account_id`.
///
/// The deduplication read Pleroma's `recently_followed` skip is: a fact the
/// recipient was already told about is not told again, and the repeated
/// write that reaches here — a second `POST …/follow` of the same pair,
/// whose row [`follow::follow`] found already open — is what makes it
/// necessary. A dismissed row does not count: the recipient cleared that
/// telling, so a fresh event may tell again.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn already_notified(
    pool: &PgPool,
    user_id: Uuid,
    from_account_id: Uuid,
    notification_type: &str,
) -> AppResult<bool> {
    let (found,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM notifications \
          WHERE user_id = $1 AND from_account_id = $2 AND type = $3 \
            AND dismissed_at IS NULL)",
    )
    .bind(user_id)
    .bind(from_account_id)
    .bind(notification_type)
    .fetch_one(pool)
    .await?;
    Ok(found)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::models::notification::{
        TYPE_FAVOURITE, dismiss,
        test_support::{note, solo, trio},
    };

    #[tokio::test]
    async fn create_stores_the_computed_group_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let row = create(
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
        assert_eq!(
            row.group_key.as_deref(),
            grouped_key(TYPE_FAVOURITE, Some(status_id), None).as_deref(),
            "the stored key is the computed one"
        );
        assert!(
            row.dismissed_at.is_none() && !row.seen,
            "a fresh row is unread and undismissed"
        );
        assert!(row.inserted_at <= Utc::now(), "stamped by the database");

        let follow = create(
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
        assert_eq!(follow.group_key, None, "a follow stores no group key");
    }

    #[tokio::test]
    async fn already_notified_answers_only_live_tellings_of_one_actor_and_type() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, _) = trio(pool).await;
        let other = solo(pool, "dup-other").await;
        assert!(
            !already_notified(pool, recipient, actor, "follow")
                .await
                .expect("the read runs"),
            "a fresh recipient has been told nothing"
        );

        let row = note(pool, recipient, actor, "follow", None).await;
        assert!(
            already_notified(pool, recipient, actor, "follow")
                .await
                .expect("the read runs"),
            "the live telling counts"
        );
        assert!(
            !already_notified(pool, recipient, actor, "favourite")
                .await
                .expect("the read runs"),
            "another type was never told"
        );
        assert!(
            !already_notified(pool, recipient, other.id, "follow")
                .await
                .expect("the read runs"),
            "one actor's telling does not speak for another's"
        );

        let _dismissed = dismiss(pool, recipient, row.id)
            .await
            .expect("the dismissal runs");
        assert!(
            !already_notified(pool, recipient, actor, "follow")
                .await
                .expect("the repeat read runs"),
            "a dismissed row does not hold the next telling back"
        );
    }

    /// The `maybe_notify` fixtures' account pair: a recipient named
    /// `<tag>-recipient` and an actor named `<tag>-actor`.
    async fn pair(pool: &PgPool, tag: &str) -> (user::User, user::User) {
        use crate::{
            config::TEST_BASE_URL,
            models::user::{NewUser, insert, local_ap_id},
        };
        let recipient = insert(
            pool,
            &NewUser::local(
                &format!("{tag}-recipient"),
                &local_ap_id(TEST_BASE_URL, &format!("{tag}-recipient")),
            ),
        )
        .await
        .expect("recipient fixture");
        let actor = insert(
            pool,
            &NewUser::local(
                &format!("{tag}-actor"),
                &local_ap_id(TEST_BASE_URL, &format!("{tag}-actor")),
            ),
        )
        .await
        .expect("actor fixture");
        (recipient, actor)
    }

    /// A status-less `notification_type` notification of `recipient`
    /// caused by `actor`, for the `maybe_notify` fixtures.
    fn about<'a>(
        recipient: &'a user::User,
        actor: &'a user::User,
        notification_type: &'a str,
    ) -> NewNotification<'a> {
        NewNotification {
            user_id: recipient.id,
            from_account_id: actor.id,
            notification_type,
            status_id: None,
            emoji: None,
            report_id: None,
            chat_message_id: None,
        }
    }

    /// Turns on the recipient's `block_from_strangers` flag.
    async fn block_strangers(pool: &PgPool, recipient: &user::User) {
        let settings = NotificationSettings {
            block_from_strangers: true,
            hide_notification_contents: false,
        };
        user::notification_settings::save(pool, recipient.id, &settings)
            .await
            .expect("the flag is stored");
    }

    /// How many notification rows the slot holds.
    async fn row_count(pool: &PgPool) -> i64 {
        let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM notifications")
            .fetch_one(pool)
            .await
            .expect("the count runs");
        count
    }

    #[tokio::test]
    async fn maybe_notify_creates_when_the_recipient_does_not_block() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor) = pair(pool, "notify-open").await;
        let row = maybe_notify(pool, &about(&recipient, &actor, "mention"))
            .await
            .expect("the notification inserts")
            .expect("the default setting notifies");
        assert_eq!(row.user_id, recipient.id, "the recipient owns the row");
        assert_eq!(row.from_account_id, actor.id, "the actor is the cause");
    }

    #[tokio::test]
    async fn blocking_strangers_withholds_the_notification_and_inserts_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor) = pair(pool, "notify-blocked").await;
        block_strangers(pool, &recipient).await;

        let withheld = maybe_notify(pool, &about(&recipient, &actor, "favourite"))
            .await
            .expect("the read of the setting runs");
        assert!(withheld.is_none(), "a stranger causes no notification");
        assert_eq!(row_count(pool).await, 0, "and nothing was inserted");
    }

    #[tokio::test]
    async fn blocking_strangers_keeps_followed_actors() {
        use crate::config::TEST_BASE_URL;
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor) = pair(pool, "notify-followed").await;
        block_strangers(pool, &recipient).await;
        let relation = follow::follow(pool, TEST_BASE_URL, &recipient, &actor)
            .await
            .expect("the accepted follow inserts");
        assert_eq!(
            relation.state,
            follow::FollowState::Accepted,
            "the fixture follow is accepted, not pending"
        );

        let row = maybe_notify(pool, &about(&recipient, &actor, "mention"))
            .await
            .expect("the read runs")
            .expect("an actor the recipient follows notifies");
        assert_eq!(
            row.from_account_id, actor.id,
            "the followed actor caused it"
        );
    }

    #[tokio::test]
    async fn a_pending_follow_does_not_unblock_the_stranger() {
        use crate::{
            config::TEST_BASE_URL,
            models::user::{Flag, NewUser, insert, local_ap_id},
        };
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, _) = pair(pool, "notify-gated").await;
        // A locked actor makes the recipient's follow a pending request,
        // not an accepted relationship.
        let actor = insert(
            pool,
            &NewUser {
                locked: Flag::new(true),
                ..NewUser::local(
                    "notify-pending-actor",
                    &local_ap_id(TEST_BASE_URL, "notify-pending-actor"),
                )
            },
        )
        .await
        .expect("locked actor fixture");
        block_strangers(pool, &recipient).await;
        let created = follow::follow(pool, TEST_BASE_URL, &recipient, &actor)
            .await
            .expect("the requested follow inserts");
        assert_eq!(
            created.state,
            follow::FollowState::Pending,
            "the locked actor keeps the request pending"
        );

        assert!(
            maybe_notify(pool, &about(&recipient, &actor, "reblog"))
                .await
                .expect("the read runs")
                .is_none(),
            "a pending follow is not following: the actor stays a stranger"
        );
    }

    #[tokio::test]
    async fn follow_requests_are_exempt_from_the_block() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor) = pair(pool, "notify-request").await;
        block_strangers(pool, &recipient).await;

        let row = maybe_notify(pool, &about(&recipient, &actor, TYPE_FOLLOW_REQUEST))
            .await
            .expect("the notification inserts")
            .expect("the exempt type reaches the recipient");
        assert_eq!(
            row.event_type, TYPE_FOLLOW_REQUEST,
            "the stored row is the follow request"
        );
    }

    #[tokio::test]
    async fn an_unknown_recipient_reaches_the_foreign_key_not_a_skip() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (_, actor) = pair(pool, "notify-nohost").await;
        let orphan = NewNotification {
            user_id: Uuid::now_v7(),
            from_account_id: actor.id,
            notification_type: "mention",
            status_id: None,
            emoji: None,
            report_id: None,
            chat_message_id: None,
        };
        let err = maybe_notify(pool, &orphan)
            .await
            .expect_err("a recipient with no row is a database failure");
        assert!(
            matches!(err, crate::error::AppError::Database(_)),
            "the broken id surfaces as the error it is: {err}"
        );
    }
}
