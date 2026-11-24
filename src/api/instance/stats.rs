//! The instance counters, their one SQL text and the cache in front of them.
//!
//! `GET /api/v1/instance` publishes them as its `stats` block and
//! `GET /nodeinfo/{version}` as its `usage` block. Both read through
//! [`stats`], so the two endpoints cannot drift apart and an unauthenticated,
//! frequently crawled endpoint costs the database one aggregate per
//! [`STATS_TTL`].

use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::{
    error::AppResult, models::user::NOT_INSTANCE_ACTOR, state::AppState, util::cache::TimedCache,
};

/// How long the counters may be served from the in-memory cache.
///
/// Pleroma's `Pleroma.Stats` `GenServer` recalculates on the same
/// 60-second interval (`lib/pleroma/stats.ex`).
pub const STATS_TTL: TimeDelta = TimeDelta::seconds(60);

/// The [`crate::state::AppState`] slot caching the counters; `Default`
/// builds it with [`STATS_TTL`].
pub type InstanceStatsCache = TimedCache<InstanceStats>;

impl Default for TimedCache<InstanceStats> {
    /// An empty stats cache with the counters' [`STATS_TTL`] window, so
    /// every [`AppState`] constructor writes the same `Arc::default()`.
    fn default() -> Self {
        Self::new(STATS_TTL)
    }
}

/// The counters of a Pleroma-compatible instance, read in one round trip
/// and cached for [`STATS_TTL`].
///
/// The field names drop the shared `count` postfix for Clippy's
/// `struct_field_names`; `serde` and `sqlx` restore it on the wire and in
/// the query, so both sides still speak Mastodon's
/// `user_count`/`status_count`/`domain_count`. The two activity windows
/// are `NodeInfo`'s alone and stay out of the `/api/v1/instance` document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct InstanceStats {
    /// Accounts this instance owns that are still active.
    #[serde(rename = "user_count")]
    #[sqlx(rename = "user_count")]
    pub users: i64,
    /// Live, non-reblog posts this instance owns.
    #[serde(rename = "status_count")]
    #[sqlx(rename = "status_count")]
    pub statuses: i64,
    /// Instances whose users are mirrored here.
    #[serde(rename = "domain_count")]
    #[sqlx(rename = "domain_count")]
    pub domains: i64,
    /// Of the active local accounts, the ones seen in the last 30 days.
    #[serde(skip)]
    pub active_month: i64,
    /// Of the active local accounts, the ones seen in the last 180 days.
    #[serde(skip)]
    pub active_halfyear: i64,
}

/// [`read_stats`]: the counters in one query. `user_count` counts active
/// local accounts, and the two activity windows count those whose
/// `last_active_at` — falling back to their newest post's time, as Pleroma
/// treats "active" — is inside them. `status_count` is the live non-reblog
/// posts this instance owns (a reblog shares its original's post, so
/// counting both would double it), and `domain_count` the distinct hosts of
/// the remote actors mirrored here. The users subquery is closed with the
/// shared [`NOT_INSTANCE_ACTOR`] fragment: the instance actor is a local
/// active account and belongs in no public statistic.
///
/// [`NOT_INSTANCE_ACTOR`]: crate::models::user::NOT_INSTANCE_ACTOR
const STATS_SQL_LOCAL: &str = "SELECT
       u.total AS user_count,
       u.active_month,
       u.active_halfyear,
       (SELECT count(*) FROM statuses
         WHERE local AND deleted_at IS NULL AND reblog_of_id IS NULL) AS status_count,
       (SELECT count(DISTINCT domain) FROM users
         WHERE NOT local AND domain IS NOT NULL) AS domain_count
   FROM (SELECT
           count(*) AS total,
           count(*) FILTER (
               WHERE coalesce(last_active_at, last_status_at) >= now() - interval '30 days'
           ) AS active_month,
           count(*) FILTER (
               WHERE coalesce(last_active_at, last_status_at) >= now() - interval '180 days'
           ) AS active_halfyear
         FROM users
         WHERE local AND is_active";
/// The rest of [`STATS_SQL_LOCAL`], pushed after the instance-actor
/// exclusion (see [`read_stats`]).
const STATS_SQL_REST: &str = ") u";

/// The counters, read live.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError::Database
async fn read_stats(pool: &PgPool) -> AppResult<InstanceStats> {
    let mut builder = QueryBuilder::<Postgres>::new(STATS_SQL_LOCAL);
    let _exclude: &mut _ = builder.push(NOT_INSTANCE_ACTOR);
    let _rest: &mut _ = builder.push(STATS_SQL_REST);
    let stats = builder
        .build_query_as::<InstanceStats>()
        .fetch_one(pool)
        .await?;
    Ok(stats)
}

/// The instance counters as of the present, from the in-memory cache while
/// its [`STATS_TTL`] window is open and from the database otherwise.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError::Database
pub async fn stats(state: &AppState) -> AppResult<InstanceStats> {
    stats_at(state, Utc::now()).await
}

/// [`stats`] with the clock injected: the cached counters when `now` is
/// inside the window, a fresh read (stored under `now`) otherwise.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
///
/// [`AppError::Database`]: crate::error::AppError::Database
pub async fn stats_at(state: &AppState, now: DateTime<Utc>) -> AppResult<InstanceStats> {
    if let Some(cached) = state.instance_stats_cache.get(now) {
        return Ok(cached);
    }
    let fresh = read_stats(&state.db).await?;
    state.instance_stats_cache.insert(now, fresh);
    Ok(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        models::{
            status::{self, NewStatus},
            user::{NewUser, insert},
        },
        test_fixtures::{local_new, local_user},
    };

    #[tokio::test]
    async fn counters_are_cached_until_the_window_closes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = AppState::for_tests(pool.clone(), Config::for_tests());
        let now = Utc::now();
        let alice = local_user(&pool, "alice").await;

        let first = stats_at(&state, now).await.expect("the first read runs");
        assert_eq!(first.users, 1, "alice counts");
        assert_eq!(first.statuses, 0, "no posts yet");

        let _bob = local_user(&pool, "bob").await;
        let _post = status::insert(
            &pool,
            "https://test.example",
            &NewStatus::local(&alice, "late"),
        )
        .await
        .expect("the post inserts");

        let inside = stats_at(&state, now + TimeDelta::seconds(59))
            .await
            .expect("the cached read runs");
        assert_eq!(inside, first, "the open window does not see new rows");

        let after = stats_at(&state, now + STATS_TTL)
            .await
            .expect("the refreshed read runs");
        assert_eq!(after.users, 2, "the closed window reads bob");
        assert_eq!(after.statuses, 1, "and the post");
    }

    #[tokio::test]
    async fn activity_windows_fall_back_to_the_newest_post() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = AppState::for_tests(pool.clone(), Config::for_tests());
        let now = Utc::now();
        let _recent = insert(
            &pool,
            &NewUser {
                last_active_at: Some(now - TimeDelta::days(1)),
                ..local_new("recent")
            },
        )
        .await
        .expect("the recent user inserts");
        let _posted = insert(
            &pool,
            &NewUser {
                last_status_at: Some(now - TimeDelta::days(100)),
                ..local_new("posted")
            },
        )
        .await
        .expect("the posting user inserts");
        let _never = local_user(&pool, "never").await;

        let counted = stats_at(&state, now).await.expect("the read runs");
        assert_eq!(counted.users, 3, "every active local account");
        assert_eq!(counted.active_month, 1, "only the recently seen one");
        assert_eq!(counted.active_halfyear, 2, "plus the one who posted");
    }

    #[test]
    fn the_activity_windows_stay_out_of_the_instance_document() {
        let stats = InstanceStats {
            users: 1,
            statuses: 2,
            domains: 3,
            active_month: 4,
            active_halfyear: 5,
        };
        assert_eq!(
            serde_json::to_value(stats).unwrap(),
            serde_json::json!({ "user_count": 1, "status_count": 2, "domain_count": 3 }),
            "only Mastodon's three counters are serialized"
        );
    }
}
