//! The relationship tellings of [`crate::services::notify`]: who is told
//! when a follow lands, when a follow request is answered, and when an
//! account moves. The shared creation plumbing these calls live in the
//! parent module.

use tracing::warn;

use crate::{
    error::AppResult,
    models::{
        follow::{Follow, FollowState},
        notification::{TYPE_FOLLOW, TYPE_FOLLOW_REQUEST, TYPE_MOVE},
        user::User,
    },
    state::AppState,
};

use super::{forget, tell_once};

/// The follow endpoint's notification, landed from `after_follow_changed`
/// in [`crate::api::accounts`].
///
/// `following` and `follow` say what the write did, and only a new follow
/// tells: a withdrawal takes no telling back, Pleroma's
/// `Notification.create_notifications` never seeing an `Undo` of a
/// `Follow`. A pair the endpoint re-found open is told only once
/// (Pleroma's `recently_followed` skip).
pub async fn on_follow(
    state: &AppState,
    follower: &User,
    target: &User,
    follow: Option<&Follow>,
    following: bool,
) {
    if let Err(err) = follow_telling(state, follower, target, follow, following).await {
        warn!(
            followee = %target.id,
            follower = %follower.id,
            %err,
            "could not tell the followee about a follow"
        );
    }
}

/// [`on_follow`]'s work: the type is the relationship's state — an
/// accepted follow tells `follow`, a locked account's waiting request
/// tells `follow_request`, and a `rejected` row (only a mirrored state,
/// never one these endpoints leave behind) tells nothing.
async fn follow_telling(
    state: &AppState,
    follower: &User,
    target: &User,
    follow: Option<&Follow>,
    following: bool,
) -> AppResult<()> {
    if !following {
        return Ok(());
    }
    let Some(row) = follow else {
        return Ok(());
    };
    let event_type = match row.state {
        FollowState::Accepted => TYPE_FOLLOW,
        FollowState::Pending => TYPE_FOLLOW_REQUEST,
        FollowState::Rejected => return Ok(()),
    };
    tell_once(state, target, follower.id, event_type).await
}

/// The follow-request answer's notification, landed from
/// `after_follow_request_resolved` in [`crate::api::follow_requests`].
///
/// An **accepted** request replaces its own telling — the
/// [`TYPE_FOLLOW_REQUEST`] row the request created is deleted and a
/// `follow` row created in its place (Pleroma's grant runs the accepted
/// `Follow` through its pipeline exactly this way); a rejection deletes
/// nothing and tells nothing, and the request's row stands until its
/// recipient clears it.
pub async fn on_follow_request_resolved(
    state: &AppState,
    resolver: &User,
    request: &Follow,
    granted: bool,
) {
    if let Err(err) = resolution_telling(state, resolver, request, granted).await {
        warn!(
            user = %resolver.id,
            follower = %request.follower_id,
            %err,
            "could not tell a followee about an answered follow request"
        );
    }
}

/// [`on_follow_request_resolved`]'s work.
async fn resolution_telling(
    state: &AppState,
    resolver: &User,
    request: &Follow,
    granted: bool,
) -> AppResult<()> {
    if !granted {
        return Ok(());
    }
    forget(
        state,
        resolver.id,
        request.follower_id,
        TYPE_FOLLOW_REQUEST,
        None,
    )
    .await?;
    tell_once(state, resolver, request.follower_id, TYPE_FOLLOW).await
}

/// The inbound `Move` handler's telling, run by
/// [`crate::federation::handlers::misc`] for every local follower whose
/// relationship the migration was applied to.
///
/// The follower is told `move` with the **old** account as actor (Mastodon's
/// `move` notification: the account they followed declared a new one, and
/// this instance moved them). The migration's new follow tells no `follow`:
/// the endpoint's [`on_follow`] audience is the client API's follows, and a
/// migrated relationship is the move itself, which this one row reports.
/// Deduplicated on
/// [`notification::already_told`](crate::models::notification::already_told)
/// through this module's `tell_once`: a `Move` redelivered after the
/// ledger's fortnight expires tells an already-told follower nothing new.
pub async fn on_move(state: &AppState, follower: &User, moved: &User) {
    if let Err(err) = tell_once(state, follower, moved.id, TYPE_MOVE).await {
        warn!(
            follower = %follower.id,
            moved = %moved.id,
            %err,
            "could not tell a follower about an account's move"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::follow,
        services::notify::test_support::{app_state, facts, followed, local, locked, remote, rows},
    };

    #[tokio::test]
    async fn an_accepted_follow_notifies_the_local_followee_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let follower = local(pool, "nf-follower").await;
        let target = local(pool, "nf-target").await;

        let created = followed(pool, &follower, &target).await;
        assert_eq!(
            created.state,
            FollowState::Accepted,
            "the fixture is granted"
        );
        on_follow(&state, &follower, &target, Some(&created), true).await;
        assert_eq!(
            facts(pool, target.id).await,
            vec![(TYPE_FOLLOW.to_owned(), follower.id, None)],
            "the follow lands as one status-less `follow`"
        );

        // The endpoint re-fires the hook for a pair already open; the
        // deduplication keeps the second call silent.
        on_follow(&state, &follower, &target, Some(&created), true).await;
        assert_eq!(
            facts(pool, target.id).await.len(),
            1,
            "a re-follow of an open pair tells once"
        );

        // The withdrawal tells nothing new and takes nothing back.
        on_follow(&state, &follower, &target, Some(&created), false).await;
        assert_eq!(
            facts(pool, target.id).await.len(),
            1,
            "an unfollow leaves the telling alone"
        );
    }

    #[tokio::test]
    async fn a_pending_follow_notifies_a_locked_account_with_a_request() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let follower = local(pool, "rq-follower").await;
        let target = locked(pool, "rq-locked").await;

        let created = followed(pool, &follower, &target).await;
        assert_eq!(
            created.state,
            FollowState::Pending,
            "the fixture is a waiting request"
        );
        on_follow(&state, &follower, &target, Some(&created), true).await;
        assert_eq!(
            facts(pool, target.id).await,
            vec![(TYPE_FOLLOW_REQUEST.to_owned(), follower.id, None)],
            "the waiting pair is told as a follow request"
        );
    }

    #[tokio::test]
    async fn a_remote_followee_is_told_by_nobody_from_here() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let follower = local(pool, "rf-follower").await;
        let target = remote(pool, "rf-remote", "remote.example").await;

        let created = followed(pool, &follower, &target).await;
        on_follow(&state, &follower, &target, Some(&created), true).await;
        assert!(
            rows(pool, target.id).await.is_empty(),
            "a remote target is the remote instance's business, not ours"
        );
    }

    #[tokio::test]
    async fn an_accepted_request_becomes_a_follow_telling() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let follower = local(pool, "ar-follower").await;
        let resolver = locked(pool, "ar-locked").await;

        let request = followed(pool, &follower, &resolver).await;
        on_follow(&state, &follower, &resolver, Some(&request), true).await;
        let granted = follow::accept(pool, request.id)
            .await
            .expect("the accept runs");
        on_follow_request_resolved(&state, &resolver, &granted, true).await;
        assert_eq!(
            facts(pool, resolver.id).await,
            vec![(TYPE_FOLLOW.to_owned(), follower.id, None)],
            "the request telling is replaced by the follow one"
        );

        // A rejection replaces nothing: the request row stands.
        let other = local(pool, "ar-other").await;
        let second = followed(pool, &other, &resolver).await;
        on_follow(&state, &other, &resolver, Some(&second), true).await;
        on_follow_request_resolved(&state, &resolver, &second, false).await;
        assert_eq!(
            facts(pool, resolver.id).await,
            vec![
                (TYPE_FOLLOW_REQUEST.to_owned(), other.id, None),
                (TYPE_FOLLOW.to_owned(), follower.id, None),
            ],
            "rejecting leaves the request telling where it was"
        );
    }
}
