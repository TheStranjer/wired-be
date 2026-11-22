//! The `usage` block of the `NodeInfo` document and the database reads
//! behind it and behind the staff account list.

use serde::Serialize;
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::{error::AppResult, models::user::NOT_INSTANCE_ACTOR};

/// The `usage` block.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Usage {
    /// The user counters.
    pub(super) users: UserCounts,
    /// Posts this instance owns.
    pub(super) local_posts: i64,
}

/// The `usage.users` block: the row [`read_user_counts`] reads, the field
/// names renamed for the schema.
#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub(super) struct UserCounts {
    /// Local accounts still active.
    pub(super) total: i64,
    /// Of those, the ones seen in the last 30 days.
    pub(super) active_month: i64,
    /// Of those, the ones seen in the last 180 days.
    pub(super) active_halfyear: i64,
}

/// The `usage.users` counters: every active local account counts toward
/// `total`, and the two activity windows count the accounts whose
/// `last_active_at` — falling back to their newest post's time, as Pleroma
/// treats "active" — is inside them. The instance actor never counts: the
/// shared [`NOT_INSTANCE_ACTOR`] fragment closes the predicate.
///
/// [`NOT_INSTANCE_ACTOR`]: crate::models::user::NOT_INSTANCE_ACTOR
const USER_COUNTS_SQL: &str = "SELECT
       count(*) AS total,
       count(*) FILTER (
           WHERE coalesce(last_active_at, last_status_at) >= now() - interval '30 days'
       ) AS active_month,
       count(*) FILTER (
           WHERE coalesce(last_active_at, last_status_at) >= now() - interval '180 days'
       ) AS active_halfyear
   FROM users
   WHERE local AND is_active";

/// The `usage.localPosts` counter: posts this instance owns, live (soft
/// deletes excluded) and not reblogs — a reblog shares its original's
/// post, so counting both would double it (Pleroma's `Stats` sums the
/// local `note_count`, which excludes announces the same way).
const LOCAL_POSTS_SQL: &str = "SELECT count(*) FROM statuses
   WHERE local AND deleted_at IS NULL AND reblog_of_id IS NULL";

/// The `metadata.staffAccounts` list: actor ids of the local staff
/// accounts, ordered by id text so the document is stable (Pleroma's
/// `User.all_superusers/0`).
const STAFF_ACCOUNTS_SQL: &str = "SELECT ap_id FROM users
   WHERE (is_admin OR is_moderator) AND local AND is_active
ORDER BY ap_id";

/// The [`usage`](NodeInfo::usage) user counters, read live. The builder
/// exists only to splice the shared [`NOT_INSTANCE_ACTOR`] fragment onto
/// the constant [`USER_COUNTS_SQL`] — no request data enters the text.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError::Database
pub(super) async fn read_user_counts(pool: &PgPool) -> AppResult<UserCounts> {
    let mut builder = QueryBuilder::<Postgres>::new(USER_COUNTS_SQL);
    let _exclude: &mut _ = builder.push(NOT_INSTANCE_ACTOR);
    let counts = builder
        .build_query_as::<UserCounts>()
        .fetch_one(pool)
        .await?;
    Ok(counts)
}

/// The [`usage`](NodeInfo::usage) post counter, read live.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError::Database
pub(super) async fn read_local_post_count(pool: &PgPool) -> AppResult<i64> {
    let (count,) = sqlx::query_as::<_, (i64,)>(LOCAL_POSTS_SQL)
        .fetch_one(pool)
        .await?;
    Ok(count)
}

/// The [`metadata`](NodeInfo::metadata) staff actor ids, read live.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError::Database
pub(super) async fn read_staff_accounts(pool: &PgPool) -> AppResult<Vec<String>> {
    let accounts = sqlx::query_scalar::<_, String>(STAFF_ACCOUNTS_SQL)
        .fetch_all(pool)
        .await?;
    Ok(accounts)
}
