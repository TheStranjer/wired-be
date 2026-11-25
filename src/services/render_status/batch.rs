//! The batch context of a page render: [`Batch`] runs the fixed query set
//! once for a page and then renders every row from the loaded maps, so a
//! row's render is a lookup, never a query.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use tracing::debug;
use uuid::Uuid;

use crate::{
    config::Config,
    entities::{
        account::JsonBool,
        poll::Poll as PollEntity,
        relationship::{Relationship, RelationshipMap},
        status::{QuoteSlot, Status, StatusSource},
    },
    error::AppResult,
    models::{
        custom_emoji::CustomEmoji as EmojiRow,
        emoji_reaction::ReactionSummary,
        hashtag::Hashtag,
        link_card::LinkCard,
        media_attachment::MediaAttachment as AttachmentRow,
        poll::{Poll as PollRow, PollOption as PollOptionRow},
        status::Status as StatusRow,
        user::User,
    },
    services::account_emojis::AccountEmojis,
    state::AppState,
};

use super::{
    ViewerMode,
    loaders::{
        load_bookmarks, load_direct_participations, load_favourites, load_media, load_mentions,
        load_pins, load_quote_denials, load_quotes, load_reactions, load_reblogs,
        load_statuses_by_id, load_tags, load_thread_mutes, load_users, load_visible_parents,
    },
    loaders_emojis::{load_row_emojis, rendered_rows},
    loaders_polls_cards::{load_cards, load_poll_options, load_poll_votes, load_polls},
    page_ids::{PageIds, distinct},
};

/// The batch-loaded parts of one page: every map keyed by the id its
/// loader was asked about, so a row's render is a lookup, never a query.
#[derive(Debug)]
pub(super) struct Batch<'a> {
    /// The instance configuration the [`Account`] and tag URLs render
    /// from.
    ///
    /// [`Account`]: crate::entities::account::Account
    config: &'a Config,
    /// The account whose view is being rendered, if any.
    viewer: Option<&'a User>,
    /// Every author and replied-to author on the page, keyed by id.
    users: HashMap<Uuid, User>,
    /// The custom emojis each rendered row uses, keyed by status id in the
    /// order the row's texts first use them — one
    /// [`custom_emoji::get_by_shortcodes`] read per distinct author domain,
    /// none when no text holds a shortcode. A row using none has no entry.
    ///
    /// [`custom_emoji::get_by_shortcodes`]: crate::models::custom_emoji::get_by_shortcodes
    emojis: HashMap<Uuid, Vec<EmojiRow>>,
    /// The custom emojis the page's accounts use in their profiles, for the
    /// embedded `account` of every rendered status — one
    /// [`custom_emoji::get_by_shortcodes`] read per distinct account domain,
    /// none when no profile text holds a shortcode.
    ///
    /// [`custom_emoji::get_by_shortcodes`]: crate::models::custom_emoji::get_by_shortcodes
    account_emojis: AccountEmojis,
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
    /// The page's quoting rows whose quoted author refused or revoked the
    /// quote, keyed by the quoting row's own id. Such a quote renders its
    /// `rejected` or `revoked` state in place of whatever the quoted post's
    /// visibility would say.
    quote_denials: HashMap<Uuid, QuoteSlot>,
    /// Which of the statuses the page (and its nested rows) reply to this
    /// viewer may see — one
    /// [`filter_visible`](crate::models::status_visibility::filter_visible)
    /// over the loaded parents, run once per page and skipped when nothing on it is
    /// a reply. A parent outside this set (hidden, or gone) makes its
    /// replies render `pleroma.parent_visible` false; a root post has no
    /// parent and renders it true.
    visible_parents: HashSet<Uuid>,
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
    /// Runs the fixed query set of the [module docs](super) for `rows` and
    /// keeps the results together. Every loader is handed its id list
    /// deduplicated, and each one skips the database when the list is
    /// empty.
    pub(super) async fn load(
        state: &'a AppState,
        mode: ViewerMode<'a>,
        rows: &[&'a StatusRow],
    ) -> AppResult<Self> {
        let pool = &state.db;
        let viewer = mode.viewer();
        let reblog_ids = distinct(rows.iter().copied().filter_map(|row| row.reblog_of_id));
        let originals = load_statuses_by_id(pool, &reblog_ids).await?;
        let quote_ids = distinct(rows.iter().copied().filter_map(|row| row.quote_of_id));
        let (quoted, visible_quotes) = load_quotes(pool, &quote_ids, mode).await?;
        let quoting_ids = distinct(
            rows.iter()
                .copied()
                .chain(originals.values())
                .filter(|row| row.quote_of_id.is_some())
                .map(|row| row.id),
        );
        let quote_denials = load_quote_denials(pool, &quoting_ids).await?;
        let ids = PageIds::of(rows, &originals, &quoted);
        let visible_parents = load_visible_parents(pool, &ids.parents, mode).await?;
        let polls = load_polls(pool, &ids.rendered).await?;
        let poll_options = load_poll_options(pool, &ids.rendered).await?;
        let poll_votes = load_poll_votes(pool, &ids.rendered, viewer).await?;
        debug!(
            polls = polls.len(),
            options = poll_options.len(),
            votes = poll_votes.len(),
            "polls batch-loaded with their options and the viewer's marks"
        );
        let users = load_users(pool, &ids.accounts).await?;
        let shown = rendered_rows(rows, &originals, &quoted, &visible_quotes);
        let emojis = load_row_emojis(pool, &shown, &users, &poll_options).await?;
        let authors: Vec<&User> = users.values().collect();
        let account_emojis = AccountEmojis::load(pool, &authors).await?;
        debug!(
            rows = shown.len(),
            with_emojis = emojis.len(),
            "custom emojis batch-loaded for the rows that use them"
        );
        Ok(Self {
            config: state.config.as_ref(),
            viewer,
            users,
            emojis,
            account_emojis,
            originals,
            quoted,
            visible_quotes,
            quote_denials,
            visible_parents,
            media: load_media(pool, &ids.rendered).await?,
            mentions: load_mentions(pool, &ids.rendered).await?,
            tags: load_tags(pool, &ids.rendered).await?,
            reactions: load_reactions(pool, &ids.rendered, viewer).await?,
            favourites: load_favourites(pool, &ids.rendered, viewer).await?,
            reblogs: load_reblogs(pool, &ids.rendered, viewer).await?,
            bookmarks: load_bookmarks(pool, &ids.rendered, viewer).await?,
            pins: load_pins(pool, &ids.rendered, viewer).await?,
            thread_mutes: load_thread_mutes(pool, &ids.conversations, viewer).await?,
            direct_participations: load_direct_participations(pool, &ids.directs, viewer).await?,
            polls,
            poll_options,
            poll_votes,
            relationships: Relationship::load_map(pool, viewer, &ids.authors).await?,
            cards: load_cards(pool, &ids.rendered).await?,
        })
    }

    /// Renders every row of `rows` in order, nesting the original of a
    /// reblog and the quoted original of a quote, and dropping the rows
    /// the [`render_statuses`] contract cannot answer for.
    ///
    /// [`render_statuses`]: super::render_statuses
    pub(super) fn render_all(&self, rows: &[&StatusRow]) -> Vec<Status> {
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

    /// What `row`'s quote resolves to, `None` when it quotes nothing.
    /// [`QuoteSlot::Visible`] carries the nested render when
    /// [`row.quote_of_id`](StatusRow::quote_of_id) names a row the batch
    /// loaded (live: [`status::get_many`] answers tombstones not at all),
    /// the visibility read admitted to [`Batch::visible_quotes`], and the
    /// maps can render (an author the batch knows). That render is one
    /// level deep — Pleroma's `show_quote: false` on the quote's own
    /// rendering — so the quoted row carries only a
    /// [`Batch::linked_quote`] reference to whatever it quotes.
    /// [`QuoteSlot::Hidden`] is a loaded row the viewer may not see and
    /// [`QuoteSlot::Deleted`] an id naming nothing the batch found;
    /// the entity renders them `unauthorized` and `deleted` with
    /// `pleroma.quote_visible` false while `quote_id` still names the
    /// target.
    fn render_quote(&self, row: &StatusRow) -> Option<QuoteSlot> {
        let quoted_id = row.quote_of_id?;
        if let Some(denied) = self.quote_denials.get(&row.id) {
            return Some(denied.clone());
        }
        let quoted = self.quoted.get(&quoted_id);
        let Some(quoted) = quoted else {
            return Some(QuoteSlot::Deleted);
        };
        if !self.visible_quotes.contains(&quoted.id) {
            return Some(QuoteSlot::Hidden);
        }
        let rendered = self
            .render_entity(quoted, None, self.linked_quote(quoted))
            .and_then(|entity| serde_json::to_value(entity).ok());
        Some(rendered.map_or(QuoteSlot::Deleted, QuoteSlot::Visible))
    }

    /// The reference a quoted status carries to its own quote, `None`
    /// when it quotes nothing. Only the page rows' quote targets are
    /// loaded, so a target the batch has not seen reads as visible.
    fn linked_quote(&self, quoted: &StatusRow) -> Option<QuoteSlot> {
        let target = quoted.quote_of_id?;
        let hidden = self.quoted.contains_key(&target) && !self.visible_quotes.contains(&target);
        Some(if hidden {
            QuoteSlot::LinkedHidden
        } else {
            QuoteSlot::Linked
        })
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
    /// `quote` is the batch's [`Batch::render_quote`] slot —
    /// `pleroma.quote` mirrors a visible one's render and
    /// `pleroma.quote_visible` is its presence, which is why the three
    /// can never disagree, while `quote_id` names the target whatever
    /// the slot answered. The
    /// `pleroma.parent_visible` flag is `true` for a row that replies to
    /// nothing and otherwise membership of the parent's id in the page's
    /// visible-parents set — the viewer's access to the parent, which the
    /// page filter on the rendered row says nothing about.
    fn render_entity(
        &self,
        row: &StatusRow,
        reblog: Option<Box<Status>>,
        quote: Option<QuoteSlot>,
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
            emojis: self.emojis.get(&row.id).map_or(&[], Vec::as_slice),
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
            parent_visible: JsonBool(
                row.in_reply_to_id
                    .is_none_or(|parent| self.visible_parents.contains(&parent)),
            ),
        };
        let mut entity = Status::from_source(
            source,
            self.viewer,
            self.relationships.as_ref(),
            self.config,
        );
        entity.account = self
            .account_emojis
            .fill(entity.account, author, self.config);
        Some(entity)
    }

    /// The [`Poll`] entity of `row`: `None` when the batch's poll read
    /// found nothing for it, and otherwise [`Poll::build`] over the row,
    /// its options in position order and the viewer's marks — the empty
    /// slice for an anonymous viewer, whose every poll renders `voted:
    /// false` with `own_votes: []`. The render instant decides `expired`
    /// and, through it, whether `hide_totals` still withholds the option
    /// counts. Its `emojis` are the row's emoji list narrowed to those the
    /// option titles use ([`Poll::with_emojis`]).
    ///
    /// [`Poll::with_emojis`]: crate::entities::poll::Poll::with_emojis
    /// [`Poll`]: crate::entities::poll::Poll
    /// [`Poll::build`]: crate::entities::poll::Poll::build
    fn render_poll(&self, row: &StatusRow) -> Option<PollEntity> {
        let poll = self.polls.get(&row.id)?;
        let entity = PollEntity::build(
            poll,
            self.poll_options.get(&row.id).map_or(&[], Vec::as_slice),
            self.poll_votes.get(&row.id).map_or(&[], Vec::as_slice),
            Utc::now(),
        )
        .with_author_view(self.viewer.is_some_and(|viewer| viewer.id == row.user_id));
        Some(entity.with_emojis(
            self.emojis.get(&row.id).map_or(&[], Vec::as_slice),
            self.config,
        ))
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        models::{
            media_attachment::{self, NewMediaAttachment},
            status::{NewStatus, insert},
        },
        services::render_status::{
            render_status, render_statuses,
            test_support::{BASE, test_state, user},
        },
    };

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

        assert_page_equals_rows(&state, ViewerMode::ANONYMOUS, &page).await;
        assert_page_equals_rows(&state, ViewerMode::user(&alice), &page).await;
    }

    /// Batch-render `page` for `viewer` and compare entry for entry with
    /// the same rows rendered one at a time: the two paths must agree.
    async fn assert_page_equals_rows(state: &AppState, viewer: ViewerMode<'_>, page: &[StatusRow]) {
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
}
