//! Reading and retiring rows: [`get`] for one owned notification
//! whatever its state, the three dismissal shapes ([`dismiss`],
//! [`dismiss_all`] and [`dismiss_ids`]), [`mark_read_through_id`] for
//! Pleroma's `set_read_up_to`, and [`dismiss_group`] for a whole group.

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::error::AppResult;

use super::{GroupSelector, Notification, flat_page::push_scope};

/// The stored notification `id` of `user_id`, whatever its state.
///
/// Pleroma's `Notification.get/2`, which only asks who the row belongs
/// to; `None` names no notification the caller owns (or the id text was
/// no id at all), which the endpoints answer 404.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn get(pool: &PgPool, user_id: Uuid, id: Uuid) -> AppResult<Option<Notification>> {
    let row = sqlx::query_as::<_, Notification>(
        "SELECT * FROM notifications WHERE id = $1 AND user_id = $2",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Dismisses the one notification `id` of `user_id` and returns the row.
///
/// The write stamps [`dismissed_at`](Notification::dismissed_at), keeping
/// the first stamp when it already has one; `None` when the caller owns
/// no such notification, foreign and unknown ids alike (Pleroma's
/// `dismiss` "Cannot dismiss notification").
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn dismiss(pool: &PgPool, user_id: Uuid, id: Uuid) -> AppResult<Option<Notification>> {
    let row = sqlx::query_as::<_, Notification>(
        "UPDATE notifications
            SET dismissed_at = COALESCE(dismissed_at, now())
          WHERE id = $1 AND user_id = $2
          RETURNING *",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Dismisses every undismissed notification of `user_id`.
///
/// The dismissal Pleroma's `clear` performs, soft here; an account with
/// nothing to dismiss changes nothing and is not an error. Returns how
/// many rows changed.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn dismiss_all(pool: &PgPool, user_id: Uuid) -> AppResult<u64> {
    let outcome = sqlx::query(
        "UPDATE notifications
            SET dismissed_at = now()
          WHERE user_id = $1 AND dismissed_at IS NULL",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(outcome.rows_affected())
}

/// Dismisses the notifications of `user_id` listed in `ids`.
///
/// The bulk dismissal Pleroma's `destroy_multiple` performs, soft here.
/// Ids the caller does not own, that name nothing, or that are dismissed
/// already simply change nothing; an empty list is the same. Returns how
/// many rows changed.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn dismiss_ids(pool: &PgPool, user_id: Uuid, ids: &[Uuid]) -> AppResult<u64> {
    let outcome = sqlx::query(
        "UPDATE notifications
            SET dismissed_at = now()
          WHERE user_id = $1 AND dismissed_at IS NULL AND id = ANY($2)",
    )
    .bind(user_id)
    .bind(ids.to_vec())
    .execute(pool)
    .await?;
    Ok(outcome.rows_affected())
}

/// Marks every unseen notification of `user_id` up to `max_id` seen.
///
/// Pleroma's `set_read_up_to`, whose `id <= ^id` walk is plain text
/// comparison here (decision 4); returns the rows this call changed,
/// newest first. Rows seen already do not appear again, so a repeat
/// marking answers `[]`.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn mark_read_through_id(
    pool: &PgPool,
    user_id: Uuid,
    max_id: Uuid,
) -> AppResult<Vec<Notification>> {
    let rows = sqlx::query_as::<_, Notification>(
        "WITH marked AS (
             UPDATE notifications
                SET seen = true
              WHERE user_id = $1 AND NOT seen AND id <= $2
             RETURNING *
         )
         SELECT * FROM marked ORDER BY id DESC",
    )
    .bind(user_id)
    .bind(max_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Marks every undismissed notification of one group dismissed.
///
/// Returns how many rows changed. Dismissing an unknown group (or one
/// already dismissed) changes nothing and is not an error — Pleroma's
/// `dismiss_group` answers `{}` either way.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn dismiss_group(
    pool: &PgPool,
    user_id: Uuid,
    selector: &GroupSelector,
) -> AppResult<u64> {
    let mut qb = QueryBuilder::<Postgres>::new("UPDATE notifications n SET dismissed_at = now()");
    push_scope(&mut qb, user_id);
    match selector {
        GroupSelector::Notification(id) => {
            let _id: &mut _ = qb.push(" AND n.id = ").push_bind(*id);
        }
        GroupSelector::GroupKey(key) => {
            let _key: &mut _ = qb.push(" AND n.group_key = ").push_bind(key.clone());
        }
    }
    let outcome = qb.build().execute(pool).await?;
    Ok(outcome.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::models::notification::{
        NewNotification, TYPE_FAVOURITE, TYPE_REBLOG, create, test_support::trio,
    };

    #[tokio::test]
    async fn dismissal_is_scoped_to_the_owner_and_stamps_once() {
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

        let foreign = Uuid::now_v7();
        assert!(
            dismiss(pool, foreign, row.id)
                .await
                .expect("read")
                .is_none(),
            "a stranger's dismissal finds no row"
        );
        assert!(
            dismiss(pool, recipient, Uuid::now_v7())
                .await
                .expect("read")
                .is_none(),
            "and an unknown id does too"
        );

        let once = dismiss(pool, recipient, row.id)
            .await
            .expect("the dismissal runs")
            .expect("the owner's row comes back");
        assert!(once.dismissed_at.is_some(), "the row is stamped");
        let twice = dismiss(pool, recipient, row.id)
            .await
            .expect("the repeat runs")
            .expect("the row still answers");
        assert_eq!(
            twice.dismissed_at, once.dismissed_at,
            "a repeat dismissal keeps the first stamp"
        );
    }

    #[tokio::test]
    async fn clear_and_the_bulk_dismissal_only_take_the_callers_rows() {
        use crate::{
            config::TEST_BASE_URL,
            models::user::{NewUser, insert, local_ap_id},
        };
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let stranger = insert(
            pool,
            &NewUser::local(
                "notif-stranger",
                &local_ap_id(TEST_BASE_URL, "notif-stranger"),
            ),
        )
        .await
        .expect("stranger fixture");
        let other = stranger.id;
        let mine_one = create(
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
        .expect("the first notification inserts");
        let _mine_two = create(
            pool,
            &NewNotification {
                user_id: recipient,
                from_account_id: actor,
                notification_type: TYPE_REBLOG,
                status_id: Some(status_id),
                emoji: None,
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the second notification inserts");
        let theirs = create(
            pool,
            &NewNotification {
                user_id: other,
                from_account_id: actor,
                notification_type: TYPE_FAVOURITE,
                status_id: Some(status_id),
                emoji: None,
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the foreign notification inserts");

        let bulk = dismiss_ids(pool, recipient, &[mine_one.id, theirs.id, Uuid::now_v7()])
            .await
            .expect("the bulk dismissal runs");
        assert_eq!(bulk, 1, "only the one listed row of theirs changed");
        let still_live = get(pool, other, theirs.id)
            .await
            .expect("the read runs")
            .expect("the foreign row is still there");
        assert!(still_live.dismissed_at.is_none(), "and stayed undismissed");

        let cleared = dismiss_all(pool, recipient).await.expect("clear runs");
        assert_eq!(cleared, 1, "the second row goes, the first was gone");
        assert_eq!(
            dismiss_all(pool, recipient).await.expect("repeat clear"),
            0,
            "nothing undismissed is left, and that is not an error"
        );
        let untouched = get(pool, other, theirs.id)
            .await
            .expect("the read runs")
            .expect("the foreign row survives the clear too");
        assert!(untouched.dismissed_at.is_none(), "clear is scoped");
    }

    #[tokio::test]
    async fn marking_read_walks_back_from_the_cursor() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let (recipient, actor, status_id) = trio(pool).await;
        let first = create(
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
        .expect("the first notification inserts");
        let second = create(
            pool,
            &NewNotification {
                user_id: recipient,
                from_account_id: actor,
                notification_type: TYPE_REBLOG,
                status_id: Some(status_id),
                emoji: None,
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the second notification inserts");

        let marked = mark_read_through_id(pool, recipient, first.id)
            .await
            .expect("the marking runs");
        assert_eq!(
            marked.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![first.id],
            "the cursor itself and everything older, newest first"
        );
        assert!(marked.first().expect("one row").seen, "the row reads seen");
        let still_unseen = get(pool, recipient, second.id)
            .await
            .expect("the read runs")
            .expect("the newer row is there");
        assert!(!still_unseen.seen, "and the newer row is untouched");

        let second_pass = mark_read_through_id(pool, recipient, second.id)
            .await
            .expect("the repeat runs");
        assert_eq!(
            second_pass.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![second.id],
            "rows seen already are not marked twice"
        );
    }
}
