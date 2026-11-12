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

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    config::Config,
    entities::{
        account::JsonBool,
        poll::Poll as PollEntity,
        relationship::{Relationship, RelationshipMap},
        status::{Status, StatusSource},
    },
    error::AppResult,
    models::{
        bookmark, conversation, conversation_mute,
        emoji_reaction::{self, ReactionSummary},
        favourite,
        hashtag::{self, Hashtag},
        link_card::{self, LinkCard},
        media_attachment::{self, MediaAttachment as AttachmentRow},
        mention, pinned_status,
        poll::{self, Poll as PollRow, PollOption as PollOptionRow},
        status::{self, Status as StatusRow},
        status_visibility,
        user::{self, User},
    },
    state::AppState,
};

/// Renders `statuses` as the [`Status`] entity for `viewer` (`None`: an
/// anonymous request), in the input order.
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
    viewer: Option<&User>,
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

/// Renders the single `status`, delegating to [`render_statuses`]: `None`
/// when the row is a tombstone, or a reblog whose original is gone. One
/// row costs the same fixed query set, no more.
///
/// # Errors
///
/// As [`render_statuses`].
pub async fn render_status(
    state: &AppState,
    viewer: Option<&User>,
    status: &StatusRow,
) -> AppResult<Option<Status>> {
    let mut rendered = render_statuses(state, viewer, std::slice::from_ref(status)).await?;
    Ok(rendered.pop())
}

/// The batch-loaded parts of one page: every map keyed by the id its
/// loader was asked about, so a row's render is a lookup, never a query.
#[derive(Debug)]
struct Batch<'a> {
    /// The instance configuration the [`Account`] and tag URLs render
    /// from.
    ///
    /// [`Account`]: crate::entities::account::Account
    config: &'a Config,
    /// The account whose view is being rendered, if any.
    viewer: Option<&'a User>,
    /// Every author and replied-to author on the page, keyed by id.
    users: HashMap<Uuid, User>,
    /// The reblogged originals, keyed by id.
    originals: HashMap<Uuid, StatusRow>,
    /// The page's quoted originals, keyed by id — the module-docs step 2
    /// read. A tombstone or a target that is not stored here is simply
    /// absent, which is one way a quote renders `null`.
    quoted: HashMap<Uuid, StatusRow>,
    /// Which of [`Batch::quoted`] this viewer may see — the module-docs
    /// step 3 read, the one [`status_visibility::filter_visible`] over
    /// the loaded quoted rows; empty when no row on the page quotes
    /// anything. A quoted row outside this set renders `quote: null`
    /// with `pleroma.quote_visible` false while `quote_id` still names
    /// it.
    visible_quotes: HashSet<Uuid>,
    /// Media attachments, keyed by status, in position order.
    media: HashMap<Uuid, Vec<AttachmentRow>>,
    /// Mentions, keyed by status, in mention order.
    mentions: HashMap<Uuid, Vec<User>>,
    /// Hashtags, keyed by status, in name order.
    tags: HashMap<Uuid, Vec<Hashtag>>,
    /// Emoji reactions, keyed by status, in the roll-up's order
    /// ([`emoji_reaction::for_statuses`]).
    reactions: HashMap<Uuid, Vec<ReactionSummary>>,
    /// The statuses on the page (and their originals) the viewer has
    /// favourited — one [`favourite::favourited_status_ids`] read keyed by
    /// every rendered id; empty for an anonymous viewer, who has no rows.
    favourites: HashSet<Uuid>,
    /// The statuses on the page (and their originals) the viewer has
    /// reblogged — one [`status::reblogged_status_ids`] read keyed by every
    /// rendered id; empty for an anonymous viewer, who has no rows. Keyed
    /// to the *original's* id like [`Batch::favourites`]: the boost lands
    /// on the original, so a reblog row on the page is flagged through its
    /// own [`reblog_of_id`](StatusRow::reblog_of_id).
    reblogs: HashSet<Uuid>,
    /// The rows on the page the viewer has bookmarked, each with the
    /// folder that bookmark is filed in — one [`bookmark::bookmarked_folders`]
    /// read keyed by every rendered id; empty for an anonymous viewer, who
    /// has no rows. Unlike [`Batch::favourites`], membership is keyed to the
    /// row's own id: a bookmark of a reblog lands on the reblog row — and so
    /// does the folder riding on it.
    bookmarks: HashMap<Uuid, Option<Uuid>>,
    /// The rows on the page the viewer has pinned to their own profile, and
    /// when — one [`pinned_status::pinned_ids`] read keyed by every rendered
    /// id; empty for an anonymous viewer, who has no rows. Keyed to the
    /// row's own id like [`Batch::bookmarks`], because a pin is always a
    /// post of the pinning account's own; the value is what
    /// `pleroma.pinned_at` renders, so the flag and the stamp cannot
    /// disagree.
    pins: HashMap<Uuid, DateTime<Utc>>,
    /// The threads on the page the viewer has muted — one
    /// [`conversation_mute::muted_conversation_ids`] read keyed by the
    /// page's conversation ids; empty for an anonymous viewer, who has no
    /// mutes. Keyed to each row's *own*
    /// [`conversation_id`](StatusRow::conversation_id), because a mute is a
    /// fact about the thread: a reblog row shares its original's
    /// conversation, so the pair of flags rides the same membership and the
    /// two can never disagree.
    thread_mutes: HashSet<Uuid>,
    /// The DM threads on the page the viewer participates in, each keyed to
    /// the thread with the id of *their own* participation row — one
    /// [`conversation::participation_ids`] read keyed by the page's
    /// `direct_conversation_id`s; empty for an anonymous viewer, who has no
    /// rows. Keyed to each row's *own*
    /// [`direct_conversation_id`](StatusRow::direct_conversation_id),
    /// because a participation is a fact about the thread: this is what
    /// answers `pleroma.direct_conversation_id`.
    direct_participations: HashMap<Uuid, Uuid>,
    /// The polls on the page — one [`poll::for_statuses`] read keyed by
    /// every rendered id; a reblogged original's id is among them, so a
    /// reblog nests its original's poll. Rows the read does not answer
    /// carry no entry, which is the entity's `poll: null`.
    polls: HashMap<Uuid, PollRow>,
    /// The options of those polls, keyed by status, in position order —
    /// one [`poll::options_for_statuses`] read over the same ids.
    poll_options: HashMap<Uuid, Vec<PollOptionRow>>,
    /// The viewer's marks on the page's polls, keyed by status with
    /// positions ascending — one [`poll::votes_by_status`] read over the
    /// same ids, run only for an authenticated viewer; the empty map makes
    /// every poll entity read `voted: false` with `own_votes: []`.
    poll_votes: HashMap<Uuid, Vec<i32>>,
    /// The viewer's relationships to the page's authors, keyed by account
    /// id; `None` for an anonymous viewer.
    relationships: Option<RelationshipMap>,
    /// The rich-media cards of the rows on the page (and of the originals
    /// nested in reblogs), keyed by status — one
    /// [`link_card::for_statuses`] read keyed by every rendered id. A
    /// status that links no cached card is absent, and a card renders the
    /// same for every viewer: it is a fact about the post's link, not
    /// about who is looking. Keyed to each row's *own* id, because
    /// `status_cards` keys on the row.
    cards: HashMap<Uuid, LinkCard>,
}

impl<'a> Batch<'a> {
    /// Runs the fixed query set of the [module docs](self) for `rows` and
    /// keeps the results together. Every loader is handed its id list
    /// deduplicated, and each one skips the database when the list is
    /// empty.
    async fn load(
        state: &'a AppState,
        viewer: Option<&'a User>,
        rows: &[&'a StatusRow],
    ) -> AppResult<Self> {
        let pool = &state.db;
        let reblog_ids = distinct(rows.iter().copied().filter_map(|row| row.reblog_of_id));
        let originals = load_statuses_by_id(pool, &reblog_ids).await?;
        let quote_ids = distinct(rows.iter().copied().filter_map(|row| row.quote_of_id));
        let quoted = load_statuses_by_id(pool, &quote_ids).await?;
        let visible_quotes = load_visible_quotes(pool, &quoted, viewer).await?;
        debug!(
            quotes = quoted.len(),
            visible = visible_quotes.len(),
            "quoted originals batch-loaded; the visible ones render nested, one level deep"
        );
        let rendered_ids = distinct(
            rows.iter()
                .copied()
                .map(|row| row.id)
                .chain(originals.keys().copied())
                .chain(quoted.keys().copied()),
        );
        let account_ids = distinct(
            rows.iter()
                .copied()
                .chain(originals.values())
                .chain(quoted.values())
                .flat_map(|row| [Some(row.user_id), row.in_reply_to_account_id])
                .flatten(),
        );
        let author_ids = distinct(
            rows.iter()
                .copied()
                .map(|row| row.user_id)
                .chain(originals.values().map(|row| row.user_id))
                .chain(quoted.values().map(|row| row.user_id)),
        );
        let conversation_ids = distinct(
            rows.iter()
                .copied()
                .chain(originals.values())
                .chain(quoted.values())
                .map(|row| row.conversation_id),
        );
        let direct_ids = distinct(
            rows.iter()
                .copied()
                .chain(originals.values())
                .filter_map(|row| row.direct_conversation_id),
        );
        let polls = load_polls(pool, &rendered_ids).await?;
        let poll_options = load_poll_options(pool, &rendered_ids).await?;
        let poll_votes = load_poll_votes(pool, &rendered_ids, viewer).await?;
        debug!(
            polls = polls.len(),
            options = poll_options.len(),
            votes = poll_votes.len(),
            "polls batch-loaded with their options and the viewer's marks"
        );
        Ok(Self {
            config: state.config.as_ref(),
            viewer,
            users: load_users(pool, &account_ids).await?,
            originals,
            quoted,
            visible_quotes,
            media: load_media(pool, &rendered_ids).await?,
            mentions: load_mentions(pool, &rendered_ids).await?,
            tags: load_tags(pool, &rendered_ids).await?,
            reactions: load_reactions(pool, &rendered_ids, viewer).await?,
            favourites: load_favourites(pool, &rendered_ids, viewer).await?,
            reblogs: load_reblogs(pool, &rendered_ids, viewer).await?,
            bookmarks: load_bookmarks(pool, &rendered_ids, viewer).await?,
            pins: load_pins(pool, &rendered_ids, viewer).await?,
            thread_mutes: load_thread_mutes(pool, &conversation_ids, viewer).await?,
            direct_participations: load_direct_participations(pool, &direct_ids, viewer).await?,
            polls,
            poll_options,
            poll_votes,
            relationships: load_relationships(pool, viewer, &author_ids).await?,
            cards: load_cards(pool, &rendered_ids).await?,
        })
    }

    /// Renders every row of `rows` in order, nesting the original of a
    /// reblog and the quoted original of a quote, and dropping the rows
    /// the [`render_statuses`] contract cannot answer for.
    fn render_all(&self, rows: &[&StatusRow]) -> Vec<Status> {
        let mut rendered = Vec::with_capacity(rows.len());
        for row in rows {
            let reblog = self.render_reblog(row);
            if row.reblog_of_id.is_some() && reblog.is_none() {
                continue;
            }
            let quote = self.render_quote(row);
            if let Some(entity) = self.render_entity(row, reblog, quote) {
                rendered.push(entity);
            }
        }
        rendered
    }

    /// The nested render of the original `row` reblogs: `None` when the
    /// row reblogs nothing, and also when its original is gone — the
    /// caller drops such reblogs. Every reblog renders its original
    /// afresh, from maps already in memory; chains longer than one link
    /// do not occur in stored data (see [`crate::models::status_visibility`]).
    /// The original carries its own quote when one rides the batch and
    /// this viewer may see it — the quote chain's depth limit belongs to
    /// [`Batch::render_quote`], not here.
    fn render_reblog(&self, row: &StatusRow) -> Option<Box<Status>> {
        let original = self.originals.get(&row.reblog_of_id?)?;
        let quote = self.render_quote(original);
        Some(Box::new(self.render_entity(original, None, quote)?))
    }

    /// The nested render of the original `row` quotes: `Some` JSON when
    /// [`row.quote_of_id`](StatusRow::quote_of_id) names a row the batch
    /// loaded (live: [`status::get_many`] answers tombstones not at all),
    /// the visibility read admitted to [`Batch::visible_quotes`], and the
    /// maps can render (an author the batch knows). That render is one
    /// level deep — Pleroma's `show_quote: false` on the quote's own
    /// rendering — so the quoted row comes back with its OWN quote
    /// forced to `None`, whatever it quoted. `None` — which the entity
    /// renders as `quote: null` with `pleroma.quote_visible` false while
    /// `quote_id` still names the target — when the id names nothing
    /// this viewer may see.
    fn render_quote(&self, row: &StatusRow) -> Option<Value> {
        let quoted = self.quoted.get(&row.quote_of_id?)?;
        if !self.visible_quotes.contains(&quoted.id) {
            return None;
        }
        let entity = self.render_entity(quoted, None, None)?;
        serde_json::to_value(entity).ok()
    }

    /// Renders one `row` from the loaded maps, attaching `reblog` as its
    /// nested original and `quote` — a [`Batch::render_quote`] result —
    /// as its nested quote. `None` when the row's author is not in the
    /// batch — the foreign keys make that unobservable in stored data,
    /// and a status with no author has no render.
    ///
    /// The `favourited` flag is membership in the loaded set, keyed to the
    /// row's own id — except for a reblog row, which is flagged through the
    /// *original's* id, the status the like actually lands on (Pleroma's
    /// `Announce` arm computes `favourited` off the liked object). The
    /// nested original is flagged from its own id in the same set. The
    /// `reblogged` flag is membership in the reblogs set under the *same*
    /// keying — a boost lands on the original, so the outer entity of the
    /// viewer's own boost and the nested original both carry it (Pleroma's
    /// `Announce` arm again). The
    /// `bookmarked` flag is membership in the bookmarks map keyed to the
    /// row's *own* id for every row: a bookmark of a reblog names the
    /// reblog row and lives there (see [`crate::models::bookmark`]), and
    /// `pleroma.bookmark_folder` is that same entry's folder — one read,
    /// which is why the flag and the folder cannot disagree. The
    /// `pinned` flag and `pleroma.pinned_at` are the pins map read at the
    /// row's *own* id too — only a post of the viewer's own can be pinned,
    /// so one lookup answers both halves and they always agree. The
    /// `muted` flag and `pleroma.thread_muted` are membership in the muted
    /// threads set keyed to the row's *own* `conversation_id` — a mute is a
    /// fact about the thread, and a reblog shares its original's, so the
    /// nested pair always renders the same. The `poll` entity is the poll
    /// read at the row's *own* id, with its options and the viewer's marks
    /// from the same-keyed mates — and because the batch loaded over the
    /// rendered ids, originals included, the nested render of a reblog
    /// carries the original's poll. The `card` is the cards map read at
    /// the row's *own* id — `status_cards` keys on the row, so a reblog
    /// row renders its own id's card and the nested original its own. The
    /// `quote` is the batch's serialized nested quote render —
    /// `pleroma.quote` mirrors it and `pleroma.quote_visible` is its
    /// presence, which is why the three can never disagree, while
    /// `quote_id` names the target whatever the render answered.
    fn render_entity(
        &self,
        row: &StatusRow,
        reblog: Option<Box<Status>>,
        quote: Option<Value>,
    ) -> Option<Status> {
        let author = self.users.get(&row.user_id)?;
        let subject_id = row.reblog_of_id.unwrap_or(row.id);
        let pinned_at = self.pins.get(&row.id).copied();
        // One map lookup answers both halves of the bookmark fact: the
        // flag is membership in the read, the folder its value.
        let bookmark = self.bookmarks.get(&row.id);
        let source = StatusSource {
            row,
            author,
            reply_author: row
                .in_reply_to_account_id
                .and_then(|id| self.users.get(&id)),
            media: self.media.get(&row.id).map_or(&[], Vec::as_slice),
            mentions: self.mentions.get(&row.id).map_or(&[], Vec::as_slice),
            tags: self.tags.get(&row.id).map_or(&[], Vec::as_slice),
            reactions: self.reactions.get(&row.id).map_or(&[], Vec::as_slice),
            card: self.cards.get(&row.id),
            favourited: self.favourites.contains(&subject_id),
            reblogged: JsonBool(self.reblogs.contains(&subject_id)),
            bookmarked: bookmark.is_some(),
            bookmark_folder: bookmark.copied().flatten(),
            pinned_at,
            muted: JsonBool(self.thread_mutes.contains(&row.conversation_id)),
            direct_participation: row
                .direct_conversation_id
                .and_then(|thread| self.direct_participations.get(&thread).copied()),
            quote,
            reblog,
            poll: self.render_poll(row),
        };
        Some(Status::from_source(
            source,
            self.viewer,
            self.relationships.as_ref(),
            self.config,
        ))
    }

    /// The [`Poll`] entity of `row`: `None` when the batch's poll read
    /// found nothing for it, and otherwise [`Poll::build`] over the row,
    /// its options in position order and the viewer's marks — the empty
    /// slice for an anonymous viewer, whose every poll renders `voted:
    /// false` with `own_votes: []`. The render instant decides `expired`
    /// and, through it, whether `hide_totals` still withholds the option
    /// counts.
    ///
    /// [`Poll`]: crate::entities::poll::Poll
    /// [`Poll::build`]: crate::entities::poll::Poll::build
    fn render_poll(&self, row: &StatusRow) -> Option<PollEntity> {
        let poll = self.polls.get(&row.id)?;
        Some(PollEntity::build(
            poll,
            self.poll_options.get(&row.id).map_or(&[], Vec::as_slice),
            self.poll_votes.get(&row.id).map_or(&[], Vec::as_slice),
            Utc::now(),
        ))
    }
}

/// The distinct ids of `ids`, in order of first appearance: one ask of a
/// batch loader is enough, whatever references an id on the page.
fn distinct<I>(ids: I) -> Vec<Uuid>
where
    I: IntoIterator<Item = Uuid>,
{
    let mut seen = HashSet::new();
    ids.into_iter().filter(|id| seen.insert(*id)).collect()
}

/// [`status::get_many`] keyed by id — one query for a page's reblogged
/// originals or quoted originals, and no query at all for an empty list.
/// Tombstones are already excluded by the loader, which is how a reblog
/// of a deleted original falls out of the render.
async fn load_statuses_by_id(pool: &PgPool, ids: &[Uuid]) -> AppResult<HashMap<Uuid, StatusRow>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = status::get_many(pool, ids).await?;
    Ok(rows.into_iter().map(|row| (row.id, row)).collect())
}

/// [`status_visibility::filter_visible`] over the batch-loaded quoted
/// originals — the one read that decides which quotes this viewer may
/// see, and the only statement step 3 adds: a page whose rows quote
/// nothing skips it entirely, and the filter itself runs its visibility
/// clause in the database, so a private or blocked quote falls out of
/// the set and renders `quote: null` with `pleroma.quote_visible` false
/// (its `quote_id` still names the target). The quoted rows' authors are
/// the page's strangers' by nature, so the filter's follow/mention/
/// block `EXISTS` arms run for real here.
async fn load_visible_quotes(
    pool: &PgPool,
    quoted: &HashMap<Uuid, StatusRow>,
    viewer: Option<&User>,
) -> AppResult<HashSet<Uuid>> {
    let candidates: Vec<StatusRow> = quoted.values().cloned().collect();
    let visible = status_visibility::filter_visible(pool, candidates, viewer).await?;
    Ok(visible.into_iter().map(|row| row.id).collect())
}

/// [`user::get_many`] keyed by id — one query for every author and
/// replied-to author on the page.
async fn load_users(pool: &PgPool, ids: &[Uuid]) -> AppResult<HashMap<Uuid, User>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let accounts = user::get_many(pool, ids).await?;
    Ok(accounts
        .into_iter()
        .map(|account| (account.id, account))
        .collect())
}

/// [`media_attachment::for_statuses`]: one query for every attachment.
async fn load_media(pool: &PgPool, ids: &[Uuid]) -> AppResult<HashMap<Uuid, Vec<AttachmentRow>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    media_attachment::for_statuses(pool, ids).await
}

/// [`mention::for_statuses`]: one query for every @-mention.
async fn load_mentions(pool: &PgPool, ids: &[Uuid]) -> AppResult<HashMap<Uuid, Vec<User>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    mention::for_statuses(pool, ids).await
}

/// [`hashtag::for_statuses`]: one query for every hashtag.
async fn load_tags(pool: &PgPool, ids: &[Uuid]) -> AppResult<HashMap<Uuid, Vec<Hashtag>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    hashtag::for_statuses(pool, ids).await
}

/// [`emoji_reaction::for_statuses`]: one query for every reaction of the
/// page, the viewer riding along only to answer each roll-up's `me` (an
/// anonymous viewer has no id to pass, which makes every `me` false).
async fn load_reactions(
    pool: &PgPool,
    ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashMap<Uuid, Vec<ReactionSummary>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    emoji_reaction::for_statuses(pool, ids, viewer.map(|who| who.id)).await
}

/// [`favourite::favourited_status_ids`]: one query for the viewer's likes
/// among the page's ids (reblogged originals included, since that is
/// where a reblog's like lives). An anonymous viewer has no rows to ask
/// about, and the empty set makes every flag false — no query at all.
async fn load_favourites(
    pool: &PgPool,
    ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashSet<Uuid>> {
    let Some(viewer) = viewer else {
        return Ok(HashSet::new());
    };
    favourite::favourited_status_ids(pool, viewer.id, ids).await
}

/// [`status::reblogged_status_ids`]: one query for the originals among the
/// page's ids the viewer has a live reblog of (reblogged originals
/// included, since that is where a reblog's boost lives and a render
/// answers the nested original's flag too). An anonymous viewer has no
/// rows to ask about, and the empty set makes every flag false — no query
/// at all.
async fn load_reblogs(
    pool: &PgPool,
    ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashSet<Uuid>> {
    let Some(viewer) = viewer else {
        return Ok(HashSet::new());
    };
    status::reblogged_status_ids(pool, viewer.id, ids).await
}

/// [`bookmark::bookmarked_folders`]: one query for the viewer's bookmarks
/// among the page's ids, each carrying the folder it is filed in (the ids
/// of reblogged originals ride along, since a render answers their flags too
/// and one of them may be bookmarked). An anonymous viewer has no rows to
/// ask about, and the empty map makes every flag false and every folder
/// `null` — no query at all.
async fn load_bookmarks(
    pool: &PgPool,
    ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashMap<Uuid, Option<Uuid>>> {
    let Some(viewer) = viewer else {
        return Ok(HashMap::new());
    };
    bookmark::bookmarked_folders(pool, viewer.id, ids).await
}

/// [`pinned_status::pinned_ids`]: one query for the viewer's pins among the
/// page's ids, keyed by status and carrying the moment each pin happened
/// (the ids of reblogged originals ride along, since a render answers their
/// flags too and one of them may be the viewer's own pinned post). An
/// anonymous viewer has no rows to ask about, and the empty map makes every
/// flag false and every stamp `null` — no query at all.
async fn load_pins(
    pool: &PgPool,
    ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashMap<Uuid, DateTime<Utc>>> {
    let Some(viewer) = viewer else {
        return Ok(HashMap::new());
    };
    pinned_status::pinned_ids(pool, viewer.id, ids).await
}

/// [`conversation_mute::muted_conversation_ids`]: one query for the threads
/// among the page's conversation ids the viewer has muted (the ids of
/// reblogged originals ride along, though a reblog shares its original's
/// conversation, so the pair never diverges). An anonymous viewer has no
/// mutes to ask about, and the empty set makes every flag false — no query
/// at all.
async fn load_thread_mutes(
    pool: &PgPool,
    conversation_ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashSet<Uuid>> {
    let Some(viewer) = viewer else {
        return Ok(HashSet::new());
    };
    conversation_mute::muted_conversation_ids(pool, viewer.id, conversation_ids).await
}

/// [`conversation::participation_ids`]: one query for the DM threads among
/// the page's direct-conversation ids the viewer participates in, keyed by
/// thread with the viewer's own participation id. An anonymous viewer has
/// no rows to ask about, and the empty map makes every
/// `pleroma.direct_conversation_id` `null` — no query at all.
async fn load_direct_participations(
    pool: &PgPool,
    direct_ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashMap<Uuid, Uuid>> {
    let Some(viewer) = viewer else {
        return Ok(HashMap::new());
    };
    conversation::participation_ids(pool, viewer.id, direct_ids).await
}

/// [`poll::for_statuses`]: one query for every poll of the page (reblogged
/// originals included, since a reblog nests its original's poll). A row
/// with no poll stays absent from the map, which is the entity's `null`.
async fn load_polls(pool: &PgPool, ids: &[Uuid]) -> AppResult<HashMap<Uuid, PollRow>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    poll::for_statuses(pool, ids).await
}

/// [`poll::options_for_statuses`]: one query for the options of the page's
/// polls, keyed by status and in position order — the choices every poll
/// entity lists.
async fn load_poll_options(
    pool: &PgPool,
    ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<PollOptionRow>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    poll::options_for_statuses(pool, ids).await
}

/// [`poll::votes_by_status`]: one query for the viewer's marks on the
/// page's polls, keyed by status with positions ascending. An anonymous
/// viewer has no marks to ask about, and the empty map makes every poll
/// render `voted: false` with `own_votes: []` — no query at all.
async fn load_poll_votes(
    pool: &PgPool,
    ids: &[Uuid],
    viewer: Option<&User>,
) -> AppResult<HashMap<Uuid, Vec<i32>>> {
    let Some(viewer) = viewer else {
        return Ok(HashMap::new());
    };
    poll::votes_by_status(pool, ids, viewer.id).await
}

/// [`link_card::for_statuses`]: one query for the rich-media card of every
/// row on the page and of the originals nested in reblogs (the rendered
/// id list covers both), keyed to each row's *own* id — `status_cards`
/// keys on the row. Rows that link no cached card are simply absent from
/// the map, and an empty id list skips the database — no query at all.
async fn load_cards(pool: &PgPool, ids: &[Uuid]) -> AppResult<HashMap<Uuid, LinkCard>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    link_card::for_statuses(pool, ids).await
}

/// The viewer's relationships to the page's authors, keyed by account id:
/// [`Relationship::load_many`] runs its own fixed set of queries once per
/// page; an anonymous viewer has none to load, and an empty author list
/// skips even that set.
async fn load_relationships(
    pool: &PgPool,
    viewer: Option<&User>,
    ids: &[Uuid],
) -> AppResult<Option<RelationshipMap>> {
    let Some(viewer) = viewer else {
        return Ok(None);
    };
    if ids.is_empty() {
        return Ok(Some(RelationshipMap::new()));
    }
    let loaded = Relationship::load_many(pool, viewer.id, ids).await?;
    Ok(Some(ids.iter().copied().zip(loaded).collect()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{SubsecRound as _, TimeZone as _, Utc};
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        http_client::HttpClient,
        mailer::MemoryTransport,
        models::{
            bookmark_folder,
            emoji_reaction::{self, NewReaction},
            follow,
            media_attachment::NewMediaAttachment,
            poll::{self, PollParams},
            status::{NewStatus, Visibility, insert, soft_delete},
            user::{NewUser, local_ap_id},
        },
        uploads::UploaderBackend,
        util::time::mastodon_time,
    };

    /// The instance origin the fixtures post under.
    const BASE: &str = TEST_BASE_URL;
    /// A unicode emoji the reaction fixtures use.
    const THUMB: &str = "👍";
    /// A custom emoji's stored spelling, and the image recorded with it.
    const CAT: &str = ":cat:";
    /// [`CAT`]'s image URL.
    const CAT_URL: &str = "https://test.example/emoji/cats/cat.png";

    /// The state under test: `pool` and the test config, with the
    /// collaborators the renderer never touches.
    fn test_state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(MemoryTransport::new()),
        }
    }

    /// Inserts a local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        user::insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("the fixture user inserts")
    }

    /// Inserts `text` as a local public status of `author`.
    async fn post(pool: &PgPool, author: &User, text: &str) -> StatusRow {
        insert(pool, BASE, &NewStatus::local(author, text))
            .await
            .expect("a plain local post inserts")
    }

    /// Every stored status, live or not, oldest id first.
    async fn stored_statuses(pool: &PgPool) -> Vec<StatusRow> {
        sqlx::query_as::<_, StatusRow>("SELECT * FROM statuses ORDER BY id")
            .fetch_all(pool)
            .await
            .expect("the table reads")
    }

    /// The id strings of `statuses`, in order.
    fn id_strings(statuses: &[StatusRow]) -> Vec<String> {
        statuses
            .iter()
            .map(|status| status.id.to_string())
            .collect()
    }

    /// The ids of rendered entities, in order.
    fn rendered_ids(rendered: &[Status]) -> Vec<&str> {
        rendered.iter().map(|status| status.id.as_str()).collect()
    }

    /// A local user named `nickname` with `count` plain posts.
    async fn author_with_posts(pool: &PgPool, nickname: &str, count: i32) -> User {
        let author = user(pool, nickname).await;
        for n in 0..count {
            let _posted = post(pool, &author, &format!("{nickname} post {n}")).await;
        }
        author
    }

    /// Renders `statuses` anonymously and returns the first entity's JSON.
    async fn render_first(state: &AppState, statuses: &[StatusRow]) -> Value {
        let rendered = render_statuses(state, None, statuses).await.unwrap();
        serde_json::to_value(&rendered[0]).unwrap()
    }

    #[tokio::test]
    async fn a_plain_status_renders_its_stored_facts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let status = insert(
            &pool,
            BASE,
            &NewStatus {
                language: Some("en".to_owned()),
                application_name: Some("Wired".to_owned()),
                application_website: Some("https://wired.example".to_owned()),
                visibility: Visibility::Local,
                ..NewStatus::local(&alice, "hello #world")
            },
        )
        .await
        .unwrap();

        let rendered = render_status(&state, None, &status).await.unwrap();
        let entity = rendered.expect("a live status renders");
        let value = serde_json::to_value(&entity).unwrap();
        assert_eq!(value["id"], status.id.to_string());
        assert_eq!(value["uri"], status.uri);
        assert_eq!(value["url"], json!(status.url));
        assert_eq!(value["content"], "<p>hello #world</p>");
        assert_eq!(
            value["visibility"], "local",
            "Pleroma's local visibility emits its literal value"
        );
        assert_eq!(value["account"]["acct"], "alice", "the author entity");
        assert_eq!(value["application"]["name"], "Wired");
        assert_eq!(value["application"]["website"], "https://wired.example");
        assert_eq!(value["language"], "en");
        assert_eq!(value["reblog"], Value::Null, "a root post nests nothing");
        assert_eq!(
            value["card"],
            Value::Null,
            "no card cached for this row renders null"
        );
        assert_eq!(
            value["pleroma"]["content"],
            json!({ "text/plain": "hello #world" }),
            "the raw text the author wrote"
        );
        assert_eq!(
            value["pleroma"]["conversation_id"],
            status.conversation_id.to_string()
        );
        assert_eq!(value["pleroma"]["local"], true);
        assert_eq!(value["pleroma"]["parent_visible"], true);
        assert_eq!(value["pleroma"]["emoji_reactions"], json!([]));
        assert_eq!(value["pleroma"]["quotes_count"], 0);
        assert_eq!(value["pleroma"]["expires_at"], Value::Null);
        assert_eq!(value["pleroma"]["direct_conversation_id"], Value::Null);
        assert_eq!(value["pleroma"]["bookmark_folder"], Value::Null);
        assert_eq!(value["pleroma"]["pinned_at"], Value::Null);
        assert_eq!(
            value["tags"],
            json!([{ "name": "world", "url": format!("{BASE}/tags/world") }]),
            "the hashtag the batch loader brought along"
        );
    }

    #[tokio::test]
    async fn replies_mentions_and_media_render_their_links() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let root = insert(
            &pool,
            BASE,
            &NewStatus {
                mentioned_user_ids: vec![bob.id],
                ..NewStatus::local(&alice, "@bob look")
            },
        )
        .await
        .unwrap();
        let reply = insert(
            &pool,
            BASE,
            &NewStatus {
                in_reply_to_id: Some(root.id),
                in_reply_to_account_id: Some(alice.id),
                ..NewStatus::local(&bob, "I will")
            },
        )
        .await
        .unwrap();
        let media = insert(&pool, BASE, &NewStatus::local(&bob, "with a picture"))
            .await
            .unwrap();
        let attachment = media_attachment::insert(
            &pool,
            &NewMediaAttachment {
                user_id: Some(bob.id),
                status_id: Some(media.id),
                url: "https://media.example/pic.png".to_owned(),
                content_type: "image/png".to_owned(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let page = [reply.clone(), media.clone()];
        let rendered = render_statuses(&state, None, &page).await.unwrap();
        assert_eq!(
            rendered_ids(&rendered),
            id_strings(&page),
            "input order, one entity per row"
        );

        let reply_json = serde_json::to_value(&rendered[0]).unwrap();
        assert_eq!(reply_json["in_reply_to_id"], root.id.to_string());
        assert_eq!(reply_json["in_reply_to_account_id"], alice.id.to_string());
        assert_eq!(
            reply_json["pleroma"]["in_reply_to_account_acct"], "alice",
            "the parent author's handle, from the same batched user read"
        );
        assert_eq!(
            reply_json["mentions"],
            json!([]),
            "this reply linked no mention row"
        );

        let media_json = serde_json::to_value(&rendered[1]).unwrap();
        assert_eq!(
            media_json["media_attachments"][0]["id"],
            attachment.id.to_string(),
            "the attached media renders"
        );
        assert_eq!(media_json["media_attachments"][0]["url"], attachment.url);

        // The mention on the root status renders through the same path.
        let root_json = render_first(&state, std::slice::from_ref(&root)).await;
        assert_eq!(
            root_json["mentions"],
            json!([{ "id": bob.id.to_string(), "username": "bob",
                     "url": format!("{BASE}/users/bob"), "acct": "bob" }]),
            "NewStatus::mentioned_user_ids linked bob, and the mention \
             loader brings him back"
        );
    }

    /// The `pleroma.emoji_reactions` array of a rendered entity.
    fn reactions(entity: &Status) -> Value {
        serde_json::to_value(entity).expect("a rendered status serializes")["pleroma"]
            ["emoji_reactions"]
            .clone()
    }

    /// Reacts to `status` as `reactor` with `emoji`, whose custom image is
    /// `url`. The model stores what it is handed; the endpoint is what
    /// validates the emoji, so a batch test needs no `custom_emojis` row.
    async fn react(
        pool: &PgPool,
        reactor: &User,
        status: &StatusRow,
        emoji: &str,
        url: Option<&str>,
    ) {
        let stored = emoji_reaction::react(
            pool,
            BASE,
            &NewReaction {
                user_id: reactor.id,
                status_id: status.id,
                emoji,
                url,
            },
        )
        .await
        .expect("the fixture reaction stores");
        assert_eq!(stored.emoji, emoji, "the fixture row names its emoji");
    }

    /// Favourites `status_id` as `liker`, asserting the write created its
    /// row — the state the flag test starts from.
    async fn favourite_it(pool: &PgPool, liker: &User, status_id: Uuid) {
        let created = favourite::favourite(pool, BASE, liker.id, status_id)
            .await
            .expect("the fixture favourite stores");
        assert!(created.is_some(), "the fixture pair had not favourited yet");
    }

    /// Bookmarks `status_id` as `who`, unfiled — the state the bookmark
    /// flag test starts from.
    async fn bookmark_it(pool: &PgPool, who: &User, status_id: Uuid) {
        let _filed = bookmark::add(pool, who.id, status_id, None)
            .await
            .expect("the fixture bookmark stores");
    }

    /// Files a fresh bookmark of `status_id` by `who` in `folder` — the
    /// state the `pleroma.bookmark_folder` test starts from.
    async fn bookmark_in(pool: &PgPool, who: &User, status_id: Uuid, folder: Uuid) {
        let filed = bookmark::add(pool, who.id, status_id, Some(folder))
            .await
            .expect("the fixture bookmark files");
        assert_eq!(filed.folder_id, Some(folder), "the row filed as asked");
    }

    /// The reactions ticket's loader inside the fixed query set: one page of
    /// three rows — two posts and a reblog of one of them — renders every
    /// roll-up from the same call, `me` follows the viewer, and the nested
    /// original of a reblog carries its own reactions.
    #[tokio::test]
    async fn the_reactions_roll_up_in_the_render_batch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let first = post(&pool, &alice, "first").await;
        let second = post(&pool, &alice, "second").await;
        let reblog = insert(
            &pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(second.id),
                ..NewStatus::local(&bob, "")
            },
        )
        .await
        .expect("a reblog of a stored post inserts");
        react(&pool, &alice, &first, THUMB, None).await;
        react(&pool, &bob, &first, CAT, Some(CAT_URL)).await;
        react(&pool, &bob, &second, THUMB, None).await;

        let page = [first.clone(), reblog.clone()];
        let for_bob = render_statuses(&state, Some(&bob), &page)
            .await
            .expect("the page renders for bob");
        let reblog_json = serde_json::to_value(&for_bob[1]).expect("the reblog serializes");
        assert!(
            !reblog_json["reblog"].is_null(),
            "the reblog row nests the original it reblogs"
        );
        assert_eq!(
            reblog_json["reblog"]["pleroma"]["emoji_reactions"],
            json!([{
                "name": THUMB, "count": 1, "me": true,
                "url": null, "account_ids": [bob.id.to_string()],
            }]),
            "the nested original's own roll-up, from the same batch"
        );
        assert_eq!(
            reactions(&for_bob[0]),
            json!([
                {
                    "name": THUMB,
                    "count": 1,
                    "me": false,
                    "url": null,
                    "account_ids": [alice.id.to_string()],
                },
                {
                    "name": CAT,
                    "count": 1,
                    "me": true,
                    "url": CAT_URL,
                    "account_ids": [bob.id.to_string()],
                },
            ]),
            "both of the first post's emojis, each flagged for the viewer \
             who reacted with one of them"
        );

        let anonymous = render_statuses(&state, None, std::slice::from_ref(&first))
            .await
            .expect("the post renders anonymously");
        assert_eq!(
            serde_json::to_value(&anonymous[0]).expect("it serializes")["pleroma"]
                ["emoji_reactions"]
                .as_array()
                .map(|entries| entries.iter().all(|entry| entry["me"] == json!(false))),
            Some(true),
            "an anonymous render is nobody's `me`"
        );
    }

    /// The favourites ticket's loader inside the fixed query set: one
    /// batched read answers the `favourited` flag of a whole page — plain
    /// rows, reblog rows (whose like lives on the original) and the nested
    /// originals alike — and an anonymous page is nobody's favourite.
    #[tokio::test]
    async fn the_favourited_flag_follows_one_batched_read() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let liked = post(&pool, &bob, "liked").await;
        let ignored = post(&pool, &bob, "ignored").await;
        let original = post(&pool, &bob, "original").await;
        let reblog = insert(
            &pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(original.id),
                ..NewStatus::local(&alice, "")
            },
        )
        .await
        .expect("a reblog of a stored post inserts");
        favourite_it(&pool, &alice, liked.id).await;
        favourite_it(&pool, &alice, original.id).await;

        let flag = |entity: &Status| {
            serde_json::to_value(entity).expect("a rendered status serializes")["favourited"]
                .clone()
        };
        let page = [liked.clone(), ignored.clone(), reblog.clone()];
        let for_alice = render_statuses(&state, Some(&alice), &page)
            .await
            .expect("the page renders for alice");
        assert_eq!(flag(&for_alice[0]), json!(true), "alice's own like shows");
        assert_eq!(
            flag(&for_alice[1]),
            json!(false),
            "and the unliked post does not"
        );
        let reblog_json = serde_json::to_value(&for_alice[2]).expect("the reblog serializes");
        assert_eq!(
            reblog_json["favourited"],
            json!(true),
            "the reblog row is flagged through its original, where the \
             like lives"
        );
        assert_eq!(
            reblog_json["reblog"]["favourited"],
            json!(true),
            "and the nested original carries its own flag"
        );

        let for_bob = render_statuses(&state, Some(&bob), std::slice::from_ref(&liked))
            .await
            .expect("the post renders for bob");
        assert_eq!(
            flag(&for_bob[0]),
            json!(false),
            "alice's like is nobody else's flag"
        );
        let anonymous = render_statuses(&state, None, &page)
            .await
            .expect("the page renders anonymously");
        assert!(
            anonymous.iter().all(|entity| flag(entity) == json!(false)),
            "an anonymous page is nobody's favourite"
        );
    }

    /// The reblogs ticket's loader inside the fixed query set: one batched
    /// read answers the `reblogged` flag of a whole page — the original the
    /// viewer boosted, a plain post they left alone, and, keyed through the
    /// original exactly like the favourites flag, the viewer's own reblog
    /// row and the original nested in it. An anonymous page is nobody's
    /// boost.
    #[tokio::test]
    async fn the_reblogged_flag_follows_one_batched_read() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let boosted = post(&pool, &bob, "boosted").await;
        let ignored = post(&pool, &bob, "ignored").await;
        let (reblog, created) =
            status::create_reblog(&pool, BASE, &alice, &boosted, Visibility::Public)
                .await
                .expect("alice boosts bob's post");
        assert!(created, "the boost is written by the reblog model");

        let flag = |entity: &Status| {
            serde_json::to_value(entity).expect("a rendered status serializes")["reblogged"].clone()
        };
        let page = [boosted.clone(), ignored.clone(), reblog.clone()];
        let for_alice = render_statuses(&state, Some(&alice), &page)
            .await
            .expect("the page renders for alice");
        assert_eq!(
            flag(&for_alice[0]),
            json!(true),
            "alice's own boost flags the original"
        );
        assert_eq!(
            flag(&for_alice[1]),
            json!(false),
            "and the unboosted post does not"
        );
        let reblog_json = serde_json::to_value(&for_alice[2]).expect("the reblog serializes");
        assert_eq!(
            reblog_json["reblogged"],
            json!(true),
            "the reblog row is flagged through its original, where the \
             boost lives — the shape POST …/reblog answers with"
        );
        assert_eq!(
            reblog_json["reblog"]["reblogged"],
            json!(true),
            "and the nested original carries its own flag"
        );

        let for_bob = render_statuses(&state, Some(&bob), std::slice::from_ref(&boosted))
            .await
            .expect("the post renders for bob");
        assert_eq!(
            flag(&for_bob[0]),
            json!(false),
            "alice's boost is nobody else's flag"
        );
        let anonymous = render_statuses(&state, None, &page)
            .await
            .expect("the page renders anonymously");
        assert!(
            anonymous.iter().all(|entity| flag(entity) == json!(false)),
            "an anonymous page is nobody's boost"
        );
    }

    /// The bookmarks ticket's loader inside the fixed query set: one
    /// batched read answers the `bookmarked` flag of a whole page — and,
    /// unlike the favourites flag, a reblog row is flagged from its *own*
    /// id, the row its client bookmarked, while the nested original stays
    /// unflagged unless it was itself bookmarked. An anonymous page is
    /// nobody's bookmark.
    #[tokio::test]
    async fn the_bookmarked_flag_follows_one_batched_read() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let saved = post(&pool, &bob, "saved").await;
        let ignored = post(&pool, &bob, "ignored").await;
        let original = post(&pool, &bob, "original").await;
        let reblog = insert(
            &pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(original.id),
                ..NewStatus::local(&alice, "")
            },
        )
        .await
        .expect("a reblog of a stored post inserts");
        bookmark_it(&pool, &alice, saved.id).await;
        bookmark_it(&pool, &alice, reblog.id).await;

        let flag = |entity: &Status| {
            serde_json::to_value(entity).expect("a rendered status serializes")["bookmarked"]
                .clone()
        };
        let page = [saved.clone(), ignored.clone(), reblog.clone()];
        let for_alice = render_statuses(&state, Some(&alice), &page)
            .await
            .expect("the page renders for alice");
        assert_eq!(flag(&for_alice[0]), json!(true), "alice's bookmark shows");
        assert_eq!(
            flag(&for_alice[1]),
            json!(false),
            "and the unbookmarked post does not"
        );
        let reblog_json = serde_json::to_value(&for_alice[2]).expect("the reblog serializes");
        assert_eq!(
            reblog_json["bookmarked"],
            json!(true),
            "the reblog row is flagged by its own id — a bookmark lands \
             where the client named it"
        );
        assert_eq!(
            reblog_json["reblog"]["bookmarked"],
            json!(false),
            "and the nested original, never bookmarked, is not"
        );

        let for_bob = render_statuses(&state, Some(&bob), std::slice::from_ref(&saved))
            .await
            .expect("the post renders for bob");
        assert_eq!(
            flag(&for_bob[0]),
            json!(false),
            "alice's bookmark is nobody else's flag"
        );
        let anonymous = render_statuses(&state, None, &page)
            .await
            .expect("the page renders anonymously");
        assert!(
            anonymous.iter().all(|entity| flag(entity) == json!(false)),
            "an anonymous page is nobody's bookmark"
        );
    }

    /// The folders ticket's half of the bookmarks read inside the fixed
    /// query set: one batched statement answers both the `bookmarked` flag
    /// and `pleroma.bookmark_folder` — a filed bookmark renders its folder
    /// id, an unfiled one and a foreign viewer render `null`, and, like the
    /// flag, the folder is keyed to the row's *own* id, so the original
    /// nested in a boost carries only its own bookmark's folder.
    #[tokio::test]
    async fn the_bookmark_folder_rides_the_batched_bookmark_read() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let folder = bookmark_folder::create(&pool, alice.id, "shelved", None)
            .await
            .expect("the fixture folder creates");
        let filed = post(&pool, &bob, "filed").await;
        let unfiled = post(&pool, &bob, "unfiled").await;
        bookmark_in(&pool, &alice, filed.id, folder.id).await;
        bookmark_it(&pool, &alice, unfiled.id).await;

        let folder_of = |entity: &Status| {
            serde_json::to_value(entity).expect("a rendered status serializes")["pleroma"]
                ["bookmark_folder"]
                .clone()
        };
        let page = [filed.clone(), unfiled.clone()];
        let for_alice = render_statuses(&state, Some(&alice), &page)
            .await
            .expect("the page renders for alice");
        assert_eq!(
            folder_of(&for_alice[0]),
            json!(folder.id.to_string()),
            "the filed bookmark renders its folder id"
        );
        assert_eq!(
            folder_of(&for_alice[1]),
            Value::Null,
            "the unfiled bookmark renders null while staying bookmarked"
        );
        assert_eq!(
            serde_json::to_value(&for_alice[1]).expect("it serializes")["bookmarked"],
            json!(true),
            "and the flag and the folder come from the one read"
        );

        let for_bob = render_statuses(&state, Some(&bob), std::slice::from_ref(&filed))
            .await
            .expect("the post renders for bob");
        assert_eq!(
            folder_of(&for_bob[0]),
            Value::Null,
            "alice's folder is nobody else's key"
        );
        let anonymous = render_statuses(&state, None, &page)
            .await
            .expect("the page renders anonymously");
        assert!(
            anonymous
                .iter()
                .all(|entity| folder_of(entity) == Value::Null),
            "an anonymous page carries nobody's folder"
        );
    }

    /// Pins `status_id` as `who`, asserting the write created its row — the
    /// state the pin flag test starts from.
    async fn pin_it(pool: &PgPool, who: &User, status_id: Uuid) {
        let created = pinned_status::pin(pool, who.id, status_id)
            .await
            .expect("the fixture pin stores");
        assert!(created.is_some(), "the fixture pair had not pinned yet");
    }

    /// The pinning ticket's loader inside the fixed query set: one batched
    /// read answers both the `pinned` flag and `pleroma.pinned_at` of a
    /// whole page — and, like the bookmarks flag, keyed to each row's *own*
    /// id, so a reblog of a pinned original renders unflagged while the
    /// nested original carries the pin. An anonymous page is nobody's pin.
    #[tokio::test]
    async fn the_pinned_flag_and_stamp_follow_one_batched_read() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let own = post(&pool, &alice, "alice's own").await;
        let second = post(&pool, &alice, "second").await;
        let reblog = insert(
            &pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(own.id),
                ..NewStatus::local(&bob, "")
            },
        )
        .await
        .expect("a reblog of a stored post inserts");
        pin_it(&pool, &alice, own.id).await;

        let stamped = pinned_status::pinned_ids(&pool, alice.id, &[own.id])
            .await
            .expect("the pin reads")
            .get(&own.id)
            .copied()
            .expect("alice pinned her own post");

        let flags = |entity: &Status| {
            let value = serde_json::to_value(entity).expect("a rendered status serializes");
            (
                value["pinned"].clone(),
                value["pleroma"]["pinned_at"].clone(),
            )
        };
        let page = [own.clone(), second.clone(), reblog.clone()];
        let for_alice = render_statuses(&state, Some(&alice), &page)
            .await
            .expect("the page renders for alice");
        assert_eq!(
            flags(&for_alice[0]),
            (json!(true), json!(mastodon_time(&stamped))),
            "alice's pin shows in the flag and in the stamp together"
        );
        assert_eq!(
            flags(&for_alice[1]),
            (json!(false), json!(null)),
            "the unpinned post carries neither"
        );
        let reblog_json = serde_json::to_value(&for_alice[2]).expect("the reblog serializes");
        assert_eq!(
            reblog_json["pinned"],
            json!(false),
            "bob's boost of her post is not alice's pin — a pin is keyed to \
             the row its account owns"
        );
        assert_eq!(
            reblog_json["reblog"]["pinned"],
            json!(true),
            "and the nested original carries the pin, from the same batch"
        );
        assert_eq!(
            reblog_json["reblog"]["pleroma"]["pinned_at"],
            json!(mastodon_time(&stamped)),
            "with its stamp"
        );

        let for_bob = render_statuses(&state, Some(&bob), std::slice::from_ref(&own))
            .await
            .expect("the post renders for bob");
        assert_eq!(
            flags(&for_bob[0]),
            (json!(false), json!(null)),
            "alice's pin is nobody else's flag"
        );
        let anonymous = render_statuses(&state, None, &page)
            .await
            .expect("the page renders anonymously");
        assert!(
            anonymous
                .iter()
                .all(|entity| flags(entity) == (json!(false), json!(null))),
            "an anonymous page is nobody's pin"
        );
    }

    /// The thread-mute ticket's loader inside the fixed query set: one
    /// batched read answers both the `muted` flag and `pleroma.thread_muted`
    /// of a whole page — keyed to each row's *own* `conversation_id`, so a
    /// reblog row and its nested original carry the same pair — and an
    /// anonymous page is nobody's mute.
    #[tokio::test]
    async fn the_thread_muted_flags_follow_one_batched_read() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let muted = post(&pool, &bob, "in a muted thread").await;
        let open = post(&pool, &bob, "in an open thread").await;
        let (boost, made) = status::create_reblog(&pool, BASE, &alice, &muted, Visibility::Public)
            .await
            .expect("a reblog of a stored post inserts");
        assert!(made, "the fixture boost is a fresh row");
        conversation_mute::mute(&pool, alice.id, muted.conversation_id)
            .await
            .expect("the fixture thread mute stores");

        let flags = |entity: &Status| {
            let value = serde_json::to_value(entity).expect("a rendered status serializes");
            (
                value["muted"].clone(),
                value["pleroma"]["thread_muted"].clone(),
            )
        };
        let page = [muted.clone(), open.clone(), boost.clone()];
        let for_alice = render_statuses(&state, Some(&alice), &page)
            .await
            .expect("the page renders for alice");
        assert_eq!(
            flags(&for_alice[0]),
            (json!(true), json!(true)),
            "alice's thread mute shows in both halves of her read"
        );
        assert_eq!(
            flags(&for_alice[1]),
            (json!(false), json!(false)),
            "the open thread carries neither"
        );
        let boost_json = serde_json::to_value(&for_alice[2]).expect("the reblog serializes");
        assert_eq!(
            boost_json["muted"],
            json!(true),
            "the reblog shares the muted thread's conversation, so it is \
             flagged too — one key, the row's own conversation id"
        );
        assert_eq!(
            boost_json["reblog"]["pleroma"]["thread_muted"],
            json!(true),
            "and the nested original carries the same pair"
        );

        let for_bob = render_statuses(&state, Some(&bob), std::slice::from_ref(&muted))
            .await
            .expect("the post renders for bob");
        assert_eq!(
            flags(&for_bob[0]),
            (json!(false), json!(false)),
            "alice's mute is nobody else's flag"
        );
        let anonymous = render_statuses(&state, None, &page)
            .await
            .expect("the page renders anonymously");
        assert!(
            anonymous
                .iter()
                .all(|entity| flags(entity) == (json!(false), json!(false))),
            "an anonymous page is nobody's mute"
        );
    }

    /// The conversation-participations ticket's loader inside the fixed
    /// query set: one batched read answers `pleroma.direct_conversation_id`
    /// of a whole page with each viewer's own participation id — the same
    /// DM thread renders a different id per participant, and a non-DM row,
    /// a non-participant and the anonymous request all render `null`.
    #[tokio::test]
    async fn the_direct_conversation_id_follows_one_batched_read() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let dm = insert(
            &pool,
            BASE,
            &NewStatus {
                visibility: Visibility::Direct,
                mentioned_user_ids: vec![bob.id],
                ..NewStatus::local(&alice, "@bob psst")
            },
        )
        .await
        .expect("the direct fixture inserts");
        conversation::participate_for_status(&pool, &dm)
            .await
            .expect("the fixture thread bump runs");
        // Re-read for the render: the page renderer keys the participation
        // to the row's stored `direct_conversation_id`, which the bump
        // writes after the insert's own answer.
        let dm = status::get_by_id(&pool, dm.id)
            .await
            .expect("the row reads")
            .expect("the fixture is live");
        let thread = dm
            .direct_conversation_id
            .expect("the bump linked the thread");
        let quiet = post(&pool, &bob, "public noise").await;

        let field = |entity: &Status| {
            serde_json::to_value(entity).expect("a rendered status serializes")["pleroma"]
                ["direct_conversation_id"]
                .clone()
        };
        let alice_expected = the_participation(&pool, &alice, thread).await;
        let bob_expected = the_participation(&pool, &bob, thread).await;
        let page = [dm.clone(), quiet.clone()];

        let for_alice = render_statuses(&state, Some(&alice), &page)
            .await
            .expect("the page renders for alice");
        assert_eq!(
            field(&for_alice[0]),
            json!(alice_expected),
            "the author sees her own participation id"
        );
        assert_eq!(
            field(&for_alice[1]),
            Value::Null,
            "and the public row on the same page stays unkeyed"
        );

        let for_bob = render_statuses(&state, Some(&bob), std::slice::from_ref(&dm))
            .await
            .expect("the post renders for bob");
        assert_eq!(
            field(&for_bob[0]),
            json!(bob_expected),
            "bob's view of the same DM names his participation, not alice's"
        );

        let for_carol = render_statuses(&state, Some(&carol), std::slice::from_ref(&dm))
            .await
            .expect("the post renders for carol");
        assert_eq!(
            field(&for_carol[0]),
            Value::Null,
            "a stranger to the thread renders null"
        );
        let anonymous = render_statuses(&state, None, &page)
            .await
            .expect("the page renders anonymously");
        assert!(
            anonymous.iter().all(|entity| field(entity) == Value::Null),
            "an anonymous page has no participation to name"
        );
    }

    /// `who`'s stored participation id in `thread`, as the wire spells it.
    async fn the_participation(pool: &PgPool, who: &User, thread: Uuid) -> String {
        conversation::participation_ids(pool, who.id, &[thread])
            .await
            .expect("the participation reads")
            .get(&thread)
            .copied()
            .expect("the fixture account participates")
            .to_string()
    }

    /// [`PollParams`] for the given option titles, nothing else set — the
    /// fixture polls' starting point.
    fn poll_params(titles: &[&str]) -> PollParams {
        PollParams {
            options: titles.iter().map(|title| (*title).to_owned()).collect(),
            ..PollParams::default()
        }
    }

    /// Creates a poll on `status_id` through a connection of the pool's
    /// own — the way a caller outside a transaction reaches
    /// [`poll::create`].
    async fn create_poll(pool: &PgPool, status_id: Uuid, params: &PollParams) -> poll::Poll {
        let mut conn = pool.acquire().await.expect("slot connection");
        poll::create(&mut conn, status_id, params)
            .await
            .expect("the fixture poll creates")
    }

    /// Records a mark straight into `poll_votes`: the vote endpoint that
    /// normally writes it (and moves the counters) is a later ticket, and
    /// the render batch's votes read needs rows to find.
    async fn vote(pool: &PgPool, poll_id: Uuid, option_id: Uuid, user_id: Uuid) {
        let inserted = sqlx::query(
            "INSERT INTO poll_votes (poll_id, option_id, user_id, inserted_at)
             VALUES ($1, $2, $3, now())",
        )
        .bind(poll_id)
        .bind(option_id)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("the fixture vote inserts");
        assert_eq!(inserted.rows_affected(), 1, "the mark was written");
    }

    /// Moves the stored roll-up counters the way the vote endpoint's
    /// transaction will — they are columns, not computed reads (see
    /// [`crate::models::poll`]) — to one voter, one vote, on `option_id`.
    async fn count_one_vote(pool: &PgPool, poll_id: Uuid, option_id: Uuid) {
        let moved = sqlx::query("UPDATE polls SET voters_count = 1, votes_count = 1 WHERE id = $1")
            .bind(poll_id)
            .execute(pool)
            .await
            .expect("the poll counters move");
        assert_eq!(moved.rows_affected(), 1, "the poll row was there to move");
        let option_moved = sqlx::query("UPDATE poll_options SET votes_count = 1 WHERE id = $1")
            .bind(option_id)
            .execute(pool)
            .await
            .expect("the option counter moves");
        assert_eq!(
            option_moved.rows_affected(),
            1,
            "the option row was there to move"
        );
    }

    /// A deadline a thousand years out: whatever the render instant is, a
    /// poll due then is still open, so `hide_totals` still withholds.
    fn far_future() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2999, 1, 1, 0, 0, 0).unwrap()
    }

    /// The open-poll fixture the render batch's poll tests share: alice's
    /// `tea or coffee` post carrying a multiple-choice poll, bob's mark on
    /// its second option, and the roll-ups moved to one voter and one vote
    /// the way the vote endpoint will.
    async fn open_poll_fixture(pool: &PgPool) -> (User, User, StatusRow, poll::Poll) {
        let alice = user(pool, "alice").await;
        let bob = user(pool, "bob").await;
        let polled = post(pool, &alice, "tea or coffee").await;
        let mut params = poll_params(&["tea", "coffee"]);
        params.multiple = true;
        let open = create_poll(pool, polled.id, &params).await;
        let choices = poll::options(pool, open.id).await.expect("options read");
        let coffee = choices.get(1).expect("the fixture poll has two options");
        vote(pool, open.id, coffee.id, bob.id).await;
        count_one_vote(pool, open.id, coffee.id).await;
        (alice, bob, polled, open)
    }

    /// The whole [`Poll`] entity the batch builds for the fixture poll as
    /// bob sees it: stored roll-ups, options in position order, and his
    /// mark at position 1 in `voted` and `own_votes`.
    ///
    /// [`Poll`]: crate::entities::poll::Poll
    fn expected_open_poll(open: &poll::Poll, voted: bool, own_votes: &[i32]) -> Value {
        json!({
            "id": open.id.to_string(),
            "expires_at": null,
            "expired": false,
            "multiple": true,
            "votes_count": 1,
            "voters_count": 1,
            "voted": voted,
            "own_votes": own_votes,
            "options": [
                {"title": "tea", "votes_count": 0},
                {"title": "coffee", "votes_count": 1},
            ],
            "emojis": [],
        })
    }

    /// The polls ticket's loaders inside the fixed query set: one page
    /// mixing a polled row, a plain one and a reblog of the polled one
    /// embeds each poll in its status — the full entity for the viewer who
    /// voted, `null` on the poll-less row and on the boost itself, and the
    /// original's poll nested in the boost, from the same batch.
    #[tokio::test]
    async fn the_poll_embeds_in_the_render_batch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let (alice, bob, polled, open) = open_poll_fixture(&pool).await;
        let plain = post(&pool, &alice, "nothing asked").await;
        let reblog = insert(
            &pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(polled.id),
                ..NewStatus::local(&bob, "")
            },
        )
        .await
        .expect("a reblog of a stored post inserts");
        let expected = expected_open_poll(&open, true, &[1]);

        let page = [polled.clone(), plain.clone(), reblog.clone()];
        let for_bob = render_statuses(&state, Some(&bob), &page)
            .await
            .expect("the page renders for bob");
        assert_eq!(
            serde_json::to_value(&for_bob[0]).expect("the polled status serializes")["poll"],
            expected,
            "the whole entity rides its status, key for key"
        );
        assert_eq!(
            serde_json::to_value(&for_bob[1]).expect("the plain status serializes")["poll"],
            Value::Null,
            "a poll-less row renders poll: null"
        );
        let reblog_json = serde_json::to_value(&for_bob[2]).expect("the reblog serializes");
        assert_eq!(
            reblog_json["poll"],
            Value::Null,
            "the boost row carries no poll of its own"
        );
        assert_eq!(
            reblog_json["reblog"]["poll"], expected,
            "the nested original brings its poll along from the same batch"
        );
    }

    /// The same batch for a viewer with no marks: the votes read runs only
    /// when there is one, so an anonymous render reports the poll voted by
    /// nobody — `voted: false`, `own_votes: []` — while every stored
    /// option and count renders unchanged.
    #[tokio::test]
    async fn an_anonymous_render_reports_the_page_poll_unvoted() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let (_alice, _bob, polled, open) = open_poll_fixture(&pool).await;

        let anonymous = render_statuses(&state, None, std::slice::from_ref(&polled))
            .await
            .expect("the poll renders anonymously");
        let value = serde_json::to_value(&anonymous[0]).expect("it serializes");
        assert_eq!(
            value["poll"],
            expected_open_poll(&open, false, &[]),
            "the whole entity again, minus nobody's marks"
        );
    }

    /// `hide_totals` inside the render batch: while an expired-less-due
    /// poll runs, its per-option counts come back JSON `null` even to the
    /// viewer who voted; the roll-up totals, the deadline and the viewer's
    /// own marks are not hidden totals and render as stored.
    #[tokio::test]
    async fn hide_totals_nulls_the_option_counts_while_the_poll_is_open() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let secret = post(&pool, &alice, "hush").await;
        let mut params = poll_params(&["yes", "no"]);
        params.expires_at = Some(far_future());
        params.hide_totals = true;
        let hidden = create_poll(&pool, secret.id, &params).await;
        let choices = poll::options(&pool, hidden.id).await.expect("options read");
        let yes = choices.first().expect("the fixture poll has an option");
        vote(&pool, hidden.id, yes.id, bob.id).await;

        let for_bob = render_statuses(&state, Some(&bob), std::slice::from_ref(&secret))
            .await
            .expect("the hidden-totals poll renders for bob");
        let value = serde_json::to_value(&for_bob[0]).expect("it serializes");
        assert_eq!(
            value["poll"]["options"],
            json!([
                {"title": "yes", "votes_count": null},
                {"title": "no", "votes_count": null},
            ]),
            "the per-option counts stay hidden while the poll is open"
        );
        assert_eq!(
            value["poll"]["votes_count"], 0,
            "the roll-up total renders as stored, hidden or not"
        );
        assert_eq!(
            value["poll"]["voted"],
            json!(true),
            "the viewer's own mark is not a hidden total"
        );
        assert_eq!(value["poll"]["own_votes"], json!([0]), "and it is his");
        assert_eq!(
            value["poll"]["expires_at"], "2999-01-01T00:00:00.000Z",
            "the deadline rides along"
        );
        assert_eq!(
            value["poll"]["expired"],
            json!(false),
            "due a thousand years out: the poll runs"
        );
        assert_eq!(value["poll"]["id"], hidden.id.to_string());
        assert_eq!(value["poll"]["multiple"], json!(false));
        assert_eq!(value["poll"]["voters_count"], 0);
    }

    /// The rich-media ticket's loader inside the fixed query set: one
    /// batched read keys each row's card to its *own* id — a reblog row
    /// renders its own card beside the original nested in it, a status
    /// linked only to a failed fetch renders null, and cards are nobody's
    /// private fact: the same row renders the same card for every viewer.
    #[tokio::test]
    async fn the_card_follows_one_batched_read() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let original = post(&pool, &alice, "https://example.test/post").await;
        let dead = post(&pool, &bob, "https://dead.test/").await;
        let (boost, made) = status::create_reblog(&pool, BASE, &bob, &original, Visibility::Public)
            .await
            .expect("a reblog of a stored post inserts");
        assert!(made, "the fixture boost is a fresh row");
        let card = json!({
            "url": "https://example.test/post",
            "title": "Example",
            "type": "link",
            "pleroma": { "opengraph": { "og:title": "Example" } },
        });
        link_card::store(&pool, "https://example.test/post", "ok", Some(&card))
            .await
            .expect("the fixture card stores");
        link_card::link_status(&pool, original.id, "https://example.test/post")
            .await
            .expect("the original links its card");
        let own = json!({"url": "https://boost.test", "title": "Boost page", "type": "link"});
        link_card::store(&pool, "https://boost.test", "ok", Some(&own))
            .await
            .expect("the boost's card stores");
        link_card::link_status(&pool, boost.id, "https://boost.test")
            .await
            .expect("the reblog row links its own card");
        link_card::store(&pool, "https://dead.test/", "failed", None)
            .await
            .expect("the failed fetch caches");
        link_card::link_status(&pool, dead.id, "https://dead.test/")
            .await
            .expect("the dead status links its failure");

        let card_of = |entity: &Status| {
            serde_json::to_value(entity).expect("a rendered status serializes")["card"].clone()
        };
        let page = [original.clone(), dead.clone(), boost.clone()];
        let anonymous = render_statuses(&state, None, &page)
            .await
            .expect("the page renders anonymously");
        assert_eq!(
            card_of(&anonymous[0]),
            card,
            "the original renders its card"
        );
        assert_eq!(
            card_of(&anonymous[1]),
            Value::Null,
            "a status linked only to a failed fetch renders null"
        );
        let boost_json = serde_json::to_value(&anonymous[2]).expect("the reblog serializes");
        assert_eq!(
            boost_json["card"], own,
            "the reblog row renders its own id's card — status_cards keys \
             on the row"
        );
        assert_eq!(
            boost_json["reblog"]["card"], card,
            "and the nested original its own, from the same batch"
        );

        let for_alice = render_statuses(&state, Some(&alice), std::slice::from_ref(&original))
            .await
            .expect("the post renders for alice");
        assert_eq!(
            card_of(&for_alice[0]),
            card,
            "the viewer's page renders the same card"
        );
    }

    /// Quotes `original` as `author` at `visibility`, straight through the
    /// model — the batch tests' quote fixture.
    async fn quote(
        pool: &PgPool,
        author: &User,
        original: &StatusRow,
        visibility: Visibility,
    ) -> StatusRow {
        insert(
            pool,
            BASE,
            &NewStatus {
                visibility,
                quote_of_id: Some(original.id),
                ..NewStatus::local(author, "quoting this")
            },
        )
        .await
        .expect("a quote of a stored status inserts")
    }

    /// The quoting ticket's nested render inside the fixed query set: the
    /// batch's one visibility read decides which quoted originals render,
    /// a quote nests exactly one level deep, an invisible one leaves
    /// `quote` null with `quote_visible` false while `quote_id` still
    /// names the target, and the quote's author rides the page's batch.
    #[tokio::test]
    async fn the_quote_nests_from_the_batch_with_its_visibility_read() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let source = post(&pool, &bob, "the source").await;
        let public_quote = quote(&pool, &carol, &source, Visibility::Public).await;
        let secret = insert(
            &pool,
            BASE,
            &NewStatus {
                visibility: Visibility::Private,
                ..NewStatus::local(&alice, "followers only")
            },
        )
        .await
        .expect("a private post inserts");
        // C ← B (alice's private quote) ← A (carol's public quote of B):
        // the depth chain, seeded past the endpoint's quotability rule.
        let deep = quote(&pool, &alice, &secret, Visibility::Private).await;
        let chain = quote(&pool, &carol, &deep, Visibility::Public).await;

        let anonymous = render_status(&state, None, &public_quote)
            .await
            .expect("the public quote renders")
            .expect("a live public row renders");
        let open = serde_json::to_value(&anonymous).expect("it serializes");
        assert_eq!(
            open["quote_id"],
            source.id.to_string(),
            "the hyphenated id of the quoted row (decision 4)"
        );
        assert_eq!(open["quote"]["id"], source.id.to_string());
        assert_eq!(
            open["quote"]["content"], "<p>the source</p>",
            "the nested entity is the quoted row's own render"
        );
        assert_eq!(
            open["quote"]["account"]["acct"], "bob",
            "the quote's author rode the page's user read"
        );
        assert_eq!(
            open["pleroma"]["quote"], open["quote"],
            "the pleroma mirror is the same object"
        );
        assert_eq!(open["pleroma"]["quote_visible"], json!(true));

        let for_owner = render_status(&state, Some(&alice), &chain)
            .await
            .expect("the chain renders for alice")
            .expect("and answers");
        let owned = serde_json::to_value(&for_owner).expect("it serializes");
        assert_eq!(
            owned["quote"]["id"],
            deep.id.to_string(),
            "alice sees her own private quote nested even though it is private"
        );
        assert_eq!(
            owned["quote"]["quote"],
            Value::Null,
            "depth 1: the quoted status's own quote never renders"
        );
        assert_eq!(owned["quote"]["quote_id"], secret.id.to_string());
        assert_eq!(owned["quote"]["pleroma"]["quote_visible"], json!(false));

        let anonymous_chain = render_status(&state, None, &chain)
            .await
            .expect("the chain renders anonymously")
            .expect("and answers");
        let quiet = serde_json::to_value(&anonymous_chain).expect("it serializes");
        assert_eq!(
            quiet["quote_id"],
            deep.id.to_string(),
            "the id names the invisible quote all the same"
        );
        assert_eq!(
            quiet["quote"],
            Value::Null,
            "alice's private row is invisible to the anonymous viewer"
        );
        assert_eq!(quiet["pleroma"]["quote"], Value::Null);
        assert_eq!(quiet["pleroma"]["quote_visible"], json!(false));
    }

    #[tokio::test]
    async fn twenty_statuses_by_five_authors_render_in_one_batch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let mut authors = Vec::new();
        for n in 0..5 {
            authors.push(author_with_posts(&pool, &format!("user{n}"), 4).await);
        }
        let all = stored_statuses(&pool).await;
        assert_eq!(all.len(), 20, "five authors with four posts each");
        let want = id_strings(&all);

        // The structural bound on the query set: one call over the whole
        // slice (ids in, entities out — no per-row round in this API)
        // renders all twenty rows, anonymous and authenticated alike. A
        // per-row loader would show up as twenty times the queries, and
        // this call makes exactly the fixed set of the module docs.
        let anonymous = render_statuses(&state, None, &all).await.unwrap();
        assert_eq!(
            rendered_ids(&anonymous),
            want,
            "every row of a 20-strong slice renders, in id order"
        );

        let viewer = &authors[0];
        let for_viewer = render_statuses(&state, Some(viewer), &all).await.unwrap();
        assert_eq!(for_viewer.len(), 20, "a viewer renders the same page");
        let value = serde_json::to_value(&for_viewer[0]).unwrap();
        assert_eq!(
            value["pleroma"]["local"], true,
            "the pleroma block rides along on every entry"
        );
    }

    /// The mixed page for the equivalence check below: a plain post with
    /// a mention and a tag, an answer to it, a reblog of it, and a remote
    /// mirror — every render branch in one batch.
    async fn mixed_page(pool: &PgPool) -> (User, Vec<StatusRow>) {
        let alice = user(pool, "alice").await;
        let bob = user(pool, "bob").await;
        let root = insert(
            pool,
            BASE,
            &NewStatus {
                mentioned_user_ids: vec![bob.id],
                ..NewStatus::local(&alice, "@bob hello #world")
            },
        )
        .await
        .unwrap();
        let answer = insert(
            pool,
            BASE,
            &NewStatus {
                in_reply_to_id: Some(root.id),
                in_reply_to_account_id: Some(alice.id),
                ..NewStatus::local(&bob, "hi back")
            },
        )
        .await
        .unwrap();
        let reblog = insert(
            pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(root.id),
                ..NewStatus::local(&bob, "")
            },
        )
        .await
        .unwrap();
        let mirror = insert(
            pool,
            BASE,
            &NewStatus {
                user_id: bob.id,
                uri: Some("https://remote.example/posts/mirror".to_owned()),
                content: "<p>mirrored</p>".to_owned(),
                ..NewStatus::default()
            },
        )
        .await
        .unwrap();
        let _attached = media_attachment::insert(
            pool,
            &NewMediaAttachment {
                user_id: Some(alice.id),
                status_id: Some(root.id),
                url: "https://media.example/pic.png".to_owned(),
                content_type: "image/png".to_owned(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        (alice, vec![root, answer, reblog, mirror])
    }

    /// The batch renderer and the single-row helper must agree entry for
    /// entry: one shared page of batch-loaded maps, then the same rows
    /// rendered one at a time, produce byte-identical JSON. That is the
    /// structural guarantee behind the fixed query set — the maps are
    /// keyed by id, so a row cannot leak its page-mates' media, mentions,
    /// tags or nested reblog into another entry.
    #[tokio::test]
    async fn the_batched_page_equals_row_by_row_rendering() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let (alice, page) = mixed_page(&pool).await;

        assert_page_equals_rows(&state, None, &page).await;
        assert_page_equals_rows(&state, Some(&alice), &page).await;
    }

    /// Batch-render `page` for `viewer` and compare entry for entry with
    /// the same rows rendered one at a time: the two paths must agree.
    async fn assert_page_equals_rows(state: &AppState, viewer: Option<&User>, page: &[StatusRow]) {
        let batched = render_statuses(state, viewer, page).await.unwrap();
        assert_eq!(batched.len(), 4, "every live row of the page renders");
        for (entry, row) in batched.iter().zip(page) {
            let alone = render_status(state, viewer, row)
                .await
                .unwrap()
                .expect("each row also renders alone");
            assert_eq!(
                serde_json::to_value(entry).unwrap(),
                serde_json::to_value(alone).unwrap(),
                "the batch entry of {} equals its single render for \
                 the same viewer",
                row.id
            );
        }
    }

    #[test]
    fn distinct_collapses_repeats_and_keeps_first_seen_order() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let ids = distinct([a, b, a, a, b]);
        assert_eq!(ids, vec![a, b], "one ask per id, in first-seen order");
        assert!(distinct(std::iter::empty()).is_empty(), "nothing in");
    }

    #[tokio::test]
    async fn a_reblog_nests_the_original_and_keeps_its_own_identity() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let original = insert(
            &pool,
            BASE,
            &NewStatus {
                spoiler_text: "cats".to_owned(),
                sensitive: true,
                ..NewStatus::local(&alice, "original text")
            },
        )
        .await
        .unwrap();
        let reblog = insert(
            &pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(original.id),
                ..NewStatus::local(&bob, "")
            },
        )
        .await
        .unwrap();

        let entity = render_status(&state, None, &reblog)
            .await
            .unwrap()
            .expect("a reblog of a live original renders");
        let value = serde_json::to_value(&entity).unwrap();

        assert_eq!(value["id"], reblog.id.to_string());
        assert_eq!(value["uri"], reblog.uri, "the reblog's own uri");
        assert_eq!(value["url"], json!(reblog.url), "and its own url");
        assert_eq!(value["content"], "", "the outer content is empty");
        assert_eq!(
            value["pleroma"]["content"],
            json!({ "text/plain": "" }),
            "a reblog has no source text of its own"
        );
        assert_eq!(value["account"]["acct"], "bob", "the reblogger speaks");
        let nested = &value["reblog"];
        assert_eq!(nested["id"], original.id.to_string());
        assert_eq!(nested["uri"], original.uri);
        assert_eq!(nested["content"], "<p>original text</p>");
        assert_eq!(nested["account"]["acct"], "alice");
        assert_eq!(nested["sensitive"], true, "the original keeps its flags");
        assert_eq!(nested["spoiler_text"], "cats");
        assert_eq!(
            nested["reblog"],
            Value::Null,
            "one level deep, Pleroma's non-recursive Announce arm"
        );

        assert_eq!(
            value["reblogs_count"], 0,
            "the reblog row's own counter, not the original's"
        );
        let counted: (i32,) = sqlx::query_as("SELECT reblogs_count FROM statuses WHERE id = $1")
            .bind(original.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(counted.0, 1, "the original is the one counted");
    }

    #[tokio::test]
    async fn tombstones_and_reblogs_of_gone_originals_are_skipped() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let keep = post(&pool, &alice, "kept").await;
        let doomed = post(&pool, &alice, "doomed").await;
        let orphan_reblog = insert(
            &pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(doomed.id),
                ..NewStatus::local(&bob, "")
            },
        )
        .await
        .unwrap();
        let tombstone = soft_delete(&pool, doomed.id)
            .await
            .unwrap()
            .expect("the fixture delete found the live post");
        assert!(tombstone.deleted_at.is_some(), "the row is a tombstone");

        let keep_id = keep.id.to_string();
        let rendered = render_statuses(&state, None, &[keep, tombstone.clone(), orphan_reblog])
            .await
            .unwrap();
        assert_eq!(
            rendered_ids(&rendered),
            [keep_id],
            "the tombstone and the reblog of a deleted original are both \
             skipped; whatever survives keeps its place"
        );
        assert!(
            render_status(&state, None, &tombstone)
                .await
                .unwrap()
                .is_none(),
            "the single helper answers None for a tombstone"
        );
    }

    #[tokio::test]
    async fn the_viewers_relationship_lands_in_the_account_entity() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let following = follow::follow(&pool, BASE, &bob, &alice)
            .await
            .expect("bob follows alice");
        assert_eq!(
            following.state,
            follow::FollowState::Accepted,
            "alice is unlocked, so the follow is granted"
        );
        let status = post(&pool, &alice, "public hello").await;

        let anonymous = render_first(&state, std::slice::from_ref(&status)).await;
        assert_eq!(
            anonymous["account"]["pleroma"]["relationship"],
            json!({}),
            "the anonymous viewer embeds Pleroma's empty relationship"
        );

        let for_bob = render_status(&state, Some(&bob), &status)
            .await
            .unwrap()
            .expect("it renders for bob too");
        let value = serde_json::to_value(&for_bob).unwrap();
        assert_eq!(
            value["account"]["pleroma"]["relationship"]["following"], true,
            "the batched relationship preload reaches the account entity"
        );

        let for_alice = render_status(&state, Some(&alice), &status)
            .await
            .unwrap()
            .expect("and for the author");
        let owner_view = serde_json::to_value(&for_alice).unwrap();
        assert_eq!(
            owner_view["account"]["pleroma"]["relationship"],
            json!({}),
            "Pleroma keeps the owner's own relationship out of the render"
        );
    }

    #[tokio::test]
    async fn remote_mirrors_render_their_stored_urls_and_text() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let mallory = user(&pool, "mallory").await;
        let published = Utc
            .with_ymd_and_hms(2024, 2, 1, 8, 0, 0)
            .unwrap()
            .trunc_subsecs(6);
        let status = insert(
            &pool,
            BASE,
            &NewStatus {
                user_id: mallory.id,
                uri: Some("https://remote.example/posts/1".to_owned()),
                url: Some("https://remote.example/notice/1".to_owned()),
                content: "<p>from elsewhere</p>".to_owned(),
                raw_text: String::new(),
                expires_at: Some(published),
                edited_at: Some(published),
                inserted_at: Some(published),
                language: Some("ko".to_owned()),
                ..NewStatus::default()
            },
        )
        .await
        .unwrap();

        let rendered = render_status(&state, None, &status)
            .await
            .unwrap()
            .expect("a mirror renders");
        let value = serde_json::to_value(&rendered).unwrap();
        assert_eq!(value["uri"], "https://remote.example/posts/1");
        assert_eq!(value["url"], "https://remote.example/notice/1");
        assert_eq!(value["pleroma"]["local"], false, "a mirror is not local");
        assert_eq!(
            value["pleroma"]["content"],
            json!({ "text/plain": "from elsewhere\n" }),
            "no stored source text: the plain variant strips the HTML, the \
             way strip_html renders a paragraph's end as a newline"
        );
        assert_eq!(value["pleroma"]["expires_at"], "2024-02-01T08:00:00.000Z");
        assert_eq!(value["edited_at"], "2024-02-01T08:00:00.000Z");
        assert_eq!(value["created_at"], "2024-02-01T08:00:00.000Z");
        assert_eq!(value["language"], "ko");
    }

    /// The two degenerate pages: an empty slice and a slice of nothing
    /// but tombstones both render empty, and the renderer returns before
    /// any loader (or the database) is needed.
    #[tokio::test]
    async fn empty_and_tombstone_pages_render_empty() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = user(&pool, "alice").await;
        let status = post(&pool, &alice, "gone").await;
        let tombstone = soft_delete(&pool, status.id)
            .await
            .unwrap()
            .expect("the post was live");

        let rendered = render_statuses(&state, None, &[]).await.unwrap();
        assert!(rendered.is_empty(), "nothing in answers nothing out");
        let tombstoned = render_statuses(&state, None, std::slice::from_ref(&tombstone))
            .await
            .unwrap();
        assert!(
            tombstoned.is_empty(),
            "a page of tombstones renders empty without loading anything"
        );
    }
}
