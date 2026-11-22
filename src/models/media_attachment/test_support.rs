//! Fixtures the media attachment model's unit tests share.

use sqlx::PgPool;
use uuid::Uuid;

use super::{MediaAttachment, NewMediaAttachment, inserts::insert, reads::get_by_id};
use crate::models::{
    status::{self, NewStatus, Status},
    user::{NewUser, User, insert as insert_user, local_ap_id},
};

/// The instance origin the fixtures pretend to serve.
pub(super) const BASE: &str = "https://test.example";

/// Inserts a local user named `nickname` for the ownership tests.
pub(super) async fn create_user(pool: &PgPool, nickname: &str) -> User {
    insert_user(
        pool,
        &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
    )
    .await
    .expect("fixture user inserts")
}

/// Inserts a local status of `author`, because `status_id` is a foreign
/// key and so every fixture that sets one needs a stored row.
pub(super) async fn create_status(pool: &PgPool, author: &User) -> Status {
    status::insert(pool, BASE, &NewStatus::local(author, "a posted photo"))
        .await
        .expect("fixture status inserts")
}

/// Deletes `status` for real, which is what takes its media with it.
pub(super) async fn hard_delete(pool: &PgPool, status: Status) {
    let removed = sqlx::query("DELETE FROM statuses WHERE id = $1")
        .bind(status.id)
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(removed.rows_affected(), 1, "the fixture status existed");
}

/// A [`NewMediaAttachment`] uploaded by `user_id` and stored at a key
/// under [`BASE`], with nothing else known about the file.
pub(super) fn new_upload(user_id: Uuid) -> NewMediaAttachment {
    NewMediaAttachment {
        user_id: Some(user_id),
        url: format!("{BASE}/media/{user_id}/9b3c1d0e4f5a6b7c.png"),
        storage_key: Some(format!("{user_id}/9b3c1d0e4f5a6b7c.png")),
        content_type: "image/png".to_owned(),
        ..NewMediaAttachment::default()
    }
}

/// Reads the row behind `id` back out of the table, panicking if absent.
pub(super) async fn fetch(pool: &PgPool, id: Uuid) -> MediaAttachment {
    get_by_id(pool, id).await.unwrap().expect("row exists")
}

/// A [`NewMediaAttachment`] of `user_id` with the given `content_type`.
pub(super) fn upload_of(user_id: Uuid, content_type: &str) -> NewMediaAttachment {
    NewMediaAttachment {
        content_type: content_type.to_owned(),
        ..new_upload(user_id)
    }
}

/// An upload of `owner` attached to `status` at `position`, inserted
/// through the model so the group reads the very row back.
pub(super) async fn attach(
    pool: &PgPool,
    owner: &User,
    status: &Status,
    position: i32,
) -> MediaAttachment {
    insert(
        pool,
        &NewMediaAttachment {
            status_id: Some(status.id),
            position,
            ..new_upload(owner.id)
        },
    )
    .await
    .unwrap()
}
