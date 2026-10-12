//! The `media_attachments` table: uploaded and remotely fetched media.
//!
//! One row per attachment (Pleroma keeps these as a jsonb array inside
//! each activity, `lib/pleroma/activity.ex`; the relational shape follows
//! `docs/ARCHITECTURE.md` decision 6). Local uploads carry a
//! [`storage_key`](MediaAttachment::storage_key) into
//! [`crate::uploads`] and no [`remote_url`](MediaAttachment::remote_url);
//! remote media is the other way round and has no owner until federation
//! claims one — `user_id` is `NULL` for it, and survives the deletion of
//! an uploader through `ON DELETE SET NULL`.
//!
//! [`status_id`](MediaAttachment::status_id) is a foreign key into
//! `statuses` (added by the statuses migration) and cascades on a hard
//! delete. [`attach_to_status`] is the only writer that claims media for a
//! status after the fact, and it attaches exactly the rows the given user
//! owns that no status has claimed yet.
//!
//! [`insert`] mints the primary key (`UUIDv7`, decision 4); everything
//! else comes from a [`NewMediaAttachment`], whose
//! [`Default`](NewMediaAttachment::default) mirrors the column defaults.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

/// One row of `media_attachments`: a single attached media file.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct MediaAttachment {
    /// `UUIDv7` primary key, minted by [`insert`] (decision 4).
    pub id: Uuid,
    /// Uploading user; `None` for remote media (and for rows whose
    /// uploader has since been deleted).
    pub user_id: Option<Uuid>,
    /// Status this attachment belongs to; `None` while unposted. A foreign
    /// key, so a status that is hard-deleted takes its media with it.
    pub status_id: Option<Uuid>,
    /// Order within the status's attachment list.
    pub position: i32,
    /// Key into the upload storage for a local file; `None` for remote
    /// media.
    pub storage_key: Option<String>,
    /// Original URL of remote media; `None` for local uploads.
    pub remote_url: Option<String>,
    /// Public URL clients read.
    pub url: String,
    /// Preview/thumbnail URL; `None` means clients fall back to
    /// [`url`](Self::url).
    pub preview_url: Option<String>,
    /// `MIME` type of the media, e.g. `image/png`.
    pub content_type: String,
    /// Alt text describing the media for accessibility.
    pub description: Option<String>,
    /// `Blurhash` string for the placeholder preview.
    pub blurhash: Option<String>,
    /// Width of the original media in pixels.
    pub width: Option<i32>,
    /// Height of the original media in pixels.
    pub height: Option<i32>,
    /// Length of audio and video in seconds.
    pub duration: Option<f64>,
    /// Horizontal focal point for cropping, in `[-1.0, 1.0]`.
    pub focus_x: Option<f32>,
    /// Vertical focal point for cropping, in `[-1.0, 1.0]`.
    pub focus_y: Option<f32>,
    /// Size of the local file in bytes.
    pub file_size: Option<i64>,
    /// Whether the uploader flagged the media as sensitive.
    pub sensitive_hint: bool,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every update here.
    pub updated_at: DateTime<Utc>,
}

/// What to insert: every `media_attachments` column except the `id`
/// [`insert`] mints itself and the timestamps the database stamps.
///
/// [`Default::default`] mirrors the column defaults, so a caller sets
/// only what it knows; the usual minimum is `url`, `content_type` and the
/// uploader's `user_id`.
#[derive(Clone, Debug, Default)]
pub struct NewMediaAttachment {
    /// Uploading user; `None` for remote media.
    pub user_id: Option<Uuid>,
    /// Status to attach to straight away; `None` for unposted media.
    pub status_id: Option<Uuid>,
    /// Order within the status's attachment list; `0` by default.
    pub position: i32,
    /// Upload storage key for a local file.
    pub storage_key: Option<String>,
    /// Original URL for remote media.
    pub remote_url: Option<String>,
    /// Public URL; required, empty in the default.
    pub url: String,
    /// Preview/thumbnail URL.
    pub preview_url: Option<String>,
    /// `MIME` type; required, empty in the default.
    pub content_type: String,
    /// Alt text.
    pub description: Option<String>,
    /// `Blurhash` string.
    pub blurhash: Option<String>,
    /// Width in pixels.
    pub width: Option<i32>,
    /// Height in pixels.
    pub height: Option<i32>,
    /// Duration in seconds (audio and video).
    pub duration: Option<f64>,
    /// Horizontal focal point.
    pub focus_x: Option<f32>,
    /// Vertical focal point.
    pub focus_y: Option<f32>,
    /// File size in bytes.
    pub file_size: Option<i64>,
    /// Sensitive flag; `false` by default, like the column.
    pub sensitive_hint: bool,
}

/// The `INSERT` behind [`insert`]: every column is written from a
/// [`NewMediaAttachment`] plus the minted id, with `RETURNING *` so the
/// row comes back with its database-side timestamps.
const INSERT_SQL: &str = "INSERT INTO media_attachments (
     id, user_id, status_id, position, storage_key, remote_url, url,
     preview_url, content_type, description, blurhash, width, height,
     duration, focus_x, focus_y, file_size, sensitive_hint
) VALUES (
     $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
     $16, $17, $18
) RETURNING *";

/// Inserts one media attachment and returns the stored row.
///
/// The id is a fresh `UUIDv7` (decision 4); every other column comes from
/// `new_attachment`. Both `user_id` and `status_id` have foreign keys, so an
/// uploader that does not exist or a status that is not stored is rejected.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn insert(
    pool: &PgPool,
    new_attachment: &NewMediaAttachment,
) -> AppResult<MediaAttachment> {
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
        .bind(new_attachment.duration)
        .bind(new_attachment.focus_x)
        .bind(new_attachment.focus_y)
        .bind(new_attachment.file_size)
        .bind(new_attachment.sensitive_hint)
        .fetch_one(pool)
        .await?;
    Ok(attachment)
}

/// Fetches the attachment whose primary key is `id`, `None` when no row
/// matches.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> AppResult<Option<MediaAttachment>> {
    let attachment =
        sqlx::query_as::<_, MediaAttachment>("SELECT * FROM media_attachments WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(attachment)
}

/// [`get_chat_attachment`]: the media a chat message may carry — `user_id`'s
/// own, claimed by no status yet, and `image/*`, `video/*` or `audio/*`.
/// Pleroma's `ChatController` posts `media_id` through
/// `Object.authorize_access` and its api spec names the kind the same way.
const CHAT_ATTACHMENT_SQL: &str = "SELECT * FROM media_attachments
     WHERE id = $1 AND user_id = $2 AND status_id IS NULL
       AND lower(split_part(content_type, '/', 1)) IN ('image', 'video', 'audio')";

/// The media row `id`, provided `user_id` uploaded it, no status has claimed
/// it, and its `MIME` type is image, video or audio — the media a chat
/// message may attach ([`crate::api::chats`]' `POST …/messages` gate).
///
/// An id with no row, one owned by somebody else, one already posted with a
/// status, and a non-media file are all answered `None`: the chat route gives
/// them all its one 400, and naming the difference would tell a caller which
/// foreign ids exist.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_chat_attachment(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
) -> AppResult<Option<MediaAttachment>> {
    let attachment = sqlx::query_as::<_, MediaAttachment>(CHAT_ATTACHMENT_SQL)
        .bind(id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(attachment)
}

/// Fetches the attachments whose ids are in `ids`, in `id` order
/// (`UUIDv7` text sorts by creation, decision 4). Ids with no row are
/// skipped; an empty `ids` yields an empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_many(pool: &PgPool, ids: &[Uuid]) -> AppResult<Vec<MediaAttachment>> {
    let ids = ids.to_vec();
    let attachments = sqlx::query_as::<_, MediaAttachment>(
        "SELECT * FROM media_attachments WHERE id = ANY($1) ORDER BY id",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    Ok(attachments)
}

/// Attaches every row `ids` names that user `user_id` owns and that no
/// status has claimed yet to `status_id`, reporting how many rows went.
///
/// This is the ownership gate for posting media: rows belonging to
/// another user (or with no user at all), rows already attached to any
/// status and ids with no row are all left untouched, so a client can
/// neither steal foreign media nor move media between statuses behind a
/// status's back. Callers that must reject foreign ids outright compare
/// the count against `ids.len()`.
///
/// `status_id` names a row of `statuses` (the foreign key the statuses
/// migration added), so an unknown status fails the whole write rather
/// than attaching anything.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn attach_to_status(
    pool: &PgPool,
    ids: &[Uuid],
    status_id: Uuid,
    user_id: Uuid,
) -> AppResult<u64> {
    let ids = ids.to_vec();
    let updated = sqlx::query(
        "UPDATE media_attachments
            SET status_id = $2, updated_at = now()
          WHERE id = ANY($1) AND user_id = $3 AND status_id IS NULL",
    )
    .bind(&ids)
    .bind(status_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected())
}

/// Fetches the attachments of every status in `status_ids`, grouped by
/// status and ordered by `position` then `id` within each group.
///
/// Statuses that carry no attachments get no entry in the map at all;
/// callers wanting a list for each one read it with
/// [`HashMap::entry`] or [`get`](`HashMap::get`) and an empty default. An
/// empty `status_ids` yields an empty map.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn for_statuses(
    pool: &PgPool,
    status_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<MediaAttachment>>> {
    let status_ids = status_ids.to_vec();
    let rows = sqlx::query_as::<_, MediaAttachment>(
        "SELECT * FROM media_attachments
          WHERE status_id = ANY($1)
          ORDER BY status_id, position, id",
    )
    .bind(&status_ids)
    .fetch_all(pool)
    .await?;
    let mut grouped: HashMap<Uuid, Vec<MediaAttachment>> = HashMap::new();
    for row in rows {
        if let Some(status_id) = row.status_id {
            grouped.entry(status_id).or_default().push(row);
        }
    }
    Ok(grouped)
}

/// The media `GET` and `PUT /api/v1/media/{id}` serve: the row `id`,
/// provided `user_id` uploaded it and no status has claimed it.
///
/// Mastodon's semantics for these endpoints: media attached to a status is
/// edited through the status, not here, so an attached row is answered the
/// same as an unknown or foreign one — the caller's one 404 keeps a client
/// from learning which foreign or claimed ids exist.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_own_unattached(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
) -> AppResult<Option<MediaAttachment>> {
    let attachment = sqlx::query_as::<_, MediaAttachment>(
        "SELECT * FROM media_attachments
          WHERE id = $1 AND user_id = $2 AND status_id IS NULL",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(attachment)
}

/// The metadata `PUT /api/v1/media/{id}` writes onto a row: alt text, the
/// focal point and the preview URL.
///
/// Like [`NewMediaAttachment`], every column is written wholesale — a `None`
/// clears what was stored. The endpoint merges present-only parameters over
/// the stored row before building one, so an absent parameter arrives here
/// as the value already kept.
#[derive(Clone, Debug, Default)]
pub struct MediaMetadata {
    /// Alt text.
    pub description: Option<String>,
    /// Horizontal focal point.
    pub focus_x: Option<f32>,
    /// Vertical focal point.
    pub focus_y: Option<f32>,
    /// Preview/thumbnail URL.
    pub preview_url: Option<String>,
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

#[cfg(test)]
mod tests {
    use sqlx::{Error as SqlxError, PgPool};

    use super::*;
    use crate::{
        error::AppError,
        models::{
            status::{self, NewStatus, Status},
            user::{NewUser, User, insert as insert_user, local_ap_id},
        },
    };

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = "https://test.example";

    /// Inserts a local user named `nickname` for the ownership tests.
    async fn create_user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("fixture user inserts")
    }

    /// Inserts a local status of `author`, because `status_id` is a foreign
    /// key and so every fixture that sets one needs a stored row.
    async fn create_status(pool: &PgPool, author: &User) -> Status {
        status::insert(pool, BASE, &NewStatus::local(author, "a posted photo"))
            .await
            .expect("fixture status inserts")
    }

    /// Deletes `status` for real, which is what takes its media with it.
    async fn hard_delete(pool: &PgPool, status: Status) {
        let removed = sqlx::query("DELETE FROM statuses WHERE id = $1")
            .bind(status.id)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(removed.rows_affected(), 1, "the fixture status existed");
    }

    /// A [`NewMediaAttachment`] uploaded by `user_id` and stored at a key
    /// under [`BASE`], with nothing else known about the file.
    fn new_upload(user_id: Uuid) -> NewMediaAttachment {
        NewMediaAttachment {
            user_id: Some(user_id),
            url: format!("{BASE}/media/{user_id}/9b3c1d0e4f5a6b7c.png"),
            storage_key: Some(format!("{user_id}/9b3c1d0e4f5a6b7c.png")),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        }
    }

    /// Reads the row behind `id` back out of the table, panicking if absent.
    async fn fetch(pool: &PgPool, id: Uuid) -> MediaAttachment {
        get_by_id(pool, id).await.unwrap().expect("row exists")
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn insert_round_trips_every_column(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn insert_defaults_to_unattached_remote_media(pool: PgPool) {
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
                && stored.duration.is_none()
                && stored.focus_x.is_none()
                && stored.focus_y.is_none()
                && stored.file_size.is_none(),
            "every media dimension stays unknown"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn insert_rejects_an_unknown_uploader(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn deleting_the_uploader_nulls_user_id(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn lookups_of_unknown_ids_are_empty(pool: PgPool) {
        assert!(
            get_by_id(&pool, new_id()).await.unwrap().is_none(),
            "an unknown id finds nothing"
        );
        assert!(
            get_many(&pool, &[]).await.unwrap().is_empty(),
            "no ids, no rows"
        );
        assert!(
            get_many(&pool, &[new_id()]).await.unwrap().is_empty(),
            "only unknown ids, still no rows"
        );
    }

    /// A [`NewMediaAttachment`] of `user_id` with the given `content_type`.
    fn upload_of(user_id: Uuid, content_type: &str) -> NewMediaAttachment {
        NewMediaAttachment {
            content_type: content_type.to_owned(),
            ..new_upload(user_id)
        }
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn chat_media_is_own_unattached_image_video_or_audio(pool: PgPool) {
        let alice = create_user(&pool, "alice").await;
        let bob = create_user(&pool, "bob").await;
        let image = insert(&pool, &new_upload(alice.id)).await.unwrap();
        let video = insert(&pool, &upload_of(alice.id, "video/mp4"))
            .await
            .unwrap();
        let audio = insert(&pool, &upload_of(alice.id, "audio/mpeg"))
            .await
            .unwrap();
        let claim = create_status(&pool, &alice).await;
        let posted = insert(
            &pool,
            &NewMediaAttachment {
                status_id: Some(claim.id),
                ..new_upload(alice.id)
            },
        )
        .await
        .unwrap();
        let foreign = insert(&pool, &new_upload(bob.id)).await.unwrap();
        let document = insert(&pool, &upload_of(alice.id, "application/pdf"))
            .await
            .unwrap();
        let ownerless = insert(
            &pool,
            &NewMediaAttachment {
                user_id: None,
                ..new_upload(alice.id)
            },
        )
        .await
        .unwrap();

        for mine in [image.id, video.id, audio.id] {
            let found = get_chat_attachment(&pool, mine, alice.id)
                .await
                .unwrap()
                .expect("alice's own unattached media is attachable");
            assert_eq!(found.id, mine, "and it is the row named");
        }
        for nope in [new_id(), posted.id, foreign.id, document.id, ownerless.id] {
            assert!(
                get_chat_attachment(&pool, nope, alice.id)
                    .await
                    .unwrap()
                    .is_none(),
                "`{nope}` is no chat media for alice: unknown, claimed, \
                 foreign, not media, or ownerless alike"
            );
        }
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn attach_to_status_only_attaches_own_unattached_media(pool: PgPool) {
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
        let attached = attach_to_status(&pool, &ids, posted.id, alice.id)
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

        let again = attach_to_status(&pool, &[mine1.id, mine2.id], posted.id, alice.id)
            .await
            .unwrap();
        assert_eq!(again, 0, "a double attach is ignored");
        let empty = attach_to_status(&pool, &[], posted.id, alice.id)
            .await
            .unwrap();
        assert_eq!(empty, 0, "no ids attach nothing");
    }

    /// The foreign key this ticket added: `status_id` names a stored status,
    /// both on the way in and when attaching afterwards.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn media_can_only_name_a_status_that_exists(pool: PgPool) {
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
        let attach_err = attach_to_status(&pool, &[unattached.id], new_id(), alice.id)
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

    /// A soft delete keeps the status row, so its media stays; the cascade
    /// runs when the row actually goes.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn deleting_a_status_takes_its_media_with_it(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn for_statuses_groups_by_status_and_orders_by_position(pool: PgPool) {
        let alice = create_user(&pool, "alice").await;
        let first = create_status(&pool, &alice).await;
        let second = create_status(&pool, &alice).await;
        // Insert out of order: positions shuffled against id order, and
        // the second status gets a position tie broken by id.
        let late = insert(
            &pool,
            &NewMediaAttachment {
                status_id: Some(first.id),
                position: 2,
                ..new_upload(alice.id)
            },
        )
        .await
        .unwrap();
        let early = insert(
            &pool,
            &NewMediaAttachment {
                status_id: Some(first.id),
                position: 0,
                ..new_upload(alice.id)
            },
        )
        .await
        .unwrap();
        let middle = insert(
            &pool,
            &NewMediaAttachment {
                status_id: Some(first.id),
                position: 1,
                ..new_upload(alice.id)
            },
        )
        .await
        .unwrap();
        let tie_b = insert(
            &pool,
            &NewMediaAttachment {
                status_id: Some(second.id),
                position: 1,
                ..new_upload(alice.id)
            },
        )
        .await
        .unwrap();
        let tie_a = insert(
            &pool,
            &NewMediaAttachment {
                status_id: Some(second.id),
                position: 1,
                ..new_upload(alice.id)
            },
        )
        .await
        .unwrap();

        let grouped = for_statuses(&pool, &[first.id, second.id, new_id()])
            .await
            .unwrap();
        assert_eq!(grouped.len(), 2, "the status with no media has no key");
        let positions: Vec<i32> = grouped[&first.id].iter().map(|row| row.position).collect();
        assert_eq!(positions, vec![0, 1, 2], "the group is ordered by position");
        assert_eq!(
            grouped[&first.id]
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![early.id, middle.id, late.id],
            "each row is the right one"
        );
        let mut tied = vec![tie_a.id, tie_b.id];
        tied.sort_unstable();
        assert_eq!(
            grouped[&second.id]
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            tied,
            "a position tie falls back to id order"
        );
        assert!(
            grouped
                .values()
                .all(|rows| rows.iter().all(|row| row.user_id == Some(alice.id))),
            "no group mixes statuses"
        );
        assert!(
            for_statuses(&pool, &[]).await.unwrap().is_empty(),
            "no statuses, no groups"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn get_own_unattached_serves_only_the_caller_s_unposted_media(pool: PgPool) {
        let alice = create_user(&pool, "alice").await;
        let bob = create_user(&pool, "bob").await;
        let mine = insert(&pool, &new_upload(alice.id)).await.unwrap();
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
        let ownerless = insert(
            &pool,
            &NewMediaAttachment {
                user_id: None,
                ..new_upload(alice.id)
            },
        )
        .await
        .unwrap();

        let found = get_own_unattached(&pool, mine.id, alice.id)
            .await
            .unwrap()
            .expect("alice's own unattached media reads back");
        assert_eq!(found.id, mine.id, "and it is the row named");

        for nope in [new_id(), attached.id, foreign.id, ownerless.id] {
            assert!(
                get_own_unattached(&pool, nope, alice.id)
                    .await
                    .unwrap()
                    .is_none(),
                "`{nope}` is no readable media for alice: unknown, attached, \
                 foreign, or ownerless alike"
            );
        }
        assert!(
            get_own_unattached(&pool, mine.id, bob.id)
                .await
                .unwrap()
                .is_none(),
            "bob cannot read alice's media either"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn update_own_metadata_writes_all_four_columns(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn update_own_metadata_only_touches_own_unattached_media(pool: PgPool) {
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

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn delete_removes_the_row_once(pool: PgPool) {
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
