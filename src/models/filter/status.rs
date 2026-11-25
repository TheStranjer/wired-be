//! The `custom_filter_statuses` table: single statuses one v2 filter hides.
//!
//! Reference: Mastodon's `CustomFilterStatus`. A row names one status for one
//! filter; as with keywords, ownership is the filter's, and the owned reads
//! and writes join `custom_filters` on `user_id` so a foreign id reads as a
//! missing one.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

/// [`insert`]: one status added to a filter; a repeat adds nothing.
const INSERT_SQL: &str = "INSERT INTO custom_filter_statuses (id, filter_id, status_id)
     VALUES ($1, $2, $3)
     ON CONFLICT (filter_id, status_id) DO NOTHING
  RETURNING *";

/// [`for_filters`]: every status entry of the named filters, oldest first.
const FOR_FILTERS_SQL: &str =
    "SELECT * FROM custom_filter_statuses WHERE filter_id = ANY($1) ORDER BY id";

/// [`get_owned`]: one entry, only of its filter's owner.
const GET_OWNED_SQL: &str = "SELECT s.* FROM custom_filter_statuses s
     JOIN custom_filters f ON f.id = s.filter_id
     WHERE s.id = $1 AND f.user_id = $2";

/// [`delete_owned`]: one entry, only of its filter's owner.
const DELETE_OWNED_SQL: &str = "DELETE FROM custom_filter_statuses s
     USING custom_filters f
     WHERE s.filter_id = f.id AND s.id = $1 AND f.user_id = $2";

/// [`status_exists`]: whether a status row carries the id.
const STATUS_EXISTS_SQL: &str = "SELECT EXISTS (SELECT 1 FROM statuses WHERE id = $1)";

/// One row of `custom_filter_statuses`.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct FilterStatus {
    /// `UUIDv7` primary key.
    pub id: Uuid,
    /// The filter the entry belongs to.
    pub filter_id: Uuid,
    /// The status the filter matches whole.
    pub status_id: Uuid,
    /// When the row was created.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed.
    pub updated_at: DateTime<Utc>,
}

/// Adds `status_id` to the filter `filter_id`, minting the entry's id.
///
/// The caller has checked that the filter exists and who owns it. `None`
/// when the filter already holds that status.
///
/// # Errors
///
/// Any database failure (including a status id that names no status)
/// arrives as [`AppError::Database`](crate::error::AppError).
pub async fn insert(
    pool: &PgPool,
    filter_id: Uuid,
    status_id: Uuid,
) -> AppResult<Option<FilterStatus>> {
    let row = sqlx::query_as::<_, FilterStatus>(INSERT_SQL)
        .bind(new_id())
        .bind(filter_id)
        .bind(status_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Whether a status with the id `status_id` exists.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn status_exists(pool: &PgPool, status_id: Uuid) -> AppResult<bool> {
    let found = sqlx::query_scalar::<_, bool>(STATUS_EXISTS_SQL)
        .bind(status_id)
        .fetch_one(pool)
        .await?;
    Ok(found)
}

/// Every status entry of the filters `filter_ids`, oldest first.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn for_filters(pool: &PgPool, filter_ids: &[Uuid]) -> AppResult<Vec<FilterStatus>> {
    let rows = sqlx::query_as::<_, FilterStatus>(FOR_FILTERS_SQL)
        .bind(filter_ids)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The entry `id` if the owner of its filter is `user_id`; `None` for a
/// missing or foreign id.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_owned(pool: &PgPool, id: Uuid, user_id: Uuid) -> AppResult<Option<FilterStatus>> {
    let row = sqlx::query_as::<_, FilterStatus>(GET_OWNED_SQL)
        .bind(id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Deletes the entry `id` of `user_id`'s filter, reporting whether one
/// went.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_owned(pool: &PgPool, id: Uuid, user_id: Uuid) -> AppResult<bool> {
    let deleted = sqlx::query(DELETE_OWNED_SQL)
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() > 0)
}
