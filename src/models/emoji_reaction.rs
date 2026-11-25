//! The `emoji_reactions` table: one account reacting to one status with one
//! emoji.
//!
//! Reference: Pleroma's `EmojiReact` activities
//! (`lib/pleroma/web/common_api.ex`, `react_with_emoji` and
//! `unreact_with_emoji`), which it stores as an activity per reaction plus an
//! `Undo` per withdrawal and reads back out of the reacted-to object's
//! `reactions` JSONB (`Object.get_emoji_reactions`, `lib/pleroma/object.ex`).
//! This table is that fact laid out relationally
//! (`docs/ARCHITECTURE.md` decision 3): one row per `(user_id, status_id,
//! emoji)`, the emoji's image URL beside it instead of Pleroma's activity
//! `tag` list.
//!
//! [`react`] and [`unreact`] are both idempotent on that key — reacting twice
//! is one row that keeps its id, its `ap_id` and the insertion time that
//! orders the roll-up, and withdrawing a reaction that is not there (or no
//! longer is) removes nothing — the same pair of moves
//! [`crate::models::block`] makes. What a reaction *may* name (one emoji
//! grapheme, or a shortcode this instance has) is validated before it gets
//! here: see [`crate::util::emoji`] and the endpoint that resolves a shortcode
//! against [`crate::models::custom_emoji`].
//!
//! The two reads are both batched by design because the Status entity needs a
//! reaction roll-up per rendered status: [`for_statuses`] answers every emoji
//! of a whole page of statuses in one statement, keyed by status, and
//! [`reactors`] is its single-status counterpart for the reactions endpoint,
//! which also hands the reactors themselves over. Both keep the same order —
//! an emoji's *first* reaction time, then the emoji text — so a status's array
//! reads the same wherever it is rendered.
//!
//! Deleting a user or a status takes its reactions through the foreign key's
//! cascade, and [`delete_all_for_user`] is the sweep for the accounts whose
//! row survives deletion. The [`EmojiReaction::ap_id`] awaits the federation
//! ticket that sends the `EmojiReact` activity (decision 6); [`react`] mints
//! a local one.

use chrono::{DateTime, Utc};
use uuid::Uuid;

mod delete;
mod groups;
mod read;
#[cfg(test)]
mod test_support;
mod write;

pub use delete::{delete_all_for_user, unreact, unreact_in_domain};
pub use groups::ReactorGroup;
pub use read::{for_statuses, reactors};
pub use write::{get_by_ap_id, react, react_in_domain, react_with_ap_id};

/// How many reactors one `pleroma.emoji_reactions` entry names.
///
/// Pleroma's status renderer lists every reactor's id there; this instance
/// keeps the summary short and leaves the full list to
/// [`GET …/reactions`](crate::api::emoji_reactions), which renders the
/// accounts themselves.
pub const MAX_SUMMARY_REACTORS: usize = 5;

/// One row of `emoji_reactions`: `user_id` reacted to `status_id` with
/// `emoji`.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct EmojiReaction {
    /// `UUIDv7` primary key, minted by [`react`] (decision 4).
    pub id: Uuid,
    /// The reacting account.
    pub user_id: Uuid,
    /// The status reacted to.
    pub status_id: Uuid,
    /// The emoji as stored: one unicode emoji grapheme, or a custom emoji
    /// written `:shortcode:` (see [`crate::util::emoji`]).
    pub emoji: String,
    /// Image URL of the custom emoji as [`react`] recorded it; `None` for a
    /// unicode reaction.
    pub url: Option<String>,
    /// The instance a custom emoji belongs to, as its `custom_emojis`
    /// `domain`: `Some` for a remote emoji (`:misskeycat:` of `misskey.test`),
    /// `None` for a unicode or local one. Together with [`emoji`](Self::emoji)
    /// it is the emoji's identity.
    pub emoji_domain: Option<String>,
    /// `ActivityPub` id of the `EmojiReact` activity behind this row
    /// (decision 6): a `{BASE_URL}/activities/{uuid}` id minted for a local
    /// reaction, to be replaced by the remote id for a mirrored one.
    pub ap_id: Option<String>,
    /// When the reaction was first made; a repeat reaction of the same emoji
    /// keeps this, which is what orders the roll-up.
    pub inserted_at: DateTime<Utc>,
}

/// What to record: every `emoji_reactions` column except the `id` and `ap_id`
/// [`react`] mints and the `inserted_at` the column stamps.
#[derive(Clone, Debug)]
pub struct NewReaction<'a> {
    /// The reacting account.
    pub user_id: Uuid,
    /// The status it reacts to.
    pub status_id: Uuid,
    /// The emoji, already validated and in its stored spelling — one unicode
    /// grapheme or `:shortcode:`.
    pub emoji: &'a str,
    /// The custom emoji's image URL; `None` for a unicode emoji.
    pub url: Option<&'a str>,
}

/// One emoji on one status, summarised for the Status entity's
/// `pleroma.emoji_reactions`: how many reacted, whether the viewer did, the
/// custom emoji's image, and the first [`MAX_SUMMARY_REACTORS`] reactors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactionSummary {
    /// The emoji's API name, which is the text a client sends back to react:
    /// the stored text, except that a local custom emoji is its bare
    /// `shortcode` and a remote one `shortcode@domain` (see
    /// [`crate::util::emoji::display_name`]).
    pub name: String,
    /// How many accounts reacted with this emoji — all of them, not only the
    /// ones [`account_ids`](Self::account_ids) names.
    pub count: i64,
    /// Whether the `viewer_id` [`for_statuses`] was given is among the
    /// reactors; `false` for an anonymous page.
    pub me: bool,
    /// The custom emoji's image URL, `None` for a unicode emoji.
    pub url: Option<String>,
    /// The reactors, earliest reaction first, at most
    /// [`MAX_SUMMARY_REACTORS`] of them.
    pub account_ids: Vec<Uuid>,
}
