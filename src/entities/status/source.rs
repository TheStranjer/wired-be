//! The [`StatusSource`] batch a render consumes: one `statuses` row plus
//! every part [`crate::services::render_status`] loaded for it, so the
//! entity's pure render never sees a database (decision 3).
//!
//! Each field is one batched read of a page, keyed to the row it feeds —
//! the comments say which read and which key of [`Status`] or
//! [`PleromaMetadata`] it answers. Later tickets add one field, and the
//! service's one loader feeding it, per rendered extension.

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use super::Status;
use crate::{
    entities::{account::JsonBool, poll::Poll},
    models::{
        custom_emoji::CustomEmoji as EmojiRow, emoji_reaction::ReactionSummary as SummaryRow,
        hashtag::Hashtag, link_card::LinkCard, media_attachment::MediaAttachment as AttachmentRow,
        status::Status as StatusRow, user::User,
    },
};

#[cfg(doc)]
use super::PleromaMetadata;
#[cfg(doc)]
use crate::entities::account::Account;

/// What a batch found behind a row's `quote_of_id`.
///
/// The `Linked*` variants are for a row that is itself rendered inside
/// another's quote: one level deep only the reference to its own quote
/// (`state` and `quoted_status_id`) is emitted, as Pleroma does. The
/// batch only loads the quote targets of the page's own rows, so a
/// nested reference can tell a hidden target from a visible one but
/// never a deleted one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuoteSlot {
    /// The quoted status is visible to the viewer; its serialized
    /// one-level-deep render.
    Visible(Value),
    /// The quoted row exists but the viewer may not see it.
    Hidden,
    /// The quoted row is gone.
    Deleted,
    /// The quoted author refused the quote request (`rejected`).
    Rejected,
    /// The quoted author withdrew the grant (`revoked`).
    Revoked,
    /// Nested reference to a quote the viewer may see (`accepted`).
    Linked,
    /// Nested reference to a quote hidden from the viewer
    /// (`unauthorized`).
    LinkedHidden,
}

impl QuoteSlot {
    /// The serialized quoted status, only for [`QuoteSlot::Visible`].
    #[must_use]
    pub fn into_rendered(self) -> Option<Value> {
        match self {
            Self::Visible(rendered) => Some(rendered),
            Self::Hidden
            | Self::Deleted
            | Self::Rejected
            | Self::Revoked
            | Self::Linked
            | Self::LinkedHidden => None,
        }
    }
}

/// One row plus every batch-loaded part its render needs, gathered by
/// [`crate::services::render_status`] with one query per part for the
/// whole page (no N+1).
///
/// [`Status::from_source`] consumes it. Later tickets add one field (and
/// the service's one loader feeding it) per rendered extension: the
/// viewer's flags for this row, its card, its poll, its emojis and its
/// rendered quote — the quoting ticket added [`quote`](Self::quote), the
/// polls ticket [`poll`](Self::poll); the render batch fills both.
#[derive(Debug)]
pub struct StatusSource<'a> {
    /// The `statuses` row being rendered.
    pub row: &'a StatusRow,
    /// [`row`](Self::row)'s author; becomes the entity's `account`.
    pub author: &'a User,
    /// The author of the status this one replies to, when
    /// [`in_reply_to_account_id`](StatusRow::in_reply_to_account_id)
    /// names a stored account; feeds `pleroma.in_reply_to_account_acct`.
    pub reply_author: Option<&'a User>,
    /// This row's media attachments, in `position` order.
    pub media: &'a [AttachmentRow],
    /// The accounts this row @-mentioned, in mention order.
    pub mentions: &'a [User],
    /// The hashtags this row carries, in name order.
    pub tags: &'a [Hashtag],
    /// The custom emojis this row's content, warning and poll options use,
    /// in the order they first appear: the rows of the batch's one
    /// [`custom_emojis`] read per author domain, enabled ones only. Feeds
    /// the entity's [`emojis`](Status::emojis); a row using none (and a
    /// preview that looked none up) comes in with the empty slice.
    ///
    /// [`custom_emojis`]: crate::models::custom_emoji
    pub emojis: &'a [EmojiRow],
    /// This row's emoji reactions, one roll-up per emoji in the roll-up's
    /// order; feeds `pleroma.emoji_reactions`. A row nobody reacted to (and a
    /// nested original nobody reacted to) comes in with the empty slice.
    pub reactions: &'a [SummaryRow],
    /// This row's rich-media card as the [`link_card`] cache stores it,
    /// `None` when the status links no card: the value of the one
    /// [`for_statuses`] read the batch runs for the page, keyed to the
    /// row's *own* id — `status_cards` keys on the row, so a reblog row
    /// renders its own id's card. [`Status::card`] answers the cached
    /// JSON verbatim; an absent row, a `failed` one and a row storing no
    /// card all render JSON `null`.
    ///
    /// [`link_card`]: crate::models::link_card
    /// [`for_statuses`]: crate::models::link_card::for_statuses
    pub card: Option<&'a LinkCard>,
    /// Whether the viewer favourited this row: membership in the one
    /// [`favourited_status_ids`](crate::models::favourite::favourited_status_ids)
    /// read the batch runs for the page. The renderer keys a reblog row's
    /// flag to its *original's* id — Pleroma's `Announce` arm computes
    /// `favourited` off the liked object — and `false` for an anonymous
    /// viewer, who has no rows.
    pub favourited: bool,
    /// Whether the viewer reblogged this row: membership in the one
    /// [`reblogged_status_ids`](crate::models::status::reblogged_status_ids)
    /// read the batch runs for the page, keyed — exactly like
    /// [`favourited`](Self::favourited) — to the id the boost lands on,
    /// which for a reblog row is its *original's*. So the outer entity of
    /// the viewer's own boost answers `reblogged: true`, as does the
    /// original itself; `false` for an anonymous viewer, who has no rows.
    /// A third plain `bool` in this struct would trip the denied
    /// `struct_excessive_bools`, so this flag rides the same [`JsonBool`]
    /// newtype [`Account`]'s flags do.
    pub reblogged: JsonBool,
    /// Whether the viewer bookmarked this row: membership in the one
    /// [`bookmarked_folders`](crate::models::bookmark::bookmarked_folders)
    /// read the batch runs for the page. The renderer keys every row's flag to
    /// its *own* id — a bookmark of a reblog id lands on the reblog row,
    /// Pleroma's `Bookmark.create` taking the activity the client named —
    /// and `false` for an anonymous viewer, who has no rows.
    pub bookmarked: bool,
    /// Which of the viewer's folders this row's bookmark is filed in, `None`
    /// for an unfiled bookmark or no bookmark at all: the value of that same
    /// [`bookmarked_folders`](crate::models::bookmark::bookmarked_folders)
    /// read entry, keyed to the row's *own* id like
    /// [`bookmarked`](Self::bookmarked) — one read answers both, so the flag
    /// and the folder can never disagree. Feeds
    /// [`pleroma.bookmark_folder`](PleromaMetadata::bookmark_folder).
    pub bookmark_folder: Option<Uuid>,
    /// When the viewer pinned this row to their own profile, `None` while
    /// they have pinned nothing: the value of the one
    /// [`pinned_ids`](crate::models::pinned_status::pinned_ids) read the
    /// batch runs for the page, keyed to the row's *own* id — only a post
    /// of one's own can be pinned, so there is no original to map through.
    /// [`Status::pinned`] is this stamp's presence and `pleroma.pinned_at`
    /// is its value, which is why the two halves can never disagree; an
    /// anonymous viewer has no rows and so no stamp.
    pub pinned_at: Option<DateTime<Utc>>,
    /// Whether the viewer muted this row's *thread*: membership in the one
    /// [`muted_conversation_ids`](crate::models::conversation_mute::muted_conversation_ids)
    /// read the batch runs for the page, keyed to the row's own
    /// [`conversation_id`](StatusRow::conversation_id) — a mute is a fact
    /// about the thread, not the post, so a reblog row and its nested
    /// original carry the same pair. This answers both the entity's
    /// `muted` and its `pleroma.thread_muted`, which is why the two can
    /// never disagree; `false` for an anonymous viewer, who has no mutes.
    /// A fourth plain `bool` in this struct would trip the denied
    /// `struct_excessive_bools`, so this flag rides the same [`JsonBool`]
    /// wrapper [`reblogged`](Self::reblogged) chose.
    pub muted: JsonBool,
    /// The viewer's `conversation_participations` row for this row's DM
    /// thread, `None` when the row belongs to no thread or the viewer is
    /// no participant: the value of the one
    /// [`participation_ids`](crate::models::conversation::participation_ids)
    /// read the batch runs for the page, keyed to the row's own
    /// [`direct_conversation_id`](StatusRow::direct_conversation_id) and
    /// answering `pleroma.direct_conversation_id`. An anonymous viewer has
    /// no rows and so no participation.
    pub direct_participation: Option<Uuid>,
    /// What the batch found behind this row's quote, as a [`QuoteSlot`]:
    /// [`crate::services::render_status`]'s nested quote render handed
    /// over so this layer stays pure (decision 3), one level deep — the
    /// quoted status's own quote is only a [`QuoteSlot::Linked`]
    /// reference. `None` when the row quotes nothing (a row that does
    /// quote but carries `None` renders as a deleted quote). Feeds the
    /// Mastodon [`quote`](Status::quote) object; only a
    /// [`QuoteSlot::Visible`] feeds `pleroma.quote`, whose
    /// `pleroma.quote_visible` is exactly that slot's presence.
    pub quote: Option<QuoteSlot>,
    /// The already-rendered original, boxed, when [`row`](Self::row) is
    /// a reblog; `None` for anything else. The nested entity is rendered
    /// from the original's own `StatusSource`, one level deep — Pleroma's
    /// recursion into `show.json` for an `Announce`.
    pub reblog: Option<Box<Status>>,
    /// The row's poll, already rendered by [`crate::services::render_status`]
    /// from the batch's poll, option and vote reads of
    /// [`crate::models::poll`] (see [`Poll::build`]);
    /// `None` for a row the batch found no poll on, and for a preview that
    /// has stored nothing. Feeds the entity's [`poll`](Status::poll) key.
    pub poll: Option<Poll>,
    /// Whether the viewer may see the status this row replies to: `true`
    /// for a root post, and for a reply whose parent the batch's one
    /// [`filter_visible`](crate::models::status_visibility::filter_visible)
    /// read over the page's parents admitted; `false` for a reply whose
    /// parent is hidden from the viewer or gone. Feeds
    /// [`pleroma.parent_visible`](PleromaMetadata::parent_visible). It
    /// rides [`JsonBool`] because a third plain `bool` in this struct would
    /// trip the denied `struct_excessive_bools`.
    pub parent_visible: JsonBool,
}
