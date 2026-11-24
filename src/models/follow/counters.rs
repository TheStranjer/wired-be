//! The two denormalised counters one accepted relationship feeds: the
//! followee's `followers_count` and the follower's `following_count`, moved
//! only through [`counted`] (a transition into accepted) and
//! [`step_counters`] (every step). Both run on the caller's connection, so the
//! row change and its counter steps commit together in the caller's
//! transaction.

use sqlx::PgConnection;

use crate::{
    error::AppResult,
    models::user::{CountDelta, update_counts},
};

use super::types::{Follow, FollowState};

/// Returns `follow`, having counted it on both users when it is an acceptance
/// this very call brought about.
pub(super) async fn counted(conn: &mut PgConnection, follow: Follow) -> AppResult<Follow> {
    if follow.state == FollowState::Accepted {
        step_counters(conn, &follow, 1).await?;
    }
    Ok(follow)
}

/// Adds `step` to the two denormalised counters one relationship feeds: the
/// followee's `followers_count` and the follower's `following_count`, on the
/// open transaction `conn`.
pub(super) async fn step_counters(
    conn: &mut PgConnection,
    follow: &Follow,
    step: i32,
) -> AppResult<()> {
    update_counts(
        &mut *conn,
        follow.followee_id,
        CountDelta {
            followers: step,
            ..CountDelta::default()
        },
    )
    .await?;
    update_counts(
        &mut *conn,
        follow.follower_id,
        CountDelta {
            following: step,
            ..CountDelta::default()
        },
    )
    .await
}
