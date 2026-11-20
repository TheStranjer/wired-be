//! Turning [`models::status`] rows into rendered [`Status`] entities.
//!
//! [`render_statuses`] is the page renderer every timeline, list, thread
//! and single-status endpoint will share (Pleroma's equivalent is
//! `StatusView.render("index.json", …)`,
//! `lib/pleroma/web/mastodon_api/views/status_view.ex`, with its
//! `safe_render_many`): it takes the already-paged rows and loads
//! everything they render from — authors, media attachments, mentions,
//! hashtags, emoji reactions, favourites, the viewer's own reblogs,
//! bookmarks, the viewer's own pins, the viewer's muted threads, the
//! viewer's direct-message participations, rich-media cards, reblogged
//! originals, quoted originals and polls — with a FIXED set of batched
//! queries, one per part, each keyed by an id list (`WHERE … = ANY($1)`),
//! never a query per row. The count does not grow with the page size:
//! nineteen statement groups whatever the batch —
//!
//! 1. reblogged originals — one [`status::get_many`];
//! 2. quoted originals — one [`status::get_many`], the quoted rows' ids
//!    threading into every id-keyed read below so a nested quote's
//!    author, media, mentions, tags and reactions ride the same
//!    statements;
//! 3. which quoted originals this viewer may see — one
//!    [`status_visibility::filter_visible`] over step 2's rows, skipped
//!    entirely when no row on the page quotes anything;
//! 4. authors and replied-to authors — one [`user::get_many`];
//! 5. media attachments — one [`media_attachment::for_statuses`];
//! 6. mentions — one [`mention::for_statuses`];
//! 7. hashtags — one [`hashtag::for_statuses`];
//! 8. emoji reactions — one [`emoji_reaction::for_statuses`], the viewer's
//!    id riding along to answer each roll-up's `me`;
//! 9. the viewer's favourites of the page — one
//!    [`favourite::favourited_status_ids`], run only for an authenticated
//!    viewer; a reblog row takes its flag through its original's id, which
//!    is where the like lives;
//! 10. the viewer's reblogs among the page — one
//!     [`status::reblogged_status_ids`], likewise only for an authenticated
//!     viewer, and likewise keyed to each row's *original's* id: the boost
//!     lands on the original, so the outer entity of the viewer's own boost
//!     renders `reblogged: true` together with the original it nests;
//! 11. the viewer's bookmarks of the page — one [`bookmark::bookmarked_folders`],
//!     likewise only for an authenticated viewer; every row takes its flag
//!     from its *own* id, because a bookmark of a reblog lands on the reblog
//!     row itself. The same map's value is the folder that bookmark is filed
//!     in, which is what answers `pleroma.bookmark_folder` — so the flag and
//!     the folder can never disagree;
//! 12. the viewer's pins among the page — one [`pinned_status::pinned_ids`],
//!     likewise only for an authenticated viewer, and likewise keyed to
//!     each row's *own* id: only a post of one's own can be pinned, so a
//!     pin's author is its viewer. The same map answers `pinned` and
//!     `pleroma.pinned_at`, which is why the two can never disagree;
//! 13. the viewer's muted threads — one
//!     [`conversation_mute::muted_conversation_ids`], likewise only for an
//!     authenticated viewer, keyed to each row's *own*
//!     `conversation_id`: a mute is a fact about the thread, so a reblog
//!     row and its nested original carry the same pair. The same set
//!     answers `muted` and `pleroma.thread_muted`, which is why the two can
//!     never disagree;
//! 14. the viewer's direct-message participations — one
//!     [`conversation::participation_ids`], likewise only for an
//!     authenticated viewer, keyed to each row's *own*
//!     `direct_conversation_id`: a participation is a fact about the DM
//!     thread, and this is what answers `pleroma.direct_conversation_id`;
//! 15. the viewer's relationships to the page's authors — the one fixed
//!     [`Relationship::load_many`] set, run only for an authenticated
//!     viewer;
//! 16. the page's rich-media cards — one [`link_card::for_statuses`] read
//!     keyed by each row's *own* id (`status_cards` keys on the row, so a
//!     reblog row renders its own id's card); it runs for every viewer,
//!     because a card is a fact about the post's link, not about who is
//!     looking;
//! 17. the page's polls — one [`poll::for_statuses`] read keyed by every
//!     rendered id (reblogged originals included, so a reblog nests its
//!     original's poll); a row the read does not answer renders `poll:
//!     null`;
//! 18. the options of those polls — one [`poll::options_for_statuses`]
//!     read over the same ids, each entry in position order;
//! 19. the viewer's marks on the page's polls — one
//!     [`poll::votes_by_status`] read over the same ids, run only for an
//!     authenticated viewer; it answers every poll entity's `voted` flag
//!     and `own_votes` array.
//!
//! Loaders whose id list came out empty skip the database entirely, so a
//! page of plain posts costs eight queries (the cards read rides with the
//! rendered ids, and the two poll reads answer empty for it), and a viewer
//! adds the favourite, reblog, bookmark, pin, thread-mute, participation
//! and poll-vote reads and the relationship set.
//!
//! # Extension points
//!
//! The [entity](crate::entities::status) renders one placeholder per part
//! no ticket has stored yet, and each later ticket fills exactly one by
//! adding exactly one batched loader here (the shape `Batch::load`
//! already shows: one id list, one keyed map) and threading that map
//! through the batch context into [`StatusSource`]:
//!
//! - `pleroma.bookmark_folder`: *filled by the folders ticket* (step 11
//!   above); the folders ticket's [`bookmark::bookmarked_folders`] read
//!   answers it off the same statement that answers `bookmarked`, which is
//!   already filled by the bookmarks ticket.
//! - `pinned` and `pleroma.pinned_at`: *filled by the pinning ticket*
//!   (step 12 above), from the one [`pinned_status::pinned_ids`] read.
//! - `muted` and `pleroma.thread_muted`: *filled by the thread-mute
//!   ticket* (step 13 above), from the one
//!   [`conversation_mute::muted_conversation_ids`] read; the writes are
//!   `POST /api/v1/statuses/{id}/mute` and its `…/unmute` twin in
//!   [`crate::api::statuses`].
//! - `pleroma.direct_conversation_id`: *filled by the
//!   conversation-participations ticket* (step 14 above), from the one
//!   [`conversation::participation_ids`] read.
//! - `card`: *filled by the rich-media ticket* (step 16 above), from the
//!   one [`link_card::for_statuses`] read keyed to each row's *own* id;
//!   the cached [`card_json`] output renders verbatim, and the deprecated
//!   `GET /api/v1/statuses/{id}/card` endpoint in [`crate::api::statuses`]
//!   answers the same stored row.
//! - `poll`: *filled by the polls render ticket* (steps 17–19 above);
//!   [`crate::entities::poll::Poll::build`] renders the poll row, its
//!   options and the viewer's marks into the entity, and the row's own
//!   absence from the poll read is the entity's `null`.
//! - `emojis`: the custom emojis, one read keyed by the shortcodes the
//!   content uses.
//! - `pleroma.emoji_reactions`: *filled by the reactions ticket* (step 8
//!   above); it renders the roll-up
//!   [`emoji_reaction::for_statuses`] answers, capped at
//!   [`MAX_SUMMARY_REACTORS`](crate::models::emoji_reaction::MAX_SUMMARY_REACTORS)
//!   reactor ids per emoji. Reactions a viewer must not see are the
//!   reactions endpoint's filter, not this renderer's.
//! - `quote`, `quote_id`, `pleroma.quote` and `pleroma.quote_visible`:
//!   *filled by the quoting ticket* (steps 2 and 3 above); the quoted
//!   original the step 2 read brought in and the step 3 visibility read
//!   admits renders as one nested entity — no new query, and one level
//!   deep: a quoted status's own quote never renders — mirrored in
//!   `pleroma.quote` beside `pleroma.quote_visible`. A quote the viewer
//!   may not see renders `quote: null` with visibility `false` while
//!   `quote_id` still names the target.
//!
//! [`models::status`]: crate::models::status
//! [`Status`]: crate::entities::status::Status
//! [`StatusSource`]: crate::entities::status::StatusSource
//! [`Relationship::load_many`]: crate::entities::relationship::Relationship::load_many
//! [`card_json`]: crate::services::rich_media::card_json
//! [`conversation_mute::muted_conversation_ids`]: crate::models::conversation_mute::muted_conversation_ids
//! [`status_visibility::filter_visible`]: crate::models::status_visibility::filter_visible
//! [`status::get_many`]: crate::models::status::get_many
//! [`status::reblogged_status_ids`]: crate::models::status::reblogged_status_ids
//! [`user::get_many`]: crate::models::user::get_many
//! [`media_attachment::for_statuses`]: crate::models::media_attachment::for_statuses
//! [`mention::for_statuses`]: crate::models::mention::for_statuses
//! [`hashtag::for_statuses`]: crate::models::hashtag::for_statuses
//! [`emoji_reaction::for_statuses`]: crate::models::emoji_reaction::for_statuses
//! [`favourite::favourited_status_ids`]: crate::models::favourite::favourited_status_ids
//! [`bookmark::bookmarked_folders`]: crate::models::bookmark::bookmarked_folders
//! [`pinned_status::pinned_ids`]: crate::models::pinned_status::pinned_ids
//! [`conversation::participation_ids`]: crate::models::conversation::participation_ids
//! [`link_card::for_statuses`]: crate::models::link_card::for_statuses
//! [`poll::for_statuses`]: crate::models::poll::for_statuses
//! [`poll::options_for_statuses`]: crate::models::poll::options_for_statuses
//! [`poll::votes_by_status`]: crate::models::poll::votes_by_status

mod batch;
mod loaders;
mod loaders_polls_cards;
mod page_ids;

#[cfg(test)]
mod engagement_tests;
#[cfg(test)]
mod pins_threads_tests;
#[cfg(test)]
mod polls_cards_tests;
#[cfg(test)]
mod reblogs_quotes_tests;
#[cfg(test)]
mod render_tests;
#[cfg(test)]
mod test_support;

use crate::{
    entities::status::Status,
    error::AppResult,
    models::{status::Status as StatusRow, user::User},
    state::AppState,
};

use batch::Batch;

/// Renders `statuses` as the [`Status`] entity for `viewer` (see
/// [`ViewerMode`]: whose account the per-viewer flags render from, and
/// whether the visibility rules apply), in the input order.
///
/// The rows arrive as the caller paged and narrowed them — per-status
/// visibility is the caller's [`filter_visible`] step — and this renderer
/// only drops the rows no render can answer for:
///
/// - a tombstone ([`deleted_at`](StatusRow::deleted_at) set) is skipped:
///   a deleted status is gone from every API surface, and list reads
///   already exclude it;
/// - a reblog row is skipped when its original is gone (deleted or never
///   stored): Pleroma's renderer has nothing to nest either, and a reblog
///   of a gone post is invisible by rule 7 of
///   [`crate::models::status_visibility`].
///
/// All batch loading happens once per call, whatever the page size — see
/// the [module docs](self) for the fixed query set and where each later
/// flag's loader goes.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
///
/// [`filter_visible`]: crate::models::status_visibility::filter_visible
pub async fn render_statuses(
    state: &AppState,
    viewer: ViewerMode<'_>,
    statuses: &[StatusRow],
) -> AppResult<Vec<Status>> {
    let rows: Vec<&StatusRow> = statuses
        .iter()
        .filter(|row| row.deleted_at.is_none())
        .collect();
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let batch = Batch::load(state, viewer, &rows).await?;
    Ok(batch.render_all(&rows))
}

/// Who a page of [`Status`] entities renders for, and how much of the
/// post network that render may see: the second argument of
/// [`render_statuses`] and [`render_status`].
///
/// The two modes differ in exactly one thing — whether the visibility
/// rules of [`crate::models::status_visibility`] are in force inside the
/// render — and agree in the rest: both name the account whose own flags
/// (`favourited`, `reblogged`, `bookmarked`, `pinned`, `muted`,
/// `pleroma.relationship`, the poll's `voted`) the entity carries.
///
/// - [`ViewerMode::User`] is every ordinary surface: the viewer's own
///   account, or `None` for an anonymous request. The rules are in force,
///   so a quoted original the viewer may not reach renders `quote: null`
///   with `pleroma.quote_visible` false (the module docs' step 3 read).
/// - [`ViewerMode::Admin`] is the staff surface of
///   [`crate::api::admin::statuses`] (WBE-201): Pleroma's
///   `AdminAPI.StatusController` and its `AdminAPI.StatusView`, which
///   render an activity's JSON with the reading admin as viewer and run no
///   visibility filter at all. The staff member's own flags still render
///   off their own account; what the mode grants is that every quoted
///   original on the page is admitted, private or direct included.
///
/// The mode is the renderer's half of the bypass only. Which *rows* a
/// staff listing carries is the caller's filter — the admin listing's
/// `godmode` parameter and the absence of any [`filter_visible`] step are
/// what decide that; this type decides nothing about the rows it is handed.
///
/// [`filter_visible`]: crate::models::status_visibility::filter_visible
#[derive(Clone, Copy, Debug)]
pub enum ViewerMode<'a> {
    /// The ordinary view of `viewer` — `None` for an anonymous request —
    /// with the visibility rules in force.
    User(Option<&'a User>),
    /// The staff view of an admin or moderator, with the visibility rules
    /// bypassed.
    Admin(&'a User),
}

impl<'a> ViewerMode<'a> {
    /// The anonymous view: the `User` arm holding `None`, the spelling the
    /// unauthenticated callers read better than the nested option.
    pub const ANONYMOUS: Self = Self::User(None);

    /// The ordinary view of `viewer`, the flags of their account and no
    /// other, with the visibility rules in force.
    #[must_use]
    pub const fn user(viewer: &'a User) -> Self {
        Self::User(Some(viewer))
    }

    /// The staff view of `admin`: their own flags, the visibility rules
    /// bypassed.
    #[must_use]
    pub const fn admin(admin: &'a User) -> Self {
        Self::Admin(admin)
    }

    /// The account whose own flags render, `None` for an anonymous view.
    #[must_use]
    pub const fn viewer(self) -> Option<&'a User> {
        match self {
            Self::User(viewer) => viewer,
            Self::Admin(admin) => Some(admin),
        }
    }

    /// Whether the visibility rules of [`crate::models::status_visibility`]
    /// are bypassed for this view: [`ViewerMode::Admin`] is, and
    /// [`ViewerMode::User`] never is.
    #[must_use]
    pub const fn bypasses_visibility(self) -> bool {
        matches!(self, Self::Admin(_))
    }
}

/// Renders the single `status`, delegating to [`render_statuses`]: `None`
/// when the row is a tombstone, or a reblog whose original is gone. One
/// row costs the same fixed query set, no more.
///
/// # Errors
///
/// As [`render_statuses`].
pub async fn render_status(
    state: &AppState,
    viewer: ViewerMode<'_>,
    status: &StatusRow,
) -> AppResult<Option<Status>> {
    let mut rendered = render_statuses(state, viewer, std::slice::from_ref(status)).await?;
    Ok(rendered.pop())
}
