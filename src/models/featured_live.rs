//! The `featured_live_marks` table: when a live `Add`/`Remove` last changed a
//! remote account's pins or featured tags.
//!
//! A peer may serve its featured collections from a cache (Mastodon's is
//! three minutes), so a collection read shortly after a live change can
//! still show the set as it was before it. The syncs ask [`is_recent`] and
//! keep the stored set when the answer is yes.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

/// How long after a live `Add`/`Remove` a sync of the same collection is
/// skipped: Mastodon's three-minute collection cache plus the debounce and
/// delivery slack.
pub const FEATURED_SYNC_GRACE: Duration = Duration::minutes(5);

/// The two featured collections a live change can touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collection {
    /// The pinned statuses (`featured`).
    Pins,
    /// The featured hashtags (`featuredTags`).
    Tags,
}

impl Collection {
    /// The value stored in the `collection` column.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pins => "pins",
            Self::Tags => "tags",
        }
    }
}

/// [`mark_at`]: one row per account and collection, the newest time wins.
const MARK_SQL: &str = "INSERT INTO featured_live_marks (user_id, collection, applied_at)
     VALUES ($1, $2, $3)
ON CONFLICT (user_id, collection) DO UPDATE SET applied_at = EXCLUDED.applied_at";

/// [`last_applied`]: the stored time, if any.
const LAST_SQL: &str =
    "SELECT applied_at FROM featured_live_marks WHERE user_id = $1 AND collection = $2";

/// Records a live change to `collection` of `user_id` applied now.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn mark(pool: &PgPool, user_id: Uuid, collection: Collection) -> AppResult<()> {
    mark_at(pool, user_id, collection, Utc::now()).await
}

/// Records a live change to `collection` of `user_id` applied at `at`.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn mark_at(
    pool: &PgPool,
    user_id: Uuid,
    collection: Collection,
    at: DateTime<Utc>,
) -> AppResult<()> {
    sqlx::query(MARK_SQL)
        .bind(user_id)
        .bind(collection.as_str())
        .bind(at)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// When the last live change to `collection` of `user_id` was applied.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn last_applied(
    pool: &PgPool,
    user_id: Uuid,
    collection: Collection,
) -> AppResult<Option<DateTime<Utc>>> {
    let at = sqlx::query_scalar(LAST_SQL)
        .bind(user_id)
        .bind(collection.as_str())
        .fetch_optional(pool)
        .await?;
    Ok(at)
}

/// Whether a live change to `collection` of `user_id` landed within
/// [`FEATURED_SYNC_GRACE`] before `now`.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn is_recent_at(
    pool: &PgPool,
    user_id: Uuid,
    collection: Collection,
    now: DateTime<Utc>,
) -> AppResult<bool> {
    let last = last_applied(pool, user_id, collection).await?;
    Ok(last.is_some_and(|at| now.signed_duration_since(at) < FEATURED_SYNC_GRACE))
}

/// [`is_recent_at`] for the current time.
///
/// A failed read counts as not recent, so a database hiccup never blocks the
/// healing sync.
pub async fn is_recent(pool: &PgPool, user_id: Uuid, collection: Collection) -> bool {
    is_recent_at(pool, user_id, collection, Utc::now())
        .await
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::local_user;

    /// A mark inside the grace is recent, one past it is not, an unmarked
    /// collection is not, and the two collections are independent.
    #[tokio::test]
    async fn a_mark_is_recent_only_inside_the_grace_and_per_collection() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = local_user(&pool, "alice").await;
        let now = Utc::now();

        assert!(
            !is_recent_at(&pool, user.id, Collection::Pins, now)
                .await
                .unwrap()
        );

        mark_at(&pool, user.id, Collection::Pins, now - Duration::minutes(1))
            .await
            .unwrap();
        assert!(
            is_recent_at(&pool, user.id, Collection::Pins, now)
                .await
                .unwrap()
        );
        assert!(
            !is_recent_at(&pool, user.id, Collection::Tags, now)
                .await
                .unwrap()
        );

        mark_at(&pool, user.id, Collection::Pins, now - Duration::minutes(6))
            .await
            .unwrap();
        assert!(
            !is_recent_at(&pool, user.id, Collection::Pins, now)
                .await
                .unwrap()
        );

        mark(&pool, user.id, Collection::Pins).await.unwrap();
        assert!(is_recent(&pool, user.id, Collection::Pins).await);
        assert!(
            last_applied(&pool, user.id, Collection::Pins)
                .await
                .unwrap()
                .is_some()
        );
    }
}
