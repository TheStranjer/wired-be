//! The Mastodon `Status` entity: its core fields and `pleroma` block.
//!
//! The reference shape is Pleroma's `StatusView.render("show.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/status_view.ex`); the JSON keys are
//! emitted exactly as the ticket lists them, plus the `pleroma` extension
//! block last. Like every entity (`docs/ARCHITECTURE.md` decision 3) this
//! struct is pure data: [`Status::from_source`] renders one row and the
//! parts a page load batched for it, and nothing here touches the
//! database — the batch loader that gathers those parts is
//! [`crate::services::render_status`].
//!
//! Deliberate decisions, per the ticket:
//!
//! - `visibility` emits the stored value verbatim, `local` included:
//!   Pleroma's renderer passes `get_visibility/1` straight through, so a
//!   local-only status reads `"local"`, never `"public"`.
//! - A reblog row renders its own `id`/`uri`/`url`/`account`/
//!   `created_at`/`visibility`, an empty `content`, and the original as
//!   the nested [`reblog`](Status::reblog) entity — Pleroma's `Announce`
//!   arm of `show.json` is the same shape.
//! - `conversation_id` is the row's `UUIDv7` thread id as text
//!   (decision 4), not Pleroma's deprecated CRC32 of the context.
//! - Pleroma's `quote_url` is not emitted at all: the quoted target is
//!   the top-level `quote_id` (its id, whatever the viewer may see of
//!   it) and the rendered quote is Mastodon's `quote` object, whose
//!   bare status is mirrored in the `pleroma` block with `quote_visible`
//!   beside it.
//!
//! Keys a later ticket fills in, each from exactly one more batched
//! loader in [`crate::services::render_status`] (see its module docs):
//! `emojis` is filled by the emoji-in-statuses ticket, from the batch's
//! one [`get_by_shortcodes`](crate::models::custom_emoji::get_by_shortcodes)
//! read per author domain: the enabled emojis the row's content, warning or
//! poll options name under its *author's* domain, each rendered as the
//! [`CustomEmoji`](crate::entities::custom_emoji::CustomEmoji) entity.
//! `pleroma.direct_conversation_id` is filled by the
//! conversation-participations ticket, from
//! [`participation_ids`](crate::models::conversation::participation_ids)
//! — one batched read per page: a status of a DM thread renders the
//! viewer's participation id there, and every other row and every
//! non-participant `null`.
//! `poll` is filled by the polls render ticket, from the batch's reads of
//! [`crate::models::poll`]: a row the batch found a poll on embeds the
//! full [`Poll`] entity — options in position
//! order, and the viewer's marks as `voted` and `own_votes` — while a
//! poll-less row renders JSON `null`.
//! `card` is filled by the rich-media ticket, from
//! [`for_statuses`](crate::models::link_card::for_statuses) — one
//! batched read per page keyed to each row's *own* id (`status_cards`
//! keys on the row, so a reblog row renders its own id's card, not its
//! original's); the cached [`card_json`](crate::services::rich_media::card_json)
//! output renders verbatim, and a status linking no card — or only a
//! failed fetch — renders `null`, the same reading the deprecated
//! `GET /api/v1/statuses/{id}/card` endpoint answers with.
//! `pleroma.bookmark_folder` is filled by the folders ticket, from
//! [`bookmarked_folders`](crate::models::bookmark::bookmarked_folders) —
//! the one read that also answers `bookmarked`: a bookmarked row renders
//! its bookmark's folder id, an unfiled or un-bookmarked row `null`.
//! `pleroma.emoji_reactions`
//! is filled by the reactions ticket, from
//! [`emoji_reaction::for_statuses`](crate::models::emoji_reaction::for_statuses).
//! `favourited` is filled by the favourites ticket, from
//! [`favourited_status_ids`](crate::models::favourite::favourited_status_ids)
//! — one batched read per page, the viewer's id deciding membership; a
//! reblog row takes its flag from its original, whose id is where the
//! like lives.
//! `reblogged` is filled by the reblogs ticket, from
//! [`reblogged_status_ids`](crate::models::status::reblogged_status_ids)
//! — the same shape of batched read and the same keying as `favourited`:
//! a row is flagged by the id its boost lands on, which for a reblog row
//! is its *original's*, so the outer entity of the viewer's own boost
//! answers `reblogged: true` and so does the nested original.
//! `bookmarked` is filled by the bookmarks ticket, from
//! [`bookmarked_folders`](crate::models::bookmark::bookmarked_folders) — the
//! same shape of batched read; a reblog row takes its flag from *its own* id,
//! because a bookmark lands on the row its client named, not the original.
//! `pinned` and `pleroma.pinned_at` are filled by the pinning ticket, from
//! [`pinned_ids`](crate::models::pinned_status::pinned_ids) — one batched
//! read of the viewer's own pins per page; every row takes both from *its
//! own* id, because only a post of one's own can be pinned, and the map's
//! value is the moment the pin happened.
//! `muted` and `pleroma.thread_muted` are filled by the thread-mute ticket,
//! from
//! [`muted_conversation_ids`](crate::models::conversation_mute::muted_conversation_ids)
//! — one batched read of the viewer's muted threads per page; every row
//! takes both flags from its *own* `conversation_id`, because a mute is a
//! fact about the thread, not the post, so a reblog row and its nested
//! original always carry the same pair.
//! `filtered` is not the render batch's at all: the filter ticket applies
//! it as a post-processing step over rendered pages
//! ([`crate::services::filter::apply_filters`]), and a render that batch
//! alone produced — like [`Status::from_source`] here — carries the empty
//! array.
//! `quote`, `quote_id` and the `pleroma.quote`/`pleroma.quote_visible`
//! pair are filled by the quoting ticket, from the quoted originals the
//! service batch-loads and the one batched
//! [`filter_visible`](crate::models::status_visibility::filter_visible)
//! read that decides which of them this viewer may see. `quote` is
//! Mastodon 4.4's Quote entity: `{state, quoted_status}` with state
//! `accepted` (the bare quoted status rides in `quoted_status`),
//! `unauthorized` (hidden from this viewer) or `deleted` (gone), and
//! `null` when the row quotes nothing. The quoted status renders one
//! level deep: its own `quote` is only `{state, quoted_status_id}`.
//! The bare quoted status also lands in `pleroma.quote` beside its
//! `quote_visible` flag, false unless the state is `accepted`. `quote_id`
//! names the quoted row either way.
//! `quote_approval` is Mastodon 4.5's `{automatic, manual, current_user}`
//! object, always present: who may quote the status and whether this viewer
//! may (see [`QuoteApproval`], which lives in `quote_approval`).
//!
//! This module is a tree. [`StatusSource`] — the batch a render consumes —
//! lives in `source`; the [`PleromaMetadata`] extension block, the viewer
//! facts behind it and the builder that assembles them live in `pleroma`;
//! [`Status`] itself, its pure render and the field helpers live in
//! `entity`; `entity_render` carries the entity's second block of unit
//! tests; and `test_support` the unit-test fixtures the children share.
//! Every item is re-exported here, so an outside path stays
//! `crate::entities::status::…`.
//!
//! [`ReactionSummary`]: crate::entities::emoji_reaction::ReactionSummary

use serde::Serialize;

#[cfg(doc)]
use crate::entities::poll::Poll;

mod entity;
mod pleroma;
mod quote_approval;
mod source;

#[cfg(test)]
mod entity_render;
#[cfg(test)]
mod test_support;

pub use self::{
    entity::{QuoteEntity, QuoteRef, Status, StatusQuote},
    pleroma::PleromaMetadata,
    quote_approval::QuoteApproval,
    source::{QuoteSlot, StatusSource},
};

/// The `{"text/plain": …}` map the `pleroma` block renders for
/// [`content`](PleromaMetadata::content) and
/// [`spoiler_text`](PleromaMetadata::spoiler_text): Pleroma's
/// per-`MIME`-type text variants, of which this instance stores one.
#[derive(Clone, Debug, Serialize)]
pub struct PlainText {
    /// The plain-text value, keyed `text/plain` on the wire.
    #[serde(rename = "text/plain")]
    pub text_plain: String,
}

/// The `application` key: the client a status was posted with, as
/// Pleroma's `build_application/1` renders a `generator` — just its name
/// and website, not the OAuth
/// [`Application`](crate::entities::application::Application) entity.
#[derive(Clone, Debug, Serialize)]
pub struct StatusApplication {
    /// The client's name as the poster reported it.
    pub name: String,
    /// The client's website, `null` when none was reported.
    pub website: Option<String>,
}
