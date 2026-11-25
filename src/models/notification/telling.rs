//! The full five-part key of one telling: [`Telling`], the live-row
//! probe ([`already_told`]) and its withdrawal twin
//! ([`delete_telling`]) — the one dedupe query and the one withdrawal
//! query of the notification model, naming the status and the reaction
//! emoji alongside the recipient, actor and type.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

/// The full key of one telling: who was told, who caused it, of what, and
/// about which status and emoji.
///
/// [`already_told`] and [`delete_telling`] take this: the status keeps two
/// ballots of one voter apart, and the emoji keeps two reactions to one
/// status apart. A `None` field matches by `IS NOT DISTINCT FROM`, so it
/// names exactly the rows that store none — a follow is
/// `status_id: None, emoji: None`.
#[derive(Clone, Debug)]
pub struct Telling<'a> {
    /// The recipient account.
    pub user_id: Uuid,
    /// The account that caused the notification.
    pub from_account_id: Uuid,
    /// Mastodon's type name.
    pub notification_type: &'a str,
    /// The status the telling names; `None` for the status-less rows.
    pub status_id: Option<Uuid>,
    /// The reaction emoji; `None` for every type but a reaction.
    pub emoji: Option<&'a str>,
}

/// The live-row probe behind [`already_told`]: whether the recipient holds
/// an undismissed notification under this key.
const ALREADY_TOLD_SQL: &str = "SELECT EXISTS (SELECT 1 FROM notifications \
          WHERE user_id = $1 AND from_account_id = $2 AND type = $3 \
            AND status_id IS NOT DISTINCT FROM $4 \
            AND emoji IS NOT DISTINCT FROM $5 \
            AND dismissed_at IS NULL)";

/// Whether `telling` is already told: a live (undismissed) row matches
/// every one of its five parts.
///
/// The deduplication the repeat-prone tellings owe — the poll closing a
/// retried sweep could re-fire, the reaction a client may `PUT` again on an
/// idempotent write, the second `POST …/follow` of an open pair (Pleroma's
/// `recently_followed` skip) — on the full key, so a second reaction with
/// another emoji is a fresh fact and not a duplicate. A dismissed row does
/// not count: the recipient cleared that telling, so a fresh event may
/// tell again.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn already_told(pool: &PgPool, telling: &Telling<'_>) -> AppResult<bool> {
    let (found,): (bool,) = sqlx::query_as(ALREADY_TOLD_SQL)
        .bind(telling.user_id)
        .bind(telling.from_account_id)
        .bind(telling.notification_type)
        .bind(telling.status_id)
        .bind(telling.emoji)
        .fetch_one(pool)
        .await?;
    Ok(found)
}

/// Removes every notification matching every part of `telling`, returning
/// how many rows went.
///
/// The withdrawal of a telling: the undo of a favourite or a reblog takes
/// back the telling it caused, the grant of a follow request takes back
/// the `follow_request` row, and retiring one reaction's notification
/// leaves another emoji's on the same status alone. `None` parts name the
/// rows that store none — never everything. Dismissed rows go like live
/// ones: a withdrawal retires the telling whatever its state.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn delete_telling(pool: &PgPool, telling: &Telling<'_>) -> AppResult<u64> {
    let outcome = sqlx::query(
        "DELETE FROM notifications \
          WHERE user_id = $1 AND from_account_id = $2 AND type = $3 \
            AND status_id IS NOT DISTINCT FROM $4 \
            AND emoji IS NOT DISTINCT FROM $5",
    )
    .bind(telling.user_id)
    .bind(telling.from_account_id)
    .bind(telling.notification_type)
    .bind(telling.status_id)
    .bind(telling.emoji)
    .execute(pool)
    .await?;
    Ok(outcome.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::models::notification::{
        NewNotification, Notification, TYPE_EMOJI_REACTION, TYPE_FAVOURITE, TYPE_FOLLOW,
        TYPE_FOLLOW_REQUEST, TYPE_POLL, TYPE_STATUS, create, dismiss, get,
        test_support::{note, solo, trio},
    };

    /// A [`Telling`] over the full key.
    fn telling<'a>(
        recipient: Uuid,
        actor: Uuid,
        event_type: &'a str,
        status_id: Option<Uuid>,
        emoji: Option<&'a str>,
    ) -> Telling<'a> {
        Telling {
            user_id: recipient,
            from_account_id: actor,
            notification_type: event_type,
            status_id,
            emoji,
        }
    }

    /// A reaction notification carrying `emoji`.
    async fn reaction_note(
        pool: &PgPool,
        recipient: Uuid,
        actor: Uuid,
        status_id: Uuid,
        emoji: &str,
    ) -> Notification {
        create(
            pool,
            &NewNotification {
                user_id: recipient,
                from_account_id: actor,
                notification_type: TYPE_EMOJI_REACTION,
                status_id: Some(status_id),
                emoji: Some(emoji),
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the reaction fixture inserts")
    }

    #[tokio::test]
    async fn already_told_reads_the_full_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        // A different status's key: the probe only reads, so the id needs
        // no row of its own.
        let other = Uuid::now_v7();

        // An absent key reads as not-told; a stored one matches on every
        // part, and an absent part matches the rows that store none.
        assert!(
            !already_told(
                pool,
                &telling(recipient, actor, TYPE_POLL, Some(status_id), None)
            )
            .await
            .expect("the probe reads"),
            "nothing has been told yet"
        );
        let _ballot = note(pool, recipient, actor, TYPE_POLL, Some(status_id)).await;
        assert!(
            already_told(
                pool,
                &telling(recipient, actor, TYPE_POLL, Some(status_id), None)
            )
            .await
            .expect("the probe reads"),
            "the ballot is told"
        );
        assert!(
            !already_told(
                pool,
                &telling(recipient, actor, TYPE_POLL, Some(other), None)
            )
            .await
            .expect("the probe reads"),
            "another status is a different fact"
        );
        assert!(
            !already_told(
                pool,
                &telling(recipient, actor, TYPE_STATUS, Some(status_id), None)
            )
            .await
            .expect("the probe reads"),
            "another type is a different fact too"
        );

        // A status-less telling matches the status-less rows only.
        let _follow = note(pool, recipient, actor, TYPE_FOLLOW, None).await;
        assert!(
            already_told(pool, &telling(recipient, actor, TYPE_FOLLOW, None, None))
                .await
                .expect("the probe reads"),
            "the status-less row answers its own key"
        );
    }

    #[tokio::test]
    async fn already_told_keys_a_reaction_on_its_emoji() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;

        // The emoji is part of the key: one reaction emoji does not stand
        // in for another on the same status, and no other type's row
        // answers for it.
        assert!(
            !already_told(
                pool,
                &telling(
                    recipient,
                    actor,
                    TYPE_EMOJI_REACTION,
                    Some(status_id),
                    Some("🎉")
                )
            )
            .await
            .expect("the probe reads"),
            "nothing has been told yet"
        );
        let _ballot = note(pool, recipient, actor, TYPE_POLL, Some(status_id)).await;
        assert!(
            !already_told(
                pool,
                &telling(
                    recipient,
                    actor,
                    TYPE_EMOJI_REACTION,
                    Some(status_id),
                    Some("🎉")
                )
            )
            .await
            .expect("the probe reads"),
            "the emoji-less poll row does not answer for a reaction"
        );
        let _party = reaction_note(pool, recipient, actor, status_id, "🎉").await;
        assert!(
            already_told(
                pool,
                &telling(
                    recipient,
                    actor,
                    TYPE_EMOJI_REACTION,
                    Some(status_id),
                    Some("🎉")
                )
            )
            .await
            .expect("the probe reads"),
            "and the stored emoji matches"
        );
        assert!(
            !already_told(
                pool,
                &telling(
                    recipient,
                    actor,
                    TYPE_EMOJI_REACTION,
                    Some(status_id),
                    Some("👍")
                )
            )
            .await
            .expect("the probe reads"),
            "and another emoji on the same status is still a fresh fact"
        );
    }

    #[tokio::test]
    async fn already_told_ignores_dismissed_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let row = note(pool, recipient, actor, TYPE_POLL, Some(status_id)).await;
        assert!(
            already_told(
                pool,
                &telling(recipient, actor, TYPE_POLL, Some(status_id), None)
            )
            .await
            .expect("the probe reads"),
            "the live row deduplicates"
        );
        let _dismissed = dismiss(pool, recipient, row.id)
            .await
            .expect("the dismissal runs");
        assert!(
            !already_told(
                pool,
                &telling(recipient, actor, TYPE_POLL, Some(status_id), None)
            )
            .await
            .expect("the probe reads"),
            "a cleared telling lets a fresh event through"
        );
    }

    #[tokio::test]
    async fn delete_telling_takes_one_emoji_and_leaves_the_rest() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let thumb = reaction_note(pool, recipient, actor, status_id, "👍").await;
        let _party = reaction_note(pool, recipient, actor, status_id, "🎉").await;
        let _ballot = note(pool, recipient, actor, TYPE_POLL, Some(status_id)).await;

        let gone = delete_telling(
            pool,
            &telling(
                recipient,
                actor,
                TYPE_EMOJI_REACTION,
                Some(status_id),
                Some("👍"),
            ),
        )
        .await
        .expect("the withdrawal runs");
        assert_eq!(gone, 1, "exactly the one emoji's row goes");
        assert!(
            get(pool, recipient, thumb.id)
                .await
                .expect("the read runs")
                .is_none(),
            "and it is deleted, not hidden"
        );
        assert!(
            already_told(
                pool,
                &telling(
                    recipient,
                    actor,
                    TYPE_EMOJI_REACTION,
                    Some(status_id),
                    Some("🎉")
                )
            )
            .await
            .expect("the probe reads"),
            "the other reaction stands"
        );
        assert!(
            already_told(
                pool,
                &telling(recipient, actor, TYPE_POLL, Some(status_id), None)
            )
            .await
            .expect("the probe reads"),
            "and so does the poll telling beside it"
        );

        // The same key again changes nothing; a status-less key retires the
        // status-less rows on `IS NOT DISTINCT FROM`, not everything.
        let repeat = delete_telling(
            pool,
            &telling(
                recipient,
                actor,
                TYPE_EMOJI_REACTION,
                Some(status_id),
                Some("👍"),
            ),
        )
        .await
        .expect("the rerun runs");
        assert_eq!(repeat, 0, "a withdrawn telling withdraws nothing twice");
    }

    #[tokio::test]
    async fn a_follow_dedupe_answers_only_live_tellings_of_one_actor_and_type() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, _) = trio(pool).await;
        let other = solo(pool, "dup-other").await;
        let follow = telling(recipient, actor, TYPE_FOLLOW, None, None);
        assert!(
            !already_told(pool, &follow).await.expect("the read runs"),
            "a fresh recipient has been told nothing"
        );

        // A row of another type from the same actor is not the follow.
        let _favourite = note(pool, recipient, actor, TYPE_FAVOURITE, None).await;
        assert!(
            !already_told(pool, &follow).await.expect("the read runs"),
            "a different type does not answer for the follow"
        );

        let row = note(pool, recipient, actor, TYPE_FOLLOW, None).await;
        assert!(
            already_told(pool, &follow).await.expect("the read runs"),
            "the live telling counts"
        );
        assert!(
            !already_told(pool, &telling(recipient, other.id, TYPE_FOLLOW, None, None))
                .await
                .expect("the read runs"),
            "one actor's telling does not speak for another's"
        );

        let _dismissed = dismiss(pool, recipient, row.id)
            .await
            .expect("the dismissal runs");
        assert!(
            !already_told(pool, &follow)
                .await
                .expect("the repeat read runs"),
            "a dismissed row does not hold the next telling back"
        );
    }

    #[tokio::test]
    async fn delete_telling_without_a_status_takes_only_the_status_less_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let carol = solo(pool, "matching-carol").await;
        let favourite = note(pool, recipient, actor, TYPE_FAVOURITE, Some(status_id)).await;
        let foreign = note(pool, carol.id, actor, TYPE_FAVOURITE, Some(status_id)).await;
        let request = note(pool, recipient, actor, TYPE_FOLLOW_REQUEST, None).await;
        let request_with_status =
            note(pool, recipient, actor, TYPE_FOLLOW_REQUEST, Some(status_id)).await;

        let gone = delete_telling(
            pool,
            &telling(recipient, actor, TYPE_FAVOURITE, Some(status_id), None),
        )
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

        let went = delete_telling(
            pool,
            &telling(recipient, actor, TYPE_FOLLOW_REQUEST, None, None),
        )
        .await
        .expect("the request withdrawal runs");
        assert_eq!(went, 1, "the `None` status matches the status-less row");
        assert!(
            get(pool, recipient, request.id)
                .await
                .expect("the read runs")
                .is_none(),
            "the status-less request is the row that went"
        );
        assert!(
            get(pool, recipient, request_with_status.id)
                .await
                .expect("the read runs")
                .is_some(),
            "and the `None` matched NULL only, never a row that names a status"
        );
    }
}
