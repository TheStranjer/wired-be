//! The full five-part key of one telling: [`Telling`], the live-row
//! probe ([`already_told`]) and its withdrawal twin
//! ([`delete_telling`]) — the pair that names the voted status and the
//! reaction emoji the three-part reads ([`already_notified`](super::already_notified) and
//! [`delete_matching`](super::delete_matching)) stop short of.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

/// The full key of one telling: who was told, who caused it, of what, and
/// about which status and emoji.
///
/// [`already_told`] and [`delete_telling`] take this — the five-part form
/// of the key [`already_notified`](super::already_notified) and [`delete_matching`](super::delete_matching) use, which
/// cannot name what those two stop short of: the status that keeps two
/// ballots of one voter apart, and the emoji that keeps two reactions to
/// one status apart. A `None` field matches by `IS NOT DISTINCT FROM`, so
/// it names exactly the rows that store none.
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

/// The live-row probe shared by [`already_told`] and its status-less twin
/// [`already_notified`](super::already_notified)]: whether the recipient holds an undismissed
/// notification under this key.
const ALREADY_TOLD_SQL: &str = "SELECT EXISTS (SELECT 1 FROM notifications \
          WHERE user_id = $1 AND from_account_id = $2 AND type = $3 \
            AND status_id IS NOT DISTINCT FROM $4 \
            AND emoji IS NOT DISTINCT FROM $5 \
            AND dismissed_at IS NULL)";

/// Whether `telling` is already told: a live (undismissed) row matches
/// every one of its five parts.
///
/// The deduplication the repeat-prone tellings owe — the poll ballot whose
/// endpoint could re-fire, the reaction a client may `PUT` again on an
/// idempotent write — on the full key, so a second reaction with another
/// emoji is a fresh fact and not a duplicate. A dismissed row does not
/// count, exactly as in [`already_notified`](super::already_notified): the recipient cleared that
/// telling, so a fresh event may tell again.
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
/// The withdrawal [`delete_matching`](super::delete_matching) is when the telling carries an
/// emoji: retiring one reaction's notification leaves another emoji's
/// group on the same status alone. Dismissed rows go like live ones, for
/// the same reason [`delete_matching`](super::delete_matching) gives.
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
        NewNotification, Notification, TYPE_EMOJI_REACTION, TYPE_FOLLOW, TYPE_POLL, TYPE_STATUS,
        create, dismiss, get,
        test_support::{note, trio},
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
            "a cleared telling lets a fresh event through, as already_notified does"
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
}
