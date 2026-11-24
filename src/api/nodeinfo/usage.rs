//! The `usage` block of the `NodeInfo` document — built from the cached
//! instance counters [`crate::api::instance::stats`] serves, the very
//! numbers `/api/v1/instance` publishes — and the database read behind the
//! staff account list.

use serde::Serialize;
use sqlx::PgPool;

use crate::{api::instance::InstanceStats, error::AppResult};

/// The `usage` block.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Usage {
    /// The user counters.
    pub(super) users: UserCounts,
    /// Posts this instance owns.
    pub(super) local_posts: i64,
}

impl From<&InstanceStats> for Usage {
    /// The block for `stats`: its active local accounts, their two
    /// activity windows and the live non-reblog posts this instance owns.
    fn from(stats: &InstanceStats) -> Self {
        Self {
            users: UserCounts {
                total: stats.users,
                active_month: stats.active_month,
                active_halfyear: stats.active_halfyear,
            },
            local_posts: stats.statuses,
        }
    }
}

/// The `usage.users` block, the field names renamed for the schema.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UserCounts {
    /// Local accounts still active.
    pub(super) total: i64,
    /// Of those, the ones seen in the last 30 days.
    pub(super) active_month: i64,
    /// Of those, the ones seen in the last 180 days.
    pub(super) active_halfyear: i64,
}

/// The `metadata.staffAccounts` list: actor ids of the local staff
/// accounts, ordered by id text so the document is stable (Pleroma's
/// `User.all_superusers/0`).
const STAFF_ACCOUNTS_SQL: &str = "SELECT ap_id FROM users
   WHERE (is_admin OR is_moderator) AND local AND is_active
ORDER BY ap_id";

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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn usage_mirrors_the_cached_instance_counters() {
        let stats = InstanceStats {
            users: 5,
            statuses: 3,
            domains: 9,
            active_month: 2,
            active_halfyear: 4,
        };
        assert_eq!(
            serde_json::to_value(Usage::from(&stats)).unwrap(),
            json!({
                "users": { "total": 5, "activeMonth": 2, "activeHalfyear": 4 },
                "localPosts": 3,
            }),
            "users from the active counters, posts from the status count"
        );
    }
}
