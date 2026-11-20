//! The `follows` table: which user follows which, and in which state.
//!
//! Per `docs/ARCHITECTURE.md` decision 6 a follow is one of the relational
//! facts that federate, so a row carries the `ActivityPub` id of the `Follow`
//! activity behind it: `{BASE_URL}/activities/{uuid}` for a follower this
//! instance owns, and whatever a remote instance sent for a mirrored one
//! (NULL until an activity has been seen). The state machine follows Pleroma
//! (`lib/pleroma/following_relationship.ex`, `follow/3` and `unfollow/2` in
//! `lib/pleroma/user.ex`): a follow is granted on the spot when this instance
//! owns an unlocked followee, and otherwise waits in
//! [`FollowState::Pending`] for that followee to [`accept`] or [`reject`] it.
//!
//! `users.followers_count` and `users.following_count` are kept in step with
//! the accepted rows through
//! [`update_counts`](crate::models::user::update_counts): one step up when a
//! relationship becomes accepted, one step down when an accepted relationship
//! goes away. [`follow`], [`accept`], [`unfollow`], [`reject`],
//! [`remove_follower`] and [`delete_all_for_user_with_counts`] are the only
//! ways either happens, and each moves the counters exactly once per
//! transition.
//!
//! Signatures name the two ends of a relationship `follower` and `target`
//! (the word Pleroma's `subscribe(subscriber, target)` and Mastodon's
//! `target_account` use for the followed account), because `follower` next to
//! `followee` is one Clippy `similar_names` error; the row keeps the table's
//! own `follower_id` and `followee_id` names.
//!
//! The two per-follow toggles the row carries are read by
//! [`crate::models::relationship`] for the
//! [`Relationship`](crate::entities::relationship::Relationship) entity.
//! Pleroma's `subscribe` / `unsubscribe` write `notify` through
//! [`set_notify`] (a granted follow alone answers it); Mastodon's
//! `follow?reblogs` / `follow?notify` write either column through
//! [`set_toggles`], which is the pair-wide counterpart of the same two
//! flags and the only writer of `show_reblogs`.

mod audience;
mod counters;
mod delete;
mod familiar;
mod guards;
mod lookup;
mod pages;
mod pending;
mod state_accept;
mod state_remove;
mod state_toggles;
mod types;
mod unfollow;
mod write;

#[cfg(test)]
mod test_support;

pub use audience::{
    accepted_local_follower_ids, move_followers_of, remote_followees, remote_followers, subscribers,
};
pub use delete::{delete_all_for_user, delete_all_for_user_with_counts};
pub use familiar::familiar_followers;
pub use lookup::{
    find_by_ap_id, following_ids, get, get_by_ap_id, has_accepted_follow, has_local_followers,
    is_following, set_ap_id,
};
pub use pages::{followers_page, following_page};
pub use pending::{outgoing_requests_from, pending_count, pending_requests_for};
pub use state_accept::{accept, reject};
pub use state_remove::{remove_follower, sever_between};
pub use state_toggles::{set_notify, set_toggles};
pub use types::{Follow, FollowPageEntry, FollowState, FollowToggles};
pub use unfollow::unfollow;
pub use write::follow;
