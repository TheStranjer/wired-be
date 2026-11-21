//! The batch-loaded parts a test render hands [`Status::from_source`]:
//! [`Parts`], one field per part the real batch would have loaded with a
//! setter for each, and the two helpers that build a [`StatusSource`] out
//! of it — [`render`] answering the entity as JSON, [`render_nested`]
//! answering the entity a reblog embeds.
//!
//! [`Status::from_source`]: crate::entities::status::Status::from_source
//! [`StatusSource`]: crate::entities::status::StatusSource

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use super::{CARD_URL, stamped};
use crate::{
    config::Config,
    entities::{
        account::JsonBool,
        poll::Poll,
        status::{Status, StatusSource},
    },
    models::{
        emoji_reaction::ReactionSummary as SummaryRow, hashtag::Hashtag, link_card::LinkCard,
        media_attachment::MediaAttachment as AttachmentRow, status::Status as StatusRow,
        user::User,
    },
};

/// The batch-loaded parts a test render supplies: whose row's author,
/// whose replied-to author, and the per-status lists.
#[derive(Debug)]
pub(in crate::entities::status) struct Parts<'a> {
    /// The row's author.
    author: &'a User,
    /// The replied-to author, when the row answers a status.
    reply_author: Option<&'a User>,
    /// The row's media attachments.
    media: Vec<AttachmentRow>,
    /// The row's mentioned accounts.
    mentions: Vec<User>,
    /// The row's hashtags.
    tags: Vec<Hashtag>,
    /// The row's emoji reactions.
    reactions: Vec<SummaryRow>,
    /// The row's cached link card, `None` when the batch's cards
    /// read found nothing: the row the rich-media batch would hand
    /// the renderer.
    card: Option<LinkCard>,
    /// Whether the viewer favourited the row.
    favourited: bool,
    /// Whether the viewer reblogged the row, in the same [`JsonBool`]
    /// wrapper the source field carries (the row's boost flags it).
    reblogged: JsonBool,
    /// Whether the viewer bookmarked the row.
    bookmarked: bool,
    /// The folder the viewer's bookmark files the row in, `None` for an
    /// unfiled bookmark or none at all — the folder the bookmarks-and-
    /// folder batch read would hand the renderer.
    bookmark_folder: Option<Uuid>,
    /// When the viewer pinned the row, `None` unless it is pinned: the
    /// pin stamp the favourites-style batch would hand the renderer.
    pinned_at: Option<DateTime<Utc>>,
    /// Whether the viewer muted the row's thread, in the same
    /// [`JsonBool`] wrapper the source field carries (the batch's
    /// thread-mute membership feeding both flags).
    muted: JsonBool,
    /// The row's rendered poll, in the same [`Option<Poll>`] the source
    /// field carries (what the polls batch's three reads would hand the
    /// renderer, already built).
    poll: Option<Poll>,
    /// The rendered quote, in the serialized shape the quoting
    /// ticket's batch hands the entity; `None` renders the quote
    /// null.
    quote: Option<Value>,
}

impl<'a> Parts<'a> {
    /// Parts with nothing loaded: `author` alone.
    pub(in crate::entities::status) fn of(author: &'a User) -> Self {
        Self {
            author,
            reply_author: None,
            media: Vec::new(),
            mentions: Vec::new(),
            tags: Vec::new(),
            reactions: Vec::new(),
            card: None,
            favourited: false,
            reblogged: JsonBool(false),
            bookmarked: false,
            bookmark_folder: None,
            pinned_at: None,
            muted: JsonBool(false),
            poll: None,
            quote: None,
        }
    }

    /// Sets [`reply_author`](Self::reply_author).
    pub(in crate::entities::status) fn by_reply_to(self, author: &'a User) -> Self {
        Self {
            reply_author: Some(author),
            ..self
        }
    }

    /// Sets [`mentions`](Self::mentions).
    pub(in crate::entities::status) fn mentioning(self, users: Vec<User>) -> Self {
        Self {
            mentions: users,
            ..self
        }
    }

    /// Sets [`tags`](Self::tags).
    pub(in crate::entities::status) fn tagged(self, tags: Vec<Hashtag>) -> Self {
        Self { tags, ..self }
    }

    /// Sets [`media`](Self::media).
    pub(in crate::entities::status) fn attached(self, media: Vec<AttachmentRow>) -> Self {
        Self { media, ..self }
    }

    /// Sets [`reactions`](Self::reactions).
    pub(in crate::entities::status) fn reacted(self, reactions: Vec<SummaryRow>) -> Self {
        Self { reactions, ..self }
    }

    /// Sets [`card`](Self::card) to a cached `ok` row holding `json`:
    /// the card the rich-media batch's [`for_statuses`] read would
    /// hand the renderer for a linked status.
    ///
    /// [`for_statuses`]: crate::models::link_card::for_statuses
    pub(in crate::entities::status) fn with_card(self, json: Value) -> Self {
        Self {
            card: Some(LinkCard {
                url: CARD_URL.to_owned(),
                status: "ok".to_owned(),
                card: Some(json),
                fetched_at: stamped(),
            }),
            ..self
        }
    }

    /// Sets [`card`](Self::card) to the `failed` row a dead URL
    /// leaves in the cache: linked, but holding no card.
    pub(in crate::entities::status) fn with_failed_card(self) -> Self {
        Self {
            card: Some(LinkCard {
                url: CARD_URL.to_owned(),
                status: "failed".to_owned(),
                card: None,
                fetched_at: stamped(),
            }),
            ..self
        }
    }

    /// Sets [`favourited`](Self::favourited): the viewer's flag, which
    /// the favourites ticket's batched read would hand the renderer.
    pub(in crate::entities::status) fn liked(self) -> Self {
        Self {
            favourited: true,
            ..self
        }
    }

    /// Sets [`reblogged`](Self::reblogged): the viewer's flag, which
    /// the reblogs ticket's batched read would hand the renderer.
    pub(in crate::entities::status) fn boosted(self) -> Self {
        Self {
            reblogged: JsonBool(true),
            ..self
        }
    }

    /// Sets [`bookmarked`](Self::bookmarked): the viewer's flag, which
    /// the bookmarks ticket's batched read would hand the renderer.
    pub(in crate::entities::status) fn bookmarked(self) -> Self {
        Self {
            bookmarked: true,
            ..self
        }
    }

    /// Sets [`bookmark_folder`](Self::bookmark_folder) (and the
    /// bookmark it rides): the folder the folders ticket's batched read
    /// would hand the renderer for a filed bookmark.
    pub(in crate::entities::status) fn filed_in(self, folder: Uuid) -> Self {
        Self {
            bookmarked: true,
            bookmark_folder: Some(folder),
            ..self
        }
    }

    /// Sets [`pinned_at`](Self::pinned_at): the viewer's own pin, which
    /// the pinning ticket's batched read would hand the renderer. The
    /// flag itself is membership in that same read, so it rides along
    /// with the stamp.
    pub(in crate::entities::status) fn pinned(self) -> Self {
        Self {
            pinned_at: Some(stamped()),
            ..self
        }
    }

    /// Sets [`muted`](Self::muted): the viewer's thread mute, which the
    /// thread-mute ticket's batched read would hand the renderer. One
    /// membership answers both the top-level flag and
    /// `pleroma.thread_muted`.
    pub(in crate::entities::status) fn muted_thread(self) -> Self {
        Self {
            muted: JsonBool(true),
            ..self
        }
    }

    /// Sets [`poll`](Self::poll): the built poll entity the polls
    /// ticket's batched reads would hand the renderer for a row that
    /// carries one.
    pub(in crate::entities::status) fn with_poll(self, poll: Poll) -> Self {
        Self {
            poll: Some(poll),
            ..self
        }
    }

    /// Sets [`quote`](Self::quote): the rendered quote, which the
    /// quoting ticket's batch hands the renderer for a quote this
    /// viewer may see.
    pub(in crate::entities::status) fn quoting(self, quote: Value) -> Self {
        Self {
            quote: Some(quote),
            ..self
        }
    }
}

/// `row` rendered as an anonymous viewer sees it, with `reblog`
/// attached as the nested entity when given.
pub(in crate::entities::status) fn render(
    row: &StatusRow,
    parts: &Parts<'_>,
    reblog: Option<Status>,
) -> Value {
    let config = Config::for_tests();
    let source = StatusSource {
        row,
        author: parts.author,
        reply_author: parts.reply_author,
        media: &parts.media,
        mentions: &parts.mentions,
        tags: &parts.tags,
        reactions: &parts.reactions,
        card: parts.card.as_ref(),
        favourited: parts.favourited,
        reblogged: parts.reblogged.clone(),
        bookmarked: parts.bookmarked,
        bookmark_folder: parts.bookmark_folder,
        pinned_at: parts.pinned_at,
        muted: parts.muted.clone(),
        direct_participation: None,
        quote: parts.quote.clone(),
        reblog: reblog.map(Box::new),
        poll: parts.poll.clone(),
    };
    serde_json::to_value(Status::from_source(source, None, None, &config)).unwrap()
}

/// The nested render of `row` by `author` — the reblog test builds its
/// original entity through here.
pub(in crate::entities::status) fn render_nested(row: &StatusRow, author: &User) -> Status {
    let config = Config::for_tests();
    Status::from_source(
        StatusSource {
            row,
            author,
            reply_author: None,
            media: &[],
            mentions: &[],
            tags: &[],
            reactions: &[],
            card: None,
            favourited: false,
            reblogged: JsonBool(false),
            bookmarked: false,
            bookmark_folder: None,
            pinned_at: None,
            muted: JsonBool(false),
            direct_participation: None,
            quote: None,
            reblog: None,
            poll: None,
        },
        None,
        None,
        &config,
    )
}
