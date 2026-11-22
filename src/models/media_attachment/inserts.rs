//! Inserting a media attachment, and the integrity rules the table keeps
//! around its owner and its status.

use sqlx::{Executor, Postgres};

use super::{MediaAttachment, NewMediaAttachment};
use crate::{error::AppResult, util::id::new_id};

/// The `INSERT` behind [`insert`]: every column is written from a
/// [`NewMediaAttachment`] plus the minted id, with `RETURNING *` so the
/// row comes back with its database-side timestamps.
const INSERT_SQL: &str = "INSERT INTO media_attachments (
     id, user_id, status_id, position, storage_key, remote_url, url,
     preview_url, content_type, description, blurhash, width, height,
     preview_width, preview_height, duration, focus_x, focus_y, file_size,
     sensitive_hint
) VALUES (
     $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
     $16, $17, $18, $19, $20
) RETURNING *";

/// Inserts one media attachment and returns the stored row.
///
/// The id is a fresh `UUIDv7` (decision 4); every other column comes from
/// `new_attachment`. Both `user_id` and `status_id` have foreign keys, so an
/// uploader that does not exist or a status that is not stored is rejected.
/// The executor is the pool or the caller's open transaction: an ingest
/// that writes a status and its media together hands this in on the same
/// connection [`crate::models::status::insert_conn`] runs on, so a
/// rollback takes the attachments down with the status.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn insert<'e, E>(
    executor: E,
    new_attachment: &NewMediaAttachment,
) -> AppResult<MediaAttachment>
where
    E: Executor<'e, Database = Postgres>,
{
    let attachment = sqlx::query_as::<_, MediaAttachment>(INSERT_SQL)
        .bind(new_id())
        .bind(new_attachment.user_id)
        .bind(new_attachment.status_id)
        .bind(new_attachment.position)
        .bind(new_attachment.storage_key.as_deref())
        .bind(new_attachment.remote_url.as_deref())
        .bind(new_attachment.url.as_str())
        .bind(new_attachment.preview_url.as_deref())
        .bind(new_attachment.content_type.as_str())
        .bind(new_attachment.description.as_deref())
        .bind(new_attachment.blurhash.as_deref())
        .bind(new_attachment.width)
        .bind(new_attachment.height)
        .bind(new_attachment.preview_width)
        .bind(new_attachment.preview_height)
        .bind(new_attachment.duration)
        .bind(new_attachment.focus_x)
        .bind(new_attachment.focus_y)
        .bind(new_attachment.file_size)
        .bind(new_attachment.sensitive_hint)
        .fetch_one(executor)
        .await?;
    Ok(attachment)
}

#[cfg(test)]
mod tests {
    use sqlx::Error as SqlxError;

    use super::super::reads::{get_by_id, get_many};
    use super::super::test_support::{
        BASE, create_status, create_user, fetch, hard_delete, new_upload,
    };
    use super::*;
    use crate::{error::AppError, models::status};

    #[tokio::test]
    async fn insert_round_trips_every_column() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let posted = create_status(&pool, &alice).await;
        let stored = insert(
            &pool,
            &NewMediaAttachment {
                user_id: Some(alice.id),
                status_id: Some(posted.id),
                position: 2,
                storage_key: Some("alice/9b3c1d0e4f5a6b7c.png".to_owned()),
                remote_url: None,
                url: format!("{BASE}/media/alice/9b3c1d0e4f5a6b7c.png"),
                preview_url: Some(format!("{BASE}/media/alice/9b3c-small.png")),
                content_type: "image/png".to_owned(),
                description: Some("a screenshot".to_owned()),
                blurhash: Some("UBLHZ;9a?wWB%%.8t7Rj".to_owned()),
                width: Some(1024),
                height: Some(512),
                preview_width: Some(400),
                preview_height: Some(200),
                duration: None,
                focus_x: Some(0.5),
                focus_y: Some(-0.25),
                file_size: Some(345_678),
                sensitive_hint: true,
            },
        )
        .await
        .unwrap();
        assert!(stored.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(stored.user_id, Some(alice.id), "uploader round trips");
        assert_eq!(stored.status_id, Some(posted.id), "status id round trips");
        assert_eq!(stored.position, 2, "position round trips");
        assert_eq!(
            stored.storage_key.as_deref(),
            Some("alice/9b3c1d0e4f5a6b7c.png"),
            "storage key round trips"
        );
        assert!(stored.remote_url.is_none(), "a local upload has no origin");
        assert_eq!(
            stored.url,
            format!("{BASE}/media/alice/9b3c1d0e4f5a6b7c.png"),
            "the public url round trips"
        );
        assert!(stored.description.is_some(), "alt text round trips");
        assert_eq!(
            stored.blurhash.as_deref(),
            Some("UBLHZ;9a?wWB%%.8t7Rj"),
            "the blurhash round trips"
        );
        assert_eq!(stored.width, Some(1024), "width round trips");
        assert_eq!(stored.height, Some(512), "height round trips");
        assert_eq!(stored.preview_width, Some(400), "preview width round trips");
        assert_eq!(
            stored.preview_height,
            Some(200),
            "preview height round trips"
        );
        assert_eq!(stored.duration, None, "an image has no duration");
        assert_eq!(stored.focus_x, Some(0.5), "focal point x round trips");
        assert_eq!(stored.focus_y, Some(-0.25), "focal point y round trips");
        assert_eq!(stored.file_size, Some(345_678), "file size round trips");
        assert!(stored.sensitive_hint, "the sensitive flag round trips");
        assert!(
            stored.inserted_at <= stored.updated_at,
            "the database stamps both timestamps"
        );

        let many = get_many(&pool, &[stored.id]).await.unwrap();
        assert_eq!(many.len(), 1, "the stored row comes back by id");
        assert_eq!(
            many.first().map(|row| row.url.as_str()),
            Some(stored.url.as_str()),
            "get_many reads the same row"
        );
    }

    #[tokio::test]
    async fn insert_defaults_to_unattached_remote_media() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let stored = insert(
            &pool,
            &NewMediaAttachment {
                remote_url: Some("https://remote.example/media/cat.gif".to_owned()),
                url: format!("{BASE}/media/remote/cat.gif"),
                content_type: "image/gif".to_owned(),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .unwrap();
        assert!(stored.user_id.is_none(), "remote media has no uploader");
        assert!(stored.status_id.is_none(), "fresh media is unattached");
        assert_eq!(stored.position, 0, "the column default is 0");
        assert!(stored.storage_key.is_none(), "remote media is not stored");
        assert!(
            !stored.sensitive_hint,
            "the sensitive flag defaults to false"
        );
        assert!(stored.preview_url.is_none(), "nothing was detected");
        assert!(stored.description.is_none(), "no alt text by default");
        assert!(
            stored.width.is_none()
                && stored.height.is_none()
                && stored.preview_width.is_none()
                && stored.preview_height.is_none()
                && stored.duration.is_none()
                && stored.focus_x.is_none()
                && stored.focus_y.is_none()
                && stored.file_size.is_none(),
            "every media dimension stays unknown"
        );
    }

    #[tokio::test]
    async fn insert_rejects_an_unknown_uploader() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let err = insert(
            &pool,
            &NewMediaAttachment {
                user_id: Some(new_id()),
                url: format!("{BASE}/media/x/y.png"),
                content_type: "image/png".to_owned(),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .unwrap_err();
        let AppError::Database(SqlxError::Database(db_err)) = err else {
            panic!("a user_id collision must surface as a database error")
        };
        assert_eq!(
            db_err.code().as_deref(),
            Some("23503"),
            "the failure is the users foreign key"
        );
    }

    #[tokio::test]
    async fn deleting_the_uploader_nulls_user_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let stored = insert(&pool, &new_upload(alice.id)).await.unwrap();
        let dropped = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(dropped.rows_affected(), 1, "the fixture user existed");
        let orphaned = fetch(&pool, stored.id).await;
        assert!(
            orphaned.user_id.is_none(),
            "the media survives its uploader as ownerless"
        );
        assert_eq!(orphaned.url, stored.url, "the rest of the row is kept");
    }

    /// A soft delete keeps the status row, so its media stays; the cascade
    /// runs when the row actually goes.
    #[tokio::test]
    async fn deleting_a_status_takes_its_media_with_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let posted = create_status(&pool, &alice).await;
        let attached = insert(
            &pool,
            &NewMediaAttachment {
                user_id: Some(alice.id),
                status_id: Some(posted.id),
                url: format!("{BASE}/media/alice/9b3c1d0e4f5a6b7c.png"),
                content_type: "image/png".to_owned(),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .unwrap();

        let tombstone = status::soft_delete(&pool, posted.id).await.unwrap();
        assert!(tombstone.is_some(), "the status is soft-deleted");
        assert!(
            get_by_id(&pool, attached.id).await.unwrap().is_some(),
            "and its media survives, because the row stayed"
        );

        hard_delete(&pool, posted).await;
        assert!(
            get_by_id(&pool, attached.id).await.unwrap().is_none(),
            "the cascade removed the media with the row"
        );
    }
}
