//! Folding the loaded flag sources into the entity for one target.

use uuid::Uuid;

use super::sources::FlagSources;
use crate::{
    entities::{account::JsonBool, relationship::Relationship},
    models::follow::FollowState,
};

/// Folds the loaded sources into the entity for one target: the six flags
/// the follows table answers, the [`note`] the notes table holds, the
/// `domain_blocking` the domain-block join decided, the `endorsed` the
/// viewer's endorsement set contains, the two directions of the blocks table
/// and the two sets of the mutes table take their sources' values, every
/// other flag keeps the default [`Relationship::unconnected`] states.
///
/// [`note`]: Relationship::note
pub(super) fn assemble(target_id: Uuid, sources: &FlagSources) -> Relationship {
    let follow = sources.outgoing.get(&target_id);
    let following = follow.is_some_and(|row| row.state == FollowState::Accepted);
    let requested = !following && follow.is_some_and(|row| row.state == FollowState::Pending);
    let notifying = follow.is_some_and(|row| row.notify);
    Relationship {
        following: JsonBool(following),
        requested: JsonBool(requested),
        followed_by: JsonBool(sources.followed_by.contains(&target_id)),
        notifying: JsonBool(notifying),
        subscribing: JsonBool(notifying),
        showing_reblogs: JsonBool(follow.is_none_or(|row| row.show_reblogs)),
        domain_blocking: JsonBool(sources.domain_blocking.contains(&target_id)),
        endorsed: JsonBool(sources.endorsed.contains(&target_id)),
        blocking: JsonBool(sources.blocks.blocking.contains(&target_id)),
        blocked_by: JsonBool(sources.blocks.blocked_by.contains(&target_id)),
        muting: JsonBool(sources.mutes.muting.contains(&target_id)),
        muting_notifications: JsonBool(sources.mutes.muting_notifications.contains(&target_id)),
        note: sources.notes.get(&target_id).cloned().unwrap_or_default(),
        ..Relationship::unconnected(target_id)
    }
}
