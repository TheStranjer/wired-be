//! The `ActivityPub` activity documents of this instance's relational
//! facts.
//!
//! Served at `{BASE_URL}/activities/{uuid}`, these are the `Like`,
//! `EmojiReact`, `Announce`, `Follow`, `Block`, `Undo`, `Accept` and
//! `Reject` behind the tables of `docs/ARCHITECTURE.md` decision 6 —
//! favourites, reactions, boosts, follows and blocks — other than notes,
//! plus the `Move`, `Flag` and `Add`/`Remove` behind the account
//! lifecycle and moderation writes, and the `Listen` of a scrobble with the
//! `Audio` object it wraps ([`listen_json`], [`audio_json`]).
//!
//! These are the counterparts of Pleroma's
//! `lib/pleroma/web/activity_pub/views/*_view.ex` renderers, and pure
//! groundwork in exactly the shape [`note`](crate::federation::note) sets
//! for statuses: every input rides in as a plain borrow or one data-only
//! struct ([`EmojiReactContext`], [`AnsweredActivity`]), every answer is a
//! [`serde_json::Value`], and nothing here asks the database — the row
//! reads that fill these inputs live in [`crate::models`]. The
//! `/activities/{uuid}` route and the outbound federation tickets are the
//! callers, and they are the reason each builder is named after its activity
//! type. `Create` lives in [`note`](crate::federation::note) and is not
//! duplicated here, and neither is a reblog row's `Announce`: its one
//! renderer, [`announce_activity_json`](crate::federation::note::announce_activity_json), lives in
//! [`note`](crate::federation::note) and serves the delivery, the object
//! route and the outbox alike.
//!
//! [`activity_id`] is the one minting rule: `{BASE_URL}/activities/{id}`,
//! the shape [`note`](crate::federation::note) stamps into a `Create` and
//! the shape [`favourite`](crate::models::favourite::favourite),
//! [`react`](crate::models::emoji_reaction::react),
//! [`follow`](crate::models::follow::follow) and
//! [`block`](crate::models::block::block) already mint into their rows'
//! `ap_id` columns. The outbound hooks hand each builder the row's stored
//! id string — the Like's id is the favourite row's `ap_id`, the Announce's
//! the reblog row's `uri` — so a peer's later lookup of what was delivered
//! finds exactly what it holds.
//!
//! The addressing is this instance's deliberate uniform convention, so the
//! outbound tickets have one shape to deliver against: every document
//! starts with the same JSON-LD `@context` the note builder writes, and
//! carries both `to` and `cc`, always present. The activity builders that
//! speak of one object — [`like_json`] and [`emoji_react_json`] — render
//! the `to` and `cc` their caller computed from
//! the interacted-with status (the outbound hooks in
//! [`crate::services::federation_out`] derive them; the C2S serving route
//! keeps this module's public-address convention), while the two-actor
//! activities [`follow_json`] and [`block_json`] name the other actor's id
//! in `to` and leave `cc` empty. [`undo_json`], [`accept_json`] and
//! [`reject_json`] render the addressing their [`AnsweredActivity`]
//! carries: the two follow answers name the follower, and the withdrawal
//! of an interaction repeats the addressing of the activity it embeds —
//! exactly who heard the fact hears its taking-back. The `target`-bearing
//! builders [`move_json`], [`add_json`] and [`remove_json`] and the
//! moderation builder [`flag_json`] likewise render the lines their caller
//! computed: the move names the origin's followers collection in `to`, the
//! pin the public address in `to` and the followers collection in `cc`,
//! and the forwarded flag the reported account alone.

use uuid::Uuid;

use crate::{config::Config, util::id::activity_ap_id};

mod contexts;
mod interactions;
mod listen;
mod relations;
#[cfg(test)]
mod test_support;

pub use contexts::{
    AnsweredActivity, EmojiReactContext, FlagContext, ObjectActivityContext,
    TargetedActivityContext,
};
pub use interactions::{emoji_react_json, like_json, undo_json};
pub use listen::{audio_json, listen_json};
pub use relations::{
    FollowActivity, accept_json, add_json, block_json, flag_json, follow_json, follow_json_to,
    move_json, reject_json, remove_json,
};

/// The activity type of a scrobble.
const TYPE_LISTEN: &str = "Listen";
/// The object type of a scrobble's media.
const TYPE_AUDIO: &str = "Audio";
/// The activity type of a favourite.
const TYPE_LIKE: &str = "Like";
/// The activity type of an emoji reaction.
const TYPE_EMOJI_REACT: &str = "EmojiReact";
/// The activity type of a follow request.
const TYPE_FOLLOW: &str = "Follow";
/// The activity type of a block.
const TYPE_BLOCK: &str = "Block";
/// The activity type of a withdrawn activity.
const TYPE_UNDO: &str = "Undo";
/// The activity type of a granted follow request.
const TYPE_ACCEPT: &str = "Accept";
/// The activity type of a refused follow request.
const TYPE_REJECT: &str = "Reject";
/// The activity type of an account move.
const TYPE_MOVE: &str = "Move";
/// The activity type of a forwarded report.
const TYPE_FLAG: &str = "Flag";
/// The activity type of a pin added to the featured collection.
const TYPE_ADD: &str = "Add";
/// The activity type of a pin withdrawn from the featured collection.
const TYPE_REMOVE: &str = "Remove";
/// The tag type of a custom emoji, as the note builder's `emoji_tag`
/// writes it.
const TYPE_EMOJI: &str = "Emoji";
/// The type of an emoji tag's `icon`.
const TYPE_IMAGE: &str = "Image";

/// The `ActivityPub` id this instance mints for the activity named `id`.
///
/// `{BASE_URL}/activities/{id}` (decision 6), the shape
/// [`note`](crate::federation::note) stamps into a `Create` and the shape
/// [`favourite`](crate::models::favourite::favourite),
/// [`react`](crate::models::emoji_reaction::react),
/// [`follow`](crate::models::follow::follow) and
/// [`block`](crate::models::block::block) already stamp into the rows'
/// `ap_id` columns; the outbound hooks deliver the row's stored string
/// itself, so the document carries the id the table keeps.
#[must_use]
pub fn activity_id(config: &Config, id: Uuid) -> String {
    activity_ap_id(&config.base_url, id)
}

#[cfg(test)]
mod tests {
    use super::test_support::{ACTIVITY_ID, BASE, activity_uuid, config};
    use super::*;

    #[test]
    fn activity_id_builds_the_documented_url() {
        assert_eq!(
            activity_id(&config(), activity_uuid()),
            format!("{BASE}/activities/{ACTIVITY_ID}"),
            "the shape decision 6 mints and the ap_id columns store"
        );
    }
}
