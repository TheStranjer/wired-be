//! The row types of `follows` and the pure helper that decides what a new
//! relationship holds: the state [`follow`](super::follow) starts it in
//! ([`initial_state`]).

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::models::user::User;

/// Lifecycle state of a [`Follow`] row, stored in the `state` text column as
/// its lowercase variant name (`pending`, `accepted` or `rejected`, which is
/// exactly what the column's `CHECK` constraint allows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum FollowState {
    /// Awaiting an answer: the followee is locked or remote and has not
    /// granted the follow yet.
    Pending,
    /// Granted: the relationship counts, feeds timelines and moves the
    /// denormalised counters.
    Accepted,
    /// Answered with a `Reject`. [`reject`](super::reject) deletes the row rather than
    /// storing this state, so a `rejected` row only appears when a remote
    /// instance's state was mirrored; [`follow`](super::follow) reopens one.
    Rejected,
}

/// One row of `follows`: the relationship between two users (decision 6).
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Follow {
    /// `UUIDv7` primary key, minted by [`follow`](super::follow) (decision 4).
    pub id: Uuid,
    /// The user who follows.
    pub follower_id: Uuid,
    /// The user who is followed.
    pub followee_id: Uuid,
    /// Whether the follow has been granted, is waiting, or was rejected.
    pub state: FollowState,
    /// `ActivityPub` id of the `Follow` activity behind this relationship: a
    /// `{BASE_URL}/activities/{uuid}` id for a local follower, the remote
    /// id for a mirrored one, `None` until an activity exists to record.
    pub ap_id: Option<String>,
    /// Whether the followee keeps seeing this follower's reblogs (Mastodon's
    /// `reblogs` toggle).
    pub show_reblogs: bool,
    /// Whether the followee is notified of this follower's posts (Mastodon's
    /// `notify` toggle).
    pub notify: bool,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every update here.
    pub updated_at: DateTime<Utc>,
}

/// One entry of a paged relationship list: the [`User`] on the other end of
/// the follow plus [`Follow::id`], the id of the row that put them on the
/// page.
///
/// The relationship id — not the user's — is what [`followers_page`](super::followers_page)
/// and [`following_page`](super::following_page) keyset on (decision 4), so the account listings'
/// `max_id`/`min_id` cursors and `Link` header carry follow row ids.
#[derive(Debug, sqlx::FromRow)]
pub struct FollowPageEntry {
    /// Primary key of the [`Follow`] row behind this entry.
    pub follow_id: Uuid,
    /// The user on the other end of the relationship.
    #[sqlx(flatten)]
    pub user: User,
}

/// The two per-follow toggles a client may name, as [`set_toggles`](super::set_toggles) stores
/// them.
///
/// Each side is optional in the sense Mastodon's `follow` endpoint means: a
/// parameter the request carried is written, one it left out keeps whatever the
/// row already holds — which is why these are `Option`s rather than the
/// [`Follow`] row's plain `bool`s.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FollowToggles {
    /// Mastodon's `reblogs`, the [`Follow::show_reblogs`] column; `None`
    /// leaves the stored flag alone.
    pub show_reblogs: Option<bool>,
    /// Mastodon's `notify`, the [`Follow::notify`] column; `None` leaves the
    /// stored flag alone.
    pub notify: Option<bool>,
}

impl FollowToggles {
    /// Whether the request named neither toggle, the case in which
    /// [`set_toggles`](super::set_toggles) has nothing to write and its caller skips the
    /// statement.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.show_reblogs.is_none() && self.notify.is_none()
    }
}

/// The state [`follow`](super::follow) starts a new relationship in: this instance can grant
/// a follow of an account it owns unless that account is locked, and anything
/// else has to wait for the remote answer (Pleroma's `maybe_direct_follow`
/// and `locked` check in `lib/pleroma/user.ex`).
pub(super) const fn initial_state(target: &User) -> FollowState {
    if target.local.get() && !target.locked.get() {
        FollowState::Accepted
    } else {
        FollowState::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::follow::test_support::*;

    #[tokio::test]
    async fn initial_state_classifies_the_target() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let carol = locked_user(&pool, "carol").await;
        let bob = remote_user(&pool, "bob").await;

        assert_eq!(
            initial_state(&alice),
            FollowState::Accepted,
            "an unlocked account this instance owns is answered at once"
        );
        assert_eq!(
            initial_state(&carol),
            FollowState::Pending,
            "a locked account answers in its own time"
        );
        assert_eq!(
            initial_state(&bob),
            FollowState::Pending,
            "a remote account cannot be answered here at all"
        );
    }
}
