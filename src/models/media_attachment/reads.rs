//! Reading media attachments back: by id, for a chat message, in bulk, by
//! status and the caller's own unattached uploads.

use std::collections::HashMap;

use sqlx::PgPool;
use uuid::Uuid;

use super::MediaAttachment;
use crate::error::AppResult;

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

#[cfg(test)]
mod tests {
    use super::super::{
        NewMediaAttachment,
        inserts::insert,
        test_support::{attach, create_status, create_user, new_upload, upload_of},
    };
    use super::*;
    use crate::util::id::new_id;

    #[tokio::test]
    async fn lookups_of_unknown_ids_are_empty() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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

    #[tokio::test]
    async fn chat_media_is_own_unattached_image_video_or_audio() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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

    #[tokio::test]
    async fn for_statuses_groups_by_status_and_orders_by_position() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let first = create_status(&pool, &alice).await;
        let second = create_status(&pool, &alice).await;
        // Insert out of order: positions shuffled against id order, and
        // the second status gets a position tie broken by id.
        let late = attach(&pool, &alice, &first, 2).await;
        let early = attach(&pool, &alice, &first, 0).await;
        let middle = attach(&pool, &alice, &first, 1).await;
        let tie_b = attach(&pool, &alice, &second, 1).await;
        let tie_a = attach(&pool, &alice, &second, 1).await;

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

    #[tokio::test]
    async fn get_own_unattached_serves_only_the_caller_s_unposted_media() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
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
}
