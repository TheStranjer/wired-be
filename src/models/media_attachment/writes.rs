//! Writing media attachments: claiming them for a status, updating their
//! metadata and deleting them.

use sqlx::{AssertSqlSafe, Executor, PgPool, Postgres};
use uuid::Uuid;

#[cfg(doc)]
use super::reads::get_own_unattached;
use super::{MediaAttachment, MediaMetadata, claimable_predicate};
use crate::error::AppResult;

/// The 422 wording for a media id the endpoint refuses.
///
/// An id with no row, one that names somebody else's upload and one a
/// status already claims are all the same answer, so a probe never learns
/// which of the three a foreign id was.
pub const MEDIA_NOT_FOUND: &str = "Media not found";

/// The [`attach_to_status`] write, as SQL text.
///
/// Every id in `ids` that [`claimable_predicate`] lets `$3` claim takes the
/// position its id holds in the list (`$4` is the schedule replaying
/// itself, if any). The ordinality is one-based, the column counts from
/// zero, and an id submitted twice claims its row once — at its first
/// position.
fn attach_sql() -> String {
    format!(
        "UPDATE media_attachments m
      SET status_id = $2, position = v.position, updated_at = now()
     FROM (
        SELECT id, (MIN(ord) - 1)::int AS position
          FROM UNNEST($1::uuid[]) WITH ORDINALITY AS t(id, ord)
         GROUP BY id
     ) v
    WHERE m.id = v.id AND {}",
        claimable_predicate("$3", "NULL", "$4")
    )
}

/// Attaches the rows `ids` names that `user_id` may claim to `status_id`,
/// reporting how many went.
///
/// A row is claimable under the one rule [`claimable_ids`](super::claimable_ids)
/// reads too: the user's own upload that no status holds and no pending
/// schedule reserves. Each row takes the position its id holds in `ids`.
///
/// `own_schedule` is the pending schedule that is itself creating this
/// status: its row still stands while it publishes, and the reservation it
/// holds on its own uploads must not block it. Every other caller passes
/// `None`.
///
/// This is the ownership gate for posting media: rows belonging to
/// another user (or with no user at all), rows already attached to any
/// status, rows another pending schedule reserves and ids with no row are
/// all left untouched, so a client can
/// neither steal foreign media nor move media between statuses behind a
/// status's back. Callers that must reject foreign ids outright compare
/// the count against `ids.len()` — see [`crate::models::status::insert_conn`],
/// which claims a fresh post's media on its own transaction and answers a
/// short count with [`MEDIA_NOT_FOUND`].
///
/// `status_id` names a row of `statuses` (the foreign key the statuses
/// migration added), so an unknown status fails the whole write rather
/// than attaching anything.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn attach_to_status<'e, E>(
    executor: E,
    ids: &[Uuid],
    status_id: Uuid,
    user_id: Uuid,
    own_schedule: Option<Uuid>,
) -> AppResult<u64>
where
    E: Executor<'e, Database = Postgres>,
{
    let ids = ids.to_vec();
    // The statement is assembled from constants only; every value is bound.
    let updated = sqlx::query(AssertSqlSafe(attach_sql()))
        .bind(&ids)
        .bind(status_id)
        .bind(user_id)
        .bind(own_schedule)
        .execute(executor)
        .await?;
    Ok(updated.rows_affected())
}

/// Writes `metadata` onto attachment `id` and returns the updated row.
///
/// `None` when the row is gone or has fallen outside the endpoint's reach:
/// not `user_id`'s own, or claimed by a status since
/// [`get_own_unattached`] read it. The `WHERE` clause repeats that gate so
/// the check and the write are one statement; callers answer `None` with
/// their 404.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update_own_metadata(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
    metadata: &MediaMetadata,
) -> AppResult<Option<MediaAttachment>> {
    let attachment = sqlx::query_as::<_, MediaAttachment>(
        "UPDATE media_attachments
            SET description = $3, focus_x = $4, focus_y = $5,
                preview_url = $6, updated_at = now()
          WHERE id = $1 AND user_id = $2 AND status_id IS NULL
        RETURNING *",
    )
    .bind(id)
    .bind(user_id)
    .bind(metadata.description.as_deref())
    .bind(metadata.focus_x)
    .bind(metadata.focus_y)
    .bind(metadata.preview_url.as_deref())
    .fetch_optional(pool)
    .await?;
    Ok(attachment)
}

/// Deletes attachment `id`, reporting whether a row went. The stored
/// file, if any, is not touched here: callers delete it through
/// [`crate::uploads::UploaderBackend`] when they want the bytes gone.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, id: Uuid) -> AppResult<bool> {
    let deleted = sqlx::query("DELETE FROM media_attachments WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// The `DELETE` behind [`delete_all_for_user`]: the account's uploads and
/// the attachment rows of its statuses (the statuses survive as
/// tombstones, so no cascade reaches their media).
const DELETE_ALL_FOR_USER_SQL: &str = "DELETE FROM media_attachments
      WHERE user_id = $1
         OR status_id IN (SELECT id FROM statuses WHERE user_id = $1)";

/// Removes every media row belonging to user `user_id`: the media step of
/// the [`users.delete`](crate::jobs::deletion::KIND_DELETE) job.
///
/// Both shapes go — uploaded by the account (`user_id`) or attached to a
/// status it authored (`status_id`). Only the rows go; the stored files
/// are the upload layer's, and unattached media of a deleted account
/// leaves no URL anyone can mint.
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
    use sqlx::Error as SqlxError;

    use super::super::{
        NewMediaAttachment,
        inserts::insert,
        reads::{for_statuses, get_by_id},
        test_support::{BASE, create_status, create_user, fetch, new_upload},
    };
    use super::*;
    use crate::{
        error::AppError,
        models::scheduled_status::{self, NewScheduledStatus, ScheduledStatus},
        util::id::new_id,
    };

    /// An upload a pending schedule reserves is out of every other create's
    /// reach, and the schedule itself — named as `own_schedule` while it
    /// publishes — still claims it.
    #[tokio::test]
    async fn attach_to_status_respects_schedule_reservations() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let reserved = insert(&pool, &new_upload(alice.id)).await.unwrap();
        let free = insert(&pool, &new_upload(alice.id)).await.unwrap();
        let schedule: ScheduledStatus = scheduled_status::insert(
            &pool,
            &NewScheduledStatus {
                user_id: alice.id,
                scheduled_at: chrono::Utc::now() + chrono::TimeDelta::hours(1),
                params: serde_json::json!({
                    "status": "later",
                    "media_ids": [reserved.id.to_string()],
                }),
                idempotency_key: None,
            },
        )
        .await
        .unwrap()
        .expect("a keyless schedule never conflicts");
        let posted = create_status(&pool, &alice).await;

        let others = attach_to_status(&pool, &[reserved.id, free.id], posted.id, alice.id, None)
            .await
            .unwrap();
        assert_eq!(others, 1, "only the unreserved upload went");
        assert!(
            fetch(&pool, reserved.id).await.status_id.is_none(),
            "the reserved upload stayed unclaimed"
        );

        let stranger = new_id();
        let wrong = attach_to_status(&pool, &[reserved.id], posted.id, alice.id, Some(stranger))
            .await
            .unwrap();
        assert_eq!(wrong, 0, "another schedule's id is no key to the row");

        let own = attach_to_status(
            &pool,
            &[reserved.id],
            posted.id,
            alice.id,
            Some(schedule.id),
        )
        .await
        .unwrap();
        assert_eq!(own, 1, "the reserving schedule claims its own upload");
        assert_eq!(
            fetch(&pool, reserved.id).await.status_id,
            Some(posted.id),
            "and the row now names the status"
        );
    }

    #[tokio::test]
    async fn attach_to_status_only_attaches_own_unattached_media() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let bob = create_user(&pool, "bob").await;
        let mine1 = insert(&pool, &new_upload(alice.id)).await.unwrap();
        let mine2 = insert(&pool, &new_upload(alice.id)).await.unwrap();
        let foreign = insert(&pool, &new_upload(bob.id)).await.unwrap();
        let claimed = create_status(&pool, &alice).await;
        let already = insert(
            &pool,
            &NewMediaAttachment {
                status_id: Some(claimed.id),
                ..new_upload(alice.id)
            },
        )
        .await
        .unwrap();
        let mine_remote = insert(
            &pool,
            &NewMediaAttachment {
                user_id: None,
                ..new_upload(alice.id)
            },
        )
        .await
        .unwrap();

        let posted = create_status(&pool, &alice).await;
        let ids = [
            mine1.id,
            mine2.id,
            foreign.id,
            already.id,
            mine_remote.id,
            new_id(),
        ];
        let attached = attach_to_status(&pool, &ids, posted.id, alice.id, None)
            .await
            .unwrap();
        assert_eq!(attached, 2, "only alice's two unattached rows went");
        assert_eq!(
            fetch(&pool, mine1.id).await.status_id,
            Some(posted.id),
            "the first row now names the status"
        );
        assert_eq!(
            fetch(&pool, mine2.id).await.status_id,
            Some(posted.id),
            "so does the second"
        );
        assert!(
            fetch(&pool, foreign.id).await.status_id.is_none(),
            "a foreign user's media cannot be attached"
        );
        let kept = already.id;
        assert_eq!(
            fetch(&pool, kept).await.status_id,
            already.status_id,
            "media already on another status is not moved"
        );
        assert!(
            fetch(&pool, mine_remote.id).await.status_id.is_none(),
            "ownerless (remote) media cannot be attached"
        );

        let again = attach_to_status(&pool, &[mine1.id, mine2.id], posted.id, alice.id, None)
            .await
            .unwrap();
        assert_eq!(again, 0, "a double attach is ignored");
        let empty = attach_to_status(&pool, &[], posted.id, alice.id, None)
            .await
            .unwrap();
        assert_eq!(empty, 0, "no ids attach nothing");
    }

    /// The order the attach writes is the order the ids arrive in, not the
    /// order the rows were uploaded in — and an id sent twice claims its row
    /// once, at its first position.
    #[tokio::test]
    async fn attach_to_status_keeps_the_requested_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let first = insert(&pool, &new_upload(alice.id)).await.unwrap();
        let second = insert(&pool, &new_upload(alice.id)).await.unwrap();
        let third = insert(&pool, &new_upload(alice.id)).await.unwrap();
        let posted = create_status(&pool, &alice).await;

        let attached = attach_to_status(
            &pool,
            &[third.id, first.id, third.id],
            posted.id,
            alice.id,
            None,
        )
        .await
        .unwrap();
        assert_eq!(attached, 2, "the repeated id claims its row once");
        assert_eq!(fetch(&pool, third.id).await.position, 0);
        assert_eq!(fetch(&pool, first.id).await.position, 1);
        assert_eq!(
            fetch(&pool, second.id).await.status_id,
            None,
            "an id the list never named stays unposted"
        );

        let rows = for_statuses(&pool, &[posted.id]).await.unwrap();
        let order: Vec<Uuid> = rows
            .get(&posted.id)
            .expect("the status answers its attachments")
            .iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(
            order,
            vec![third.id, first.id],
            "and the render order is the order the client asked for"
        );
    }

    /// The foreign key this ticket added: `status_id` names a stored status,
    /// both on the way in and when attaching afterwards.
    #[tokio::test]
    async fn media_can_only_name_a_status_that_exists() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let err = insert(
            &pool,
            &NewMediaAttachment {
                user_id: Some(alice.id),
                status_id: Some(new_id()),
                url: format!("{BASE}/media/alice/9b3c1d0e4f5a6b7c.png"),
                content_type: "image/png".to_owned(),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .expect_err("media cannot name a status that was never posted");
        let AppError::Database(SqlxError::Database(db_err)) = err else {
            panic!("a status_id collision must surface as a database error")
        };
        assert_eq!(
            db_err.code().as_deref(),
            Some("23503"),
            "the failure is the statuses foreign key"
        );

        let unattached = insert(&pool, &new_upload(alice.id)).await.unwrap();
        let attach_err = attach_to_status(&pool, &[unattached.id], new_id(), alice.id, None)
            .await
            .expect_err("attaching to an unknown status is not possible");
        let AppError::Database(SqlxError::Database(attach_db_err)) = attach_err else {
            panic!("a status_id collision must surface as a database error")
        };
        assert_eq!(
            attach_db_err.code().as_deref(),
            Some("23503"),
            "and the same key stops the attach"
        );
        assert!(
            fetch(&pool, unattached.id).await.status_id.is_none(),
            "the rejected attach left the media unposted"
        );
    }

    #[tokio::test]
    async fn update_own_metadata_writes_all_four_columns() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let stored = insert(&pool, &new_upload(alice.id)).await.unwrap();

        let updated = update_own_metadata(
            &pool,
            stored.id,
            alice.id,
            &MediaMetadata {
                description: Some("a cat".to_owned()),
                focus_x: Some(0.5),
                focus_y: Some(-0.5),
                preview_url: Some(format!("{BASE}/media/alice/9b3c-small.png")),
            },
        )
        .await
        .unwrap()
        .expect("the row exists and is alice's unattached media");
        assert_eq!(
            updated.description.as_deref(),
            Some("a cat"),
            "alt text is written"
        );
        assert_eq!(updated.focus_x, Some(0.5), "focal point x is written");
        assert_eq!(updated.focus_y, Some(-0.5), "focal point y is written");
        assert_eq!(
            updated.preview_url.as_deref(),
            Some("https://test.example/media/alice/9b3c-small.png"),
            "the preview url is written"
        );
        assert_eq!(updated.url, stored.url, "the rest of the row is kept");

        let cleared = update_own_metadata(&pool, stored.id, alice.id, &MediaMetadata::default())
            .await
            .unwrap()
            .expect("the row still exists");
        assert!(
            cleared.description.is_none()
                && cleared.focus_x.is_none()
                && cleared.focus_y.is_none()
                && cleared.preview_url.is_none(),
            "absent metadata clears the columns"
        );

        assert!(
            update_own_metadata(&pool, new_id(), alice.id, &MediaMetadata::default())
                .await
                .unwrap()
                .is_none(),
            "an unknown id updates nothing"
        );
    }

    #[tokio::test]
    async fn update_own_metadata_only_touches_own_unattached_media() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let bob = create_user(&pool, "bob").await;
        let metadata = MediaMetadata {
            description: Some("hijacked".to_owned()),
            ..MediaMetadata::default()
        };

        let posted = create_status(&pool, &alice).await;
        let attached = insert(
            &pool,
            &NewMediaAttachment {
                status_id: Some(posted.id),
                ..new_upload(alice.id)
            },
        )
        .await
        .unwrap();
        let foreign = insert(&pool, &new_upload(bob.id)).await.unwrap();

        for nope in [attached.id, foreign.id] {
            assert!(
                update_own_metadata(&pool, nope, alice.id, &metadata)
                    .await
                    .unwrap()
                    .is_none(),
                "`{nope}` is out of alice's reach: attached or foreign"
            );
        }
        assert_eq!(
            fetch(&pool, attached.id).await.description,
            None,
            "the attached row kept its (empty) description"
        );
        assert_eq!(
            fetch(&pool, foreign.id).await.description,
            None,
            "and the foreign row never heard of alice"
        );
    }

    #[tokio::test]
    async fn delete_removes_the_row_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let stored = insert(&pool, &new_upload(alice.id)).await.unwrap();
        assert!(
            delete(&pool, stored.id).await.unwrap(),
            "the first delete reports the row"
        );
        assert!(
            get_by_id(&pool, stored.id).await.unwrap().is_none(),
            "the row is gone"
        );
        assert!(
            !(delete(&pool, stored.id).await.unwrap()),
            "deleting again finds nothing"
        );
    }
}
