//! The two denormalised counters one accepted relationship feeds: the
//! followee's `followers_count` and the follower's `following_count`, moved
//! only through [`counted`] (a transition into accepted) and
//! [`step_counters`] (every step).

use sqlx::PgPool;

use crate::{
    error::AppResult,
    models::user::{CountDelta, update_counts},
};

use super::types::{Follow, FollowState};

/// Returns `follow`, having counted it on both users when it is an acceptance
/// this very call brought about.
pub(super) async fn counted(pool: &PgPool, follow: Follow) -> AppResult<Follow> {
    if follow.state == FollowState::Accepted {
        step_counters(pool, &follow, 1).await?;
    }
    Ok(follow)
}

/// Adds `step` to the two denormalised counters one relationship feeds: the
/// followee's `followers_count` and the follower's `following_count`.
pub(super) async fn step_counters(pool: &PgPool, follow: &Follow, step: i32) -> AppResult<()> {
    update_counts(
        pool,
        follow.followee_id,
        CountDelta {
            followers: step,
            ..CountDelta::default()
        },
    )
    .await?;
    update_counts(
        pool,
        follow.follower_id,
        CountDelta {
            following: step,
            ..CountDelta::default()
        },
    )
    .await
}
