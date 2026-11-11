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
//! - `quote_id`, `quote_url` and `quote_visible` are not emitted at all.
//!
//! Keys a later ticket fills in, each from exactly one more batched
//! loader in [`crate::services::render_status`] (see its module docs):
//! `emojis` (custom emojis);
//! and `quote` (the quoting ticket, whose original rows the service
//! already batch-loads).
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
//!
//! [`ReactionSummary`]: crate::entities::emoji_reaction::ReactionSummary

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    config::Config,
    entities::{
        account::{Account, AccountViewer, JsonBool},
        emoji_reaction::ReactionSummary,
        filter::FilterResult,
        media_attachment::MediaAttachment,
        mention::Mention,
        poll::Poll,
        relationship::RelationshipMap,
        tag::Tag,
    },
    models::{
        emoji_reaction::ReactionSummary as SummaryRow, hashtag::Hashtag, link_card::LinkCard,
        media_attachment::MediaAttachment as AttachmentRow, status::Status as StatusRow,
        user::User,
    },
    services::rich_media::stored_card_json,
    util::sanitize::strip_html,
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

/// The `pleroma` extension object of a [`Status`]: the Pleroma-specific
/// keys of `StatusView.render("show.json", …)`.
///
/// `quote_id`, `quote_url` and `quote_visible` are not part of this
/// shape (ticket decision); `quote` lives at the entity's top level until
/// the quoting ticket renders it.
#[derive(Clone, Debug, Serialize)]
pub struct PleromaMetadata {
    /// `content`: the post as its author wrote it — see the renderer's
    /// plain-text rule for where the value comes from: the stored
    /// [`raw_text`](StatusRow::raw_text), falling back to
    /// [`strip_html`] of the content when no source text was stored.
    pub content: PlainText,
    /// `spoiler_text`: the content warning as stored, in the same
    /// `{"text/plain": …}` map (Pleroma's `summary`, and `""` when there
    /// is none).
    pub spoiler_text: PlainText,
    /// `conversation_id`: the row's thread id as the hyphenated lowercase
    /// `UUIDv7` string (decision 4) — this instance's replacement for
    /// Pleroma's deprecated CRC32 context id.
    pub conversation_id: String,
    /// `direct_conversation_id`: the id of the viewer's
    /// [`Participation`](crate::models::conversation::Participation) in
    /// the DM thread this status belongs to, as the hyphenated lowercase
    /// `UUIDv7` text (decision 4), or `null` — the value of the same
    /// [`participation_ids`](crate::models::conversation::participation_ids)
    /// read the batch runs per page, keyed to the row's
    /// [`direct_conversation_id`](StatusRow::direct_conversation_id). A
    /// status of no DM thread, an anonymous viewer and a stranger to the
    /// thread all render `null`; only a participant ever sees a value.
    pub direct_conversation_id: Option<String>,
    /// `in_reply_to_account_acct`: the handle of the replied-to status's
    /// author as stored (decision 7), or `null` when this status answers
    /// nothing, or this instance does not know that author.
    pub in_reply_to_account_acct: Option<String>,
    /// `local`: whether this instance owns the post.
    pub local: JsonBool,
    /// `expires_at`: when the post is due to disappear, in the Mastodon
    /// time format (decision 5), or `null` — Pleroma's `Purge` job
    /// schedule, here the stored [`expires_at`](StatusRow::expires_at)
    /// column.
    #[serde(with = "crate::util::time::serde_mastodon_time::option")]
    pub expires_at: Option<DateTime<Utc>>,
    /// `thread_muted`: whether the viewer muted this post's whole thread;
    /// the thread-mute ticket's batched
    /// [`muted_conversation_ids`](crate::models::conversation_mute::muted_conversation_ids)
    /// read answers it — the same fact as the top-level `muted`, keyed to
    /// the row's `conversation_id`, so the two always agree.
    pub thread_muted: JsonBool,
    /// `emoji_reactions`: the post's reactions, one entry per emoji in the
    /// roll-up's order (the emoji's first reaction, then the emoji text),
    /// empty for a post nobody reacted to. [`crate::services::render_status`]
    /// batch-loads the whole page's share of them in one read of
    /// [`emoji_reaction::for_statuses`](crate::models::emoji_reaction::for_statuses).
    pub emoji_reactions: Vec<ReactionSummary>,
    /// `parent_visible`: whether the viewer may see the replied-to
    /// status. `true`: callers hand whole pages already narrowed by
    /// [`crate::models::status_visibility`], and the rows this crate's
    /// renderer itself drops (tombstones, reblogs of gone originals)
    /// never reach the wire, so every rendered reply's parent is visible
    /// under that contract.
    pub parent_visible: JsonBool,
    /// `quotes_count`: the stored counter of statuses quoting this one.
    /// Pleroma emits it at the top level as well; here it belongs to the
    /// extension block only.
    pub quotes_count: i32,
    /// `bookmark_folder`: the id of the folder the viewer's bookmark of
    /// this post is filed in, as the hyphenated lowercase `UUIDv7` text
    /// (decision 4), or `null` — the value of the same
    /// [`bookmarked_folders`](crate::models::bookmark::bookmarked_folders)
    /// read that answers [`Status::bookmarked`] (the folders ticket),
    /// which is why only a bookmarked post can ever carry a folder here.
    pub bookmark_folder: Option<String>,
    /// `pinned_at`: when the viewer pinned this post to their own profile,
    /// in the Mastodon time format (decision 5), or `null` — the value of
    /// the same [`pinned_ids`](crate::models::pinned_status::pinned_ids)
    /// read that answers [`Status::pinned`], which is why a post that
    /// renders `pinned: true` always carries its pin's timestamp here too.
    #[serde(with = "crate::util::time::serde_mastodon_time::option")]
    pub pinned_at: Option<DateTime<Utc>>,
}

/// One row plus every batch-loaded part its render needs, gathered by
/// [`crate::services::render_status`] with one query per part for the
/// whole page (no N+1).
///
/// [`Status::from_source`] consumes it. Later tickets add one field (and
/// the service's one loader feeding it) per rendered extension: the
/// viewer's flags for this row, its card, its emojis, its rendered quote.
/// The polls ticket added [`poll`](Self::poll); the render batch fills it.
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
}

/// The Mastodon `Status` entity (core fields plus the `pleroma` block).
///
/// Build one with [`Status::from_source`]; its [`Serialize`] impl emits
/// exactly the keys listed in the [module docs](self). Presence: every
/// key is always emitted — the nullable ones (`url`, `application`,
/// `reblog`, `in_reply_to_id`, `in_reply_to_account_id`, `language`,
/// `card`, `poll`, `edited_at`, `quote` and the `pleroma` nulls) render
/// JSON `null` rather than disappearing, as Pleroma's renderer sends
/// them.
#[derive(Debug, Serialize)]
pub struct Status {
    /// The status id as the hyphenated lowercase `UUIDv7` string
    /// (decision 4).
    pub id: String,
    /// The `ActivityPub` object id of this row (decision 6) — for a
    /// reblog, the reblog's own object id, not the original's.
    pub uri: String,
    /// The human-facing URL of this row, or `null` when its origin named
    /// none.
    pub url: Option<String>,
    /// When the post was created (the remote `published` time for a
    /// mirror), in the Mastodon time format (decision 5). For a reblog,
    /// when the reblog happened.
    #[serde(with = "crate::util::time::serde_mastodon_time")]
    pub created_at: DateTime<Utc>,
    /// The author, as the [`Account`] entity — including
    /// `pleroma.relationship`, the viewer's relationship to the author
    /// when one was preloaded (see [`Status::from_source`]).
    pub account: Account,
    /// The post as sanitized HTML; the empty string on a reblog row,
    /// whose text lives in the nested [`reblog`](Self::reblog).
    pub content: String,
    /// Who may see the post, stored verbatim: `public`, `unlisted`,
    /// `private`, `direct` — or Pleroma's `local`, which is emitted as
    /// itself, Pleroma's renderer doing the same.
    pub visibility: String,
    /// Whether the content warning hides [`content`](Self::content) until
    /// the reader opens it.
    pub sensitive: JsonBool,
    /// The content warning subject line, `""` when there is none.
    pub spoiler_text: String,
    /// The media attachments, in `position` order.
    pub media_attachments: Vec<MediaAttachment>,
    /// The client the post arrived from (Pleroma's `generator`), or
    /// `null`.
    pub application: Option<StatusApplication>,
    /// The @-mentioned accounts, in mention order.
    pub mentions: Vec<Mention>,
    /// The hashtags, in name order.
    pub tags: Vec<Tag>,
    /// Custom emojis used in the post; `[]` until the emoji-in-statuses
    /// ticket fills it from the rows the service already batch-loads.
    pub emojis: Vec<Value>,
    /// How many times this post has been reblogged (stored counter).
    pub reblogs_count: i32,
    /// How many favourites this post has (stored counter).
    pub favourites_count: i32,
    /// How many answers this post has (stored counter).
    pub replies_count: i32,
    /// The reblogged original as a full entity of its own, or `null`.
    /// One level deep: the nested entity's own `reblog` is `null`.
    pub reblog: Option<Box<Self>>,
    /// The replied-to status's id, or `null` for a root post.
    pub in_reply_to_id: Option<String>,
    /// The replied-to status's author's id, or `null`.
    pub in_reply_to_account_id: Option<String>,
    /// The `BCP 47` language tag the poster stated, or `null`.
    pub language: Option<String>,
    /// The rich-media card for the post's first link: the JSON the
    /// [`link_card`] cache stored for this row's id, rendered verbatim;
    /// `null` for a status that links no card, or only a failed fetch.
    ///
    /// [`link_card`]: crate::models::link_card
    pub card: Option<Value>,
    /// The poll attached to the post, as the [`Poll`] entity; `null` for a
    /// row the batch found no poll on. The polls render ticket's batched
    /// reads of [`crate::models::poll`] feed the render — see
    /// [`StatusSource::poll`] and [`Poll::build`].
    pub poll: Option<Poll>,
    /// When the content last changed, in the Mastodon time format, or
    /// `null` for an unedited post.
    #[serde(with = "crate::util::time::serde_mastodon_time::option")]
    pub edited_at: Option<DateTime<Utc>>,
    /// Whether the viewer favourited this post; the favourites ticket's
    /// batched read answers it, `false` for an anonymous viewer and for a
    /// post they have not liked.
    pub favourited: JsonBool,
    /// Whether the viewer reblogged this post; the reblogs ticket's
    /// batched read answers it, `false` for an anonymous viewer and for a
    /// post they have not boosted. A reblog row carries the flag of its
    /// original — the status its own boost landed on — so the outer entity
    /// of the viewer's boost reads `reblogged: true`.
    pub reblogged: JsonBool,
    /// Whether the viewer muted this post's thread; the thread-mute
    /// ticket's batched read answers it, keyed to the row's
    /// `conversation_id`, `false` for an anonymous viewer and for a thread
    /// they have not muted. `pleroma.thread_muted` is the same value.
    pub muted: JsonBool,
    /// Whether the viewer bookmarked this post; the bookmarks ticket's
    /// batched read answers it, `false` for an anonymous viewer and for a
    /// post they have not bookmarked.
    pub bookmarked: JsonBool,
    /// Whether the viewer pinned this post to their own profile; the
    /// pinning ticket's batched read answers it, `false` for an anonymous
    /// viewer, for a post they have not pinned, and — since a pin is
    /// always the author's own act — for anybody else's post.
    pub pinned: JsonBool,
    /// The quoted status as a nested entity; `null` until the quoting
    /// ticket renders it — from the originals the service already
    /// batch-loads. (`quote_id` is not emitted at all.)
    pub quote: Option<Value>,
    /// The filters that warned over this status, as the v2 `FilterResult`
    /// entities: [`crate::services::filter::apply_filters`] fills it from
    /// the viewer's active filters as a page is post-processed, and an
    /// unfiltered (or exempt, or anonymous) render — and every endpoint
    /// the filter contexts do not reach — carries the empty array. A
    /// status a matching `irreversible` filter governs never renders at
    /// all: it leaves the response instead.
    pub filtered: Vec<FilterResult>,
    /// Pleroma's extension block (see [`PleromaMetadata`]).
    pub pleroma: PleromaMetadata,
}

impl Status {
    /// Renders `source`'s row as the entity, as `viewer` sees it.
    ///
    /// `relationships` is the optional preload of the viewer's
    /// relationships to the page's authors, keyed by account id — the
    /// same map [`Account::from_user_for`] embeds into
    /// `pleroma.relationship`; `None` renders Pleroma's anonymous `{}`.
    /// `config` supplies the instance origin that tag pages live under.
    ///
    /// Pure (decision 3): every value comes from `source`, `viewer`,
    /// `relationships` or `config`.
    #[must_use]
    pub fn from_source(
        source: StatusSource<'_>,
        viewer: AccountViewer<'_>,
        relationships: Option<&RelationshipMap>,
        config: &Config,
    ) -> Self {
        let StatusSource {
            row,
            author,
            reply_author,
            media,
            mentions,
            tags,
            reactions,
            card,
            favourited,
            reblogged,
            bookmarked,
            bookmark_folder,
            pinned_at,
            muted,
            direct_participation,
            reblog,
            poll,
        } = source;
        Self {
            id: row.id.to_string(),
            uri: row.uri.clone(),
            url: row.url.clone(),
            created_at: row.inserted_at,
            account: Account::from_user_for(viewer, author, config, relationships),
            content: rendered_content(row),
            visibility: row.visibility.as_str().to_owned(),
            sensitive: JsonBool(row.sensitive),
            spoiler_text: row.spoiler_text.clone(),
            media_attachments: media
                .iter()
                .map(|attachment| MediaAttachment::from_model(attachment, config))
                .collect(),
            application: application_block(row),
            mentions: mentions.iter().map(Mention::from_user).collect(),
            tags: tags
                .iter()
                .map(|tag| Tag::from_hashtag(&config.base_url, tag))
                .collect(),
            emojis: Vec::new(),
            reblogs_count: row.reblogs_count,
            favourites_count: row.favourites_count,
            replies_count: row.replies_count,
            reblog,
            in_reply_to_id: row.in_reply_to_id.map(|id| id.to_string()),
            in_reply_to_account_id: row.in_reply_to_account_id.map(|id| id.to_string()),
            language: row.language.clone(),
            card: card.and_then(stored_card_json),
            poll,
            edited_at: row.edited_at,
            favourited: JsonBool(favourited),
            reblogged,
            muted: muted.clone(),
            bookmarked: JsonBool(bookmarked),
            pinned: JsonBool(pinned_at.is_some()),
            quote: None,
            filtered: Vec::new(),
            pleroma: pleroma_block(
                row,
                reply_author,
                reactions,
                BlockViewer {
                    pinned_at,
                    muted,
                    bookmark_folder,
                    direct_participation,
                },
            ),
        }
    }
}

/// The `content` key: the row's sanitized HTML, or the empty string for
/// a reblog row — the ticket's rule, and Pleroma's `Announce` arm: a
/// reblog carries no text of its own, which lives in the nested `reblog`
/// entity instead.
fn rendered_content(row: &StatusRow) -> String {
    if row.reblog_of_id.is_some() {
        String::new()
    } else {
        row.content.clone()
    }
}

/// The `application` key: Pleroma's `build_application/1` answers a
/// `generator` that has a name, and `nil` for anything else; here the two
/// stored columns, with the name deciding.
fn application_block(row: &StatusRow) -> Option<StatusApplication> {
    row.application_name.as_ref().map(|name| StatusApplication {
        name: name.clone(),
        website: row.application_website.clone(),
    })
}

/// The `pleroma.content` value: the raw source text the row stores.
///
/// Pleroma renders it as the content HTML stripped back to text, so for
/// a local post the stored [`raw_text`](StatusRow::raw_text) is exactly
/// what its author wrote, and for a mirrored remote one the stripped HTML
/// is all the source federation ever had. A row whose `raw_text` came in
/// empty (a mirror that shipped only rich content) falls back to
/// [`strip_html`] of the content, so the key always carries the post as
/// plain text. Keeping it simple per the ticket, the value is emitted to
/// every viewer: this instance has no author-only source variant to gate
/// (Pleroma's `text` key is opt-in and separate).
fn plain_text(row: &StatusRow) -> PlainText {
    let text = if row.raw_text.is_empty() {
        strip_html(&row.content)
    } else {
        row.raw_text.clone()
    };
    PlainText { text_plain: text }
}

/// The four viewer facts the `pleroma` block renders, grouped because
/// there are more than [`pleroma_block`] could take beside the row, its
/// replied-to author and its reactions (decision 14's five-argument limit).
///
/// Each is the batch's one read per fact, keyed to the row: [`StatusSource`]'s
/// `pinned_at`, `muted`, `bookmark_folder` and `direct_participation`.
#[derive(Debug)]
struct BlockViewer {
    /// When the viewer pinned the row, `None` while they have pinned
    /// nothing.
    pinned_at: Option<DateTime<Utc>>,
    /// Whether the viewer muted the row's thread — the same [`JsonBool`] the
    /// entity's top-level `muted` carries, so the two halves never disagree.
    muted: JsonBool,
    /// The folder the viewer's bookmark files the row in, `None` when the
    /// bookmark is unfiled or there is none.
    bookmark_folder: Option<Uuid>,
    /// The viewer's `conversation_participations` row for the row's DM
    /// thread, `None` when they are no participant or there is no thread.
    direct_participation: Option<Uuid>,
}

/// Builds the `pleroma` block of `row`, whose replied-to author is
/// `reply_author` when the batch knew one, whose reactions are `reactions`
/// when the batch found any, and whose pin stamp, thread-mute flag and
/// bookmark folder are `viewer`'s three facts — see [`PleromaMetadata`] for
/// where each key comes from and which later tickets fill the placeholders.
fn pleroma_block(
    row: &StatusRow,
    reply_author: Option<&User>,
    reactions: &[SummaryRow],
    viewer: BlockViewer,
) -> PleromaMetadata {
    let BlockViewer {
        pinned_at,
        muted,
        bookmark_folder,
        direct_participation,
    } = viewer;
    PleromaMetadata {
        content: plain_text(row),
        spoiler_text: PlainText {
            text_plain: row.spoiler_text.clone(),
        },
        conversation_id: row.conversation_id.to_string(),
        direct_conversation_id: direct_participation.map(|id| id.to_string()),
        in_reply_to_account_acct: reply_author.map(|author| author.nickname.clone()),
        local: JsonBool(row.local),
        expires_at: row.expires_at,
        thread_muted: muted,
        emoji_reactions: reactions.iter().map(ReactionSummary::from_model).collect(),
        parent_visible: JsonBool(true),
        quotes_count: row.quotes_count,
        bookmark_folder: bookmark_folder.map(|id| id.to_string()),
        pinned_at,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{status::Visibility, user::Flag, user::mfa},
        util::time::parse_time,
    };

    /// The instance origin [`Config::for_tests`] serves.
    const BASE: &str = TEST_BASE_URL;
    /// The author's id, as stored.
    const AUTHOR_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// A second account id, for the reactor lists.
    const REACTOR_ID: &str = "01934567-89ab-7cde-f012-3456789abcee";
    /// A custom emoji's image URL, for the reaction entries.
    const CAT_IMAGE: &str = "https://test.example/emoji/cats/cat.png";
    /// The URL a link card is cached under, for the card entries.
    const CARD_URL: &str = "https://example.test/post";
    /// Every JSON key the entity must emit, sorted.
    const EXPECTED_KEYS: [&str; 32] = [
        "account",
        "application",
        "bookmarked",
        "card",
        "content",
        "created_at",
        "edited_at",
        "emojis",
        "favourited",
        "favourites_count",
        "filtered",
        "id",
        "in_reply_to_account_id",
        "in_reply_to_id",
        "language",
        "media_attachments",
        "mentions",
        "muted",
        "pinned",
        "pleroma",
        "poll",
        "quote",
        "reblog",
        "reblogged",
        "reblogs_count",
        "replies_count",
        "sensitive",
        "spoiler_text",
        "tags",
        "uri",
        "url",
        "visibility",
    ];
    /// Every `pleroma` block key, sorted — `quote_id`, `quote_url` and
    /// `quote_visible` are deliberately absent.
    const PLEROMA_KEYS: [&str; 13] = [
        "bookmark_folder",
        "content",
        "conversation_id",
        "direct_conversation_id",
        "emoji_reactions",
        "expires_at",
        "in_reply_to_account_acct",
        "local",
        "parent_visible",
        "pinned_at",
        "quotes_count",
        "spoiler_text",
        "thread_muted",
    ];

    /// The instant every fixture row is stamped with, in and out.
    fn stamped() -> DateTime<Utc> {
        parse_time("2024-01-31T12:34:56.789Z").unwrap()
    }

    /// A [`User`] row at the column defaults a fresh local account
    /// carries, named `nickname`.
    fn base_user(nickname: &str) -> User {
        User {
            id: Uuid::parse_str(AUTHOR_ID).unwrap(),
            nickname: nickname.to_owned(),
            ap_id: format!("{BASE}/users/{nickname}"),
            local: Flag::new(true),
            domain: None,
            actor_type: "Person".to_owned(),
            email: None,
            password_hash: None,
            name: String::new(),
            bio: String::new(),
            raw_bio: String::new(),
            avatar_url: None,
            banner_url: None,
            profile_url: None,
            locked: Flag::new(false),
            bot: Flag::new(false),
            discoverable: Flag::new(false),
            fields: json!([]),
            raw_fields: json!([]),
            inbox: None,
            shared_inbox: None,
            outbox: None,
            followers_url: None,
            following_url: None,
            featured_url: None,
            public_key: None,
            private_key: None,
            is_admin: Flag::new(false),
            is_moderator: Flag::new(false),
            is_active: Flag::new(true),
            hide_followers: Flag::new(false),
            hide_follows: Flag::new(false),
            hide_followers_count: Flag::new(false),
            hide_follows_count: Flag::new(false),
            hide_favorites: Flag::new(true),
            allow_following_move: Flag::new(true),
            skip_thread_containment: Flag::new(false),
            accepts_chat_messages: Flag::new(true),
            show_role: Flag::new(true),
            no_rich_text: Flag::new(false),
            default_scope: "public".to_owned(),
            sensitive_default: Flag::new(false),
            language: None,
            tags: Vec::new(),
            settings_store: json!({}),
            notification_settings: json!({}),
            mfa_settings: mfa::MfaSettings::default_value(),
            background_image_url: None,
            birthday: None,
            show_birthday: Flag::new(false),
            also_known_as: Vec::new(),
            moved_to_ap_id: None,
            last_move_at: None,
            email_confirmed: Flag::new(true),
            approval_pending: Flag::new(false),
            registration_reason: None,
            confirmation_token: None,
            confirmation_sent_at: None,
            last_active_at: None,
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            last_status_at: None,
            last_fetched_at: None,
            inserted_at: stamped(),
            updated_at: stamped(),
            preferred_frontend: None,
            is_suggested: Flag::new(false),
            password_reset_pending: Flag::new(false),
            email_notifications: Flag::new(true),
            deleted_at: None,
        }
    }

    /// A second user, distinct from [`base_user`]'s author row: `nickname`
    /// with id `id`.
    fn other_user(nickname: &str, id: &str) -> User {
        let mut user = base_user(nickname);
        user.id = Uuid::parse_str(id).unwrap();
        user
    }

    /// A live local public status by [`base_user`]'s author: every link
    /// empty, every counter zero, the timestamps fixed.
    fn base_row() -> StatusRow {
        let id = Uuid::parse_str("01944444-4444-7444-8444-444444444444").unwrap();
        StatusRow {
            id,
            uri: format!("{BASE}/objects/{id}"),
            url: Some(format!("{BASE}/notice/{id}")),
            user_id: Uuid::parse_str(AUTHOR_ID).unwrap(),
            content: "<p>hello world</p>".to_owned(),
            raw_text: "hello world".to_owned(),
            content_type: "text/plain".to_owned(),
            spoiler_text: String::new(),
            sensitive: false,
            visibility: Visibility::Public,
            language: None,
            in_reply_to_id: None,
            in_reply_to_account_id: None,
            in_reply_to_ap_id: None,
            conversation_id: id,
            direct_conversation_id: None,
            reblog_of_id: None,
            quote_of_id: None,
            local: true,
            application_name: None,
            application_website: None,
            replies_count: 0,
            reblogs_count: 0,
            favourites_count: 0,
            quotes_count: 0,
            edited_at: None,
            deleted_at: None,
            expires_at: None,
            inserted_at: stamped(),
            updated_at: stamped(),
        }
    }

    /// The batch-loaded parts a test render supplies: whose row's author,
    /// whose replied-to author, and the per-status lists.
    #[derive(Debug)]
    struct Parts<'a> {
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
    }

    impl<'a> Parts<'a> {
        /// Parts with nothing loaded: `author` alone.
        fn of(author: &'a User) -> Self {
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
            }
        }

        /// Sets [`reply_author`](Self::reply_author).
        fn by_reply_to(self, author: &'a User) -> Self {
            Self {
                reply_author: Some(author),
                ..self
            }
        }

        /// Sets [`mentions`](Self::mentions).
        fn mentioning(self, users: Vec<User>) -> Self {
            Self {
                mentions: users,
                ..self
            }
        }

        /// Sets [`tags`](Self::tags).
        fn tagged(self, tags: Vec<Hashtag>) -> Self {
            Self { tags, ..self }
        }

        /// Sets [`media`](Self::media).
        fn attached(self, media: Vec<AttachmentRow>) -> Self {
            Self { media, ..self }
        }

        /// Sets [`reactions`](Self::reactions).
        fn reacted(self, reactions: Vec<SummaryRow>) -> Self {
            Self { reactions, ..self }
        }

        /// Sets [`card`](Self::card) to a cached `ok` row holding `json`:
        /// the card the rich-media batch's [`for_statuses`] read would
        /// hand the renderer for a linked status.
        ///
        /// [`for_statuses`]: crate::models::link_card::for_statuses
        fn with_card(self, json: Value) -> Self {
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
        fn with_failed_card(self) -> Self {
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
        fn liked(self) -> Self {
            Self {
                favourited: true,
                ..self
            }
        }

        /// Sets [`reblogged`](Self::reblogged): the viewer's flag, which
        /// the reblogs ticket's batched read would hand the renderer.
        fn boosted(self) -> Self {
            Self {
                reblogged: JsonBool(true),
                ..self
            }
        }

        /// Sets [`bookmarked`](Self::bookmarked): the viewer's flag, which
        /// the bookmarks ticket's batched read would hand the renderer.
        fn bookmarked(self) -> Self {
            Self {
                bookmarked: true,
                ..self
            }
        }

        /// Sets [`bookmark_folder`](Self::bookmark_folder) (and the
        /// bookmark it rides): the folder the folders ticket's batched read
        /// would hand the renderer for a filed bookmark.
        fn filed_in(self, folder: Uuid) -> Self {
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
        fn pinned(self) -> Self {
            Self {
                pinned_at: Some(stamped()),
                ..self
            }
        }

        /// Sets [`muted`](Self::muted): the viewer's thread mute, which the
        /// thread-mute ticket's batched read would hand the renderer. One
        /// membership answers both the top-level flag and
        /// `pleroma.thread_muted`.
        fn muted_thread(self) -> Self {
            Self {
                muted: JsonBool(true),
                ..self
            }
        }

        /// Sets [`poll`](Self::poll): the built poll entity the polls
        /// ticket's batched reads would hand the renderer for a row that
        /// carries one.
        fn with_poll(self, poll: Poll) -> Self {
            Self {
                poll: Some(poll),
                ..self
            }
        }
    }

    /// `row` rendered as an anonymous viewer sees it, with `reblog`
    /// attached as the nested entity when given.
    fn render(row: &StatusRow, parts: &Parts<'_>, reblog: Option<Status>) -> Value {
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
            reblog: reblog.map(Box::new),
            poll: parts.poll.clone(),
        };
        serde_json::to_value(Status::from_source(source, None, None, &config)).unwrap()
    }

    /// The nested render of `row` by `author` — the reblog test builds its
    /// original entity through here.
    fn render_nested(row: &StatusRow, author: &User) -> Status {
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
                reblog: None,
                poll: None,
            },
            None,
            None,
            &config,
        )
    }

    /// A stored attachment row with `url` and `content_type` set.
    fn attachment(url: &str, content_type: &str) -> AttachmentRow {
        AttachmentRow {
            id: Uuid::now_v7(),
            user_id: Some(Uuid::parse_str(AUTHOR_ID).unwrap()),
            status_id: Some(Uuid::nil()),
            position: 0,
            storage_key: Some("media/x.png".to_owned()),
            remote_url: None,
            url: url.to_owned(),
            preview_url: None,
            content_type: content_type.to_owned(),
            description: None,
            blurhash: None,
            width: None,
            height: None,
            preview_width: None,
            preview_height: None,
            duration: None,
            focus_x: None,
            focus_y: None,
            file_size: None,
            sensitive_hint: false,
            inserted_at: stamped(),
            updated_at: stamped(),
        }
    }

    /// A stored tag row named `name`.
    fn hashtag(name: &str) -> Hashtag {
        Hashtag {
            id: Uuid::now_v7(),
            name: name.to_owned(),
            inserted_at: stamped(),
        }
    }

    /// The rendered object's keys, sorted, for exact key-set assertions.
    fn json_keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn a_plain_status_emits_every_key_and_its_defaults() {
        let alice = base_user("alice");
        let row = base_row();
        let value = render(&row, &Parts::of(&alice), None);
        assert_eq!(json_keys(&value), EXPECTED_KEYS, "exact key set");
        assert_eq!(json_keys(&value["pleroma"]), PLEROMA_KEYS);

        assert_eq!(value["id"], row.id.to_string());
        assert_eq!(value["uri"], row.uri);
        assert_eq!(value["url"], json!(row.url));
        assert_eq!(value["created_at"], "2024-01-31T12:34:56.789Z");
        assert_eq!(value["content"], "<p>hello world</p>");
        assert_eq!(value["visibility"], "public");
        assert_eq!(value["sensitive"], false);
        assert_eq!(value["spoiler_text"], "");
        assert_eq!(value["media_attachments"], json!([]));
        assert_eq!(value["application"], Value::Null, "no generator, null");
        assert_eq!(value["mentions"], json!([]));
        assert_eq!(value["tags"], json!([]));
        assert_eq!(value["emojis"], json!([]), "emojis are a later ticket");
        assert_eq!(value["reblogs_count"], 0);
        assert_eq!(value["favourites_count"], 0);
        assert_eq!(value["replies_count"], 0);
        assert_eq!(value["reblog"], Value::Null, "not a reblog");
        assert_eq!(value["in_reply_to_id"], Value::Null);
        assert_eq!(value["in_reply_to_account_id"], Value::Null);
        assert_eq!(value["language"], Value::Null);
        assert_eq!(
            value["card"],
            Value::Null,
            "a page whose cards read found nothing renders null"
        );
        assert_eq!(
            value["poll"],
            Value::Null,
            "a row the batch found no poll on renders null"
        );
        assert_eq!(value["edited_at"], Value::Null);
        assert_eq!(
            value["reblogged"], false,
            "a page whose reblogs read found nothing renders the flag false"
        );
        assert_eq!(
            value["muted"], false,
            "a page whose thread-mutes read found nothing renders the flag \
             false, and the extension block agrees — one membership feeds both"
        );
        assert_eq!(value["pleroma"]["thread_muted"], false);
        assert_eq!(
            value["pinned"], false,
            "a page whose pins read found nothing renders the flag false"
        );
        assert_eq!(
            value["favourited"], false,
            "a page whose favourites read found nothing renders the flag false"
        );
        assert_eq!(
            value["bookmarked"], false,
            "a page whose bookmarks read found nothing renders the flag false"
        );
        assert_eq!(value["quote"], Value::Null, "quotes are a later ticket");
        assert_eq!(
            value["filtered"],
            json!([]),
            "the render itself never filters: the entity comes out with the \
             empty array and `services::filter` fills it as a page is post-processed"
        );
    }

    #[test]
    fn a_linked_ok_card_renders_the_stored_json_verbatim() {
        let alice = base_user("alice");
        let row = base_row();
        let stored = json!({
            "url": CARD_URL,
            "title": "Example Domain",
            "type": "link",
            "image": null,
            "pleroma": { "opengraph": { "og:title": "Example Domain" } },
        });
        let value = render(&row, &Parts::of(&alice).with_card(stored.clone()), None);
        assert_eq!(
            value["card"], stored,
            "the card the fetch job stored renders exactly as cached"
        );
    }

    #[test]
    fn a_failed_card_row_renders_null() {
        let alice = base_user("alice");
        let row = base_row();
        let value = render(&row, &Parts::of(&alice).with_failed_card(), None);
        assert_eq!(
            value["card"],
            Value::Null,
            "a linked but failed fetch renders no card, like no link at all"
        );
    }

    #[test]
    fn the_viewers_favourite_lands_in_the_flag() {
        let alice = base_user("alice");
        let row = base_row();
        let value = render(&row, &Parts::of(&alice).liked(), None);
        assert_eq!(
            value["favourited"], true,
            "the favourites batch's membership reaches the entity verbatim"
        );
    }

    #[test]
    fn the_viewers_reblog_lands_in_the_flag() {
        let alice = base_user("alice");
        let row = base_row();
        let value = render(&row, &Parts::of(&alice).boosted(), None);
        assert_eq!(
            value["reblogged"], true,
            "the reblogs batch's membership reaches the entity verbatim"
        );
    }

    #[test]
    fn the_viewers_bookmark_lands_in_the_flag() {
        let alice = base_user("alice");
        let row = base_row();
        let value = render(&row, &Parts::of(&alice).bookmarked(), None);
        assert_eq!(
            value["bookmarked"], true,
            "the bookmarks batch's membership reaches the entity verbatim"
        );
        assert_eq!(
            value["pleroma"]["bookmark_folder"],
            Value::Null,
            "an unfiled bookmark carries no folder"
        );
    }

    #[test]
    fn a_filed_bookmark_lands_in_the_folder_key_too() {
        let alice = base_user("alice");
        let row = base_row();
        let folder = Uuid::parse_str("01955555-5555-7555-8555-555555555555").unwrap();
        let value = render(&row, &Parts::of(&alice).filed_in(folder), None);
        assert_eq!(value["bookmarked"], true, "the file is a bookmark");
        assert_eq!(
            value["pleroma"]["bookmark_folder"],
            folder.to_string(),
            "and the folder the same read names reaches the pleroma block \
             as the hyphenated id (decision 4)"
        );
    }

    #[test]
    fn the_viewers_pin_lands_in_the_flag_and_the_stamp() {
        let alice = base_user("alice");
        let row = base_row();
        let value = render(&row, &Parts::of(&alice).pinned(), None);
        assert_eq!(
            value["pinned"], true,
            "the pins batch's membership reaches the entity verbatim"
        );
        assert_eq!(
            value["pleroma"]["pinned_at"], "2024-01-31T12:34:56.789Z",
            "and the same read's timestamp reaches the pleroma block"
        );
    }

    #[test]
    fn the_viewers_thread_mute_lands_in_both_flags() {
        let alice = base_user("alice");
        let row = base_row();
        let value = render(&row, &Parts::of(&alice).muted_thread(), None);
        assert_eq!(
            value["muted"], true,
            "the thread-mutes batch's membership reaches the entity verbatim"
        );
        assert_eq!(
            value["pleroma"]["thread_muted"], true,
            "and the same membership reaches the pleroma block — the two \
             halves are one lookup"
        );
    }

    /// The [`Poll`] entity the batch would hand a polled render: the
    /// fixture row's stored poll, two choices with one vote on the second,
    /// and the viewer's mark at position 1. `expires_at` is the render
    /// instant itself, and [`Poll::build`] expires a poll only strictly
    /// past its deadline — so this one is open, and `hide_totals` (false
    /// here, as stored) has nothing to withhold.
    fn built_poll() -> Poll {
        use crate::models::poll::{Poll as PollRow, PollOption as PollOptionRow};

        let poll_id = Uuid::parse_str("01966666-6666-7666-8666-666666666666").unwrap();
        let option = |position: i32, title: &str, votes_count: i32| PollOptionRow {
            id: Uuid::now_v7(),
            poll_id,
            position,
            title: title.to_owned(),
            votes_count,
        };
        Poll::build(
            &PollRow {
                id: poll_id,
                status_id: Uuid::parse_str("01944444-4444-7444-8444-444444444444").unwrap(),
                expires_at: Some(stamped()),
                multiple: true,
                hide_totals: false,
                voters_count: 1,
                votes_count: 1,
            },
            &[option(0, "tea", 0), option(1, "coffee", 1)],
            &[1],
            stamped(),
        )
    }

    #[test]
    fn a_polled_row_renders_the_full_poll_entity() {
        let alice = base_user("alice");
        let row = base_row();
        let value = render(&row, &Parts::of(&alice).with_poll(built_poll()), None);
        assert_eq!(
            value["poll"],
            json!({
                "id": "01966666-6666-7666-8666-666666666666",
                "expires_at": "2024-01-31T12:34:56.789Z",
                "expired": false,
                "multiple": true,
                "votes_count": 1,
                "voters_count": 1,
                "voted": true,
                "own_votes": [1],
                "options": [
                    {"title": "tea", "votes_count": 0},
                    {"title": "coffee", "votes_count": 1},
                ],
                "emojis": [],
            }),
            "the batch's built poll reaches the wire whole, key for key"
        );
        assert_eq!(
            json_keys(&value),
            EXPECTED_KEYS,
            "and the entity's key set is unchanged by filling the slot"
        );
    }

    #[test]
    fn an_unpinned_row_carries_no_stamp() {
        let alice = base_user("alice");
        let row = base_row();
        let value = render(&row, &Parts::of(&alice), None);
        assert_eq!(value["pinned"], false, "no pin in the batch");
        assert_eq!(
            value["pleroma"]["pinned_at"],
            Value::Null,
            "and so no timestamp either — the two always agree"
        );
    }

    #[test]
    fn the_nested_render_keeps_the_viewers_bookmark_out_of_the_original() {
        // The bookmarks read keys every row's flag to its own id, so a
        // reblog the viewer bookmarked renders flagged while the nested
        // original — which the client never named — does not.
        let alice = base_user("alice");
        let row = base_row();
        let nested = render_nested(&row, &alice);
        let value = render(&row, &Parts::of(&alice).bookmarked(), Some(nested));
        assert_eq!(value["bookmarked"], true, "the named row is flagged");
        assert_eq!(
            value["reblog"]["bookmarked"], false,
            "and the nested original is nobody's bookmark by itself"
        );
    }

    #[test]
    fn the_pleroma_block_carries_the_stored_row() {
        let alice = base_user("alice");
        let row = base_row();
        let pleroma = render(&row, &Parts::of(&alice), None)["pleroma"].clone();
        assert_eq!(pleroma["content"], json!({"text/plain": "hello world"}));
        assert_eq!(pleroma["spoiler_text"], json!({"text/plain": ""}));
        assert_eq!(pleroma["conversation_id"], row.conversation_id.to_string());
        assert_eq!(pleroma["direct_conversation_id"], Value::Null);
        assert_eq!(pleroma["in_reply_to_account_acct"], Value::Null);
        assert_eq!(pleroma["local"], true);
        assert_eq!(pleroma["expires_at"], Value::Null);
        assert_eq!(pleroma["thread_muted"], false);
        assert_eq!(
            pleroma["emoji_reactions"],
            json!([]),
            "no reactions in the batch is the empty array"
        );
        assert_eq!(pleroma["parent_visible"], true);
        assert_eq!(pleroma["quotes_count"], 0);
        assert_eq!(pleroma["bookmark_folder"], Value::Null);
        assert_eq!(pleroma["pinned_at"], Value::Null);
    }

    #[test]
    fn the_pleroma_block_lists_the_reactions_the_batch_found() {
        let alice = base_user("alice");
        let bob = other_user("bob", REACTOR_ID);
        let row = base_row();
        let summaries = vec![
            rollup("👍", 2, true, None, &[alice.id, bob.id]),
            rollup(":cat:", 1, false, Some(CAT_IMAGE), &[bob.id]),
        ];
        let pleroma = render(&row, &Parts::of(&alice).reacted(summaries), None)["pleroma"].clone();
        assert_eq!(
            pleroma["emoji_reactions"],
            json!([
                {
                    "name": "👍",
                    "count": 2,
                    "me": true,
                    "url": null,
                    "account_ids": [AUTHOR_ID, REACTOR_ID],
                },
                {
                    "name": ":cat:",
                    "count": 1,
                    "me": false,
                    "url": CAT_IMAGE,
                    "account_ids": [REACTOR_ID],
                },
            ]),
            "the roll-up reaches the wire in the batch's order, with the \
             reactor ids as the hyphenated text clients know"
        );
    }

    /// A model [`SummaryRow`] to render, in one line.
    fn rollup(
        name: &str,
        count: i64,
        me: bool,
        url: Option<&str>,
        account_ids: &[Uuid],
    ) -> SummaryRow {
        SummaryRow {
            name: name.to_owned(),
            count,
            me,
            url: url.map(str::to_owned),
            account_ids: account_ids.to_vec(),
        }
    }

    #[test]
    fn local_visibility_emits_its_literal_value() {
        let alice = base_user("alice");
        let mut row = base_row();
        row.visibility = Visibility::Local;
        assert_eq!(
            render(&row, &Parts::of(&alice), None)["visibility"],
            "local",
            "Pleroma passes the visibility through: a local-only status is \
             `local` on the wire, never `public`"
        );
    }

    #[test]
    fn a_row_with_no_raw_text_strips_the_html_for_the_plain_variant() {
        let alice = base_user("alice");
        let mut row = base_row();
        row.raw_text = String::new();
        row.content = "<p>one</p><p>two</p>".to_owned();
        let pleroma = render(&row, &Parts::of(&alice), None)["pleroma"].clone();
        assert_eq!(
            pleroma["content"],
            json!({"text/plain": "one\ntwo\n"}),
            "raw_text empty: text/plain is strip_html of the content, the \
             same reduction Pleroma's remote posts can only ever have"
        );
    }

    #[test]
    fn a_reply_links_parent_parent_author_and_their_acct() {
        let alice = base_user("alice");
        let bob = other_user("bob", "01934567-89ab-7cde-f012-3456789abcdf");
        let mut row = base_row();
        row.in_reply_to_id = Some(bob.id);
        row.in_reply_to_account_id = Some(bob.id);
        let value = render(&row, &Parts::of(&alice).by_reply_to(&bob), None);
        assert_eq!(value["in_reply_to_id"], bob.id.to_string());
        assert_eq!(value["in_reply_to_account_id"], bob.id.to_string());
        assert_eq!(
            value["pleroma"]["in_reply_to_account_acct"], "bob",
            "the replied-to author's stored handle (decision 7)"
        );
    }

    #[test]
    fn a_reblog_nests_the_original_and_blanks_its_own_content() {
        let alice = base_user("alice");
        let bob = other_user("bob", "01934567-89ab-7cde-f012-3456789abcdf");

        let mut original = base_row();
        original.user_id = bob.id;
        let original_id = original.id;
        let original_uri = original.uri.clone();
        let nested = render_nested(&original, &bob);

        let mut reblog = base_row();
        reblog.reblog_of_id = Some(original_id);
        reblog.content = String::new();
        reblog.raw_text = String::new();
        let row_id = reblog.id;
        let row_uri = reblog.uri.clone();
        let value = render(&reblog, &Parts::of(&alice), Some(nested));

        assert_eq!(value["id"], row_id.to_string(), "the reblog's own id");
        assert_eq!(
            value["uri"], row_uri,
            "and uri: the reblog's own, not the original's"
        );
        assert_eq!(value["content"], "", "the outer content is empty");
        assert_eq!(
            value["pleroma"]["content"],
            json!({"text/plain": ""}),
            "and so is the plain-text variant"
        );
        assert_eq!(value["account"]["acct"], "alice", "the reblogger speaks");
        assert_eq!(value["reblog"]["id"], original_id.to_string());
        assert_eq!(value["reblog"]["uri"], original_uri);
        assert_eq!(value["reblog"]["content"], "<p>hello world</p>");
        assert_eq!(value["reblog"]["account"]["acct"], "bob");
        assert_eq!(
            value["reblog"]["reblog"],
            Value::Null,
            "one level deep: the nested entity's own reblog is null"
        );
    }

    #[test]
    fn media_mentions_and_tags_render_from_the_loaded_rows() {
        let alice = base_user("alice");
        let mut carol = other_user(
            "carol@remote.example",
            "01934567-89ab-7cde-f012-3456789abcee",
        );
        carol.local = Flag::new(false);
        carol.ap_id = "https://remote.example/users/carol".to_owned();
        let carol_id = carol.id;
        let media = vec![
            attachment("https://media.example/1.png", "image/png"),
            attachment("https://media.example/2.mp4", "video/mp4"),
        ];
        let parts = Parts::of(&alice)
            .attached(media)
            .mentioning(vec![carol])
            .tagged(vec![hashtag("rust"), hashtag("news")]);
        let value = render(&base_row(), &parts, None);

        let rendered_media: Vec<&str> = value["media_attachments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["url"].as_str().unwrap())
            .collect();
        assert_eq!(
            rendered_media,
            ["https://media.example/1.png", "https://media.example/2.mp4"],
            "attachments keep their position order"
        );
        assert_eq!(value["media_attachments"][0]["type"], "image");
        assert_eq!(value["media_attachments"][1]["type"], "video");
        assert_eq!(
            value["mentions"],
            json!([{
                "id": carol_id.to_string(),
                "username": "carol",
                "url": "https://remote.example/users/carol",
                "acct": "carol@remote.example",
            }])
        );
        assert_eq!(
            value["tags"],
            json!([
                {"name": "rust", "url": format!("{BASE}/tags/rust")},
                {"name": "news", "url": format!("{BASE}/tags/news")},
            ])
        );
    }

    #[test]
    fn application_renders_name_and_website_or_null() {
        let alice = base_user("alice");
        let mut row = base_row();
        row.application_name = Some("WiredClient".to_owned());
        let named_only = render(&row, &Parts::of(&alice), None);
        assert_eq!(
            named_only["application"],
            json!({"name": "WiredClient", "website": null}),
            "a name alone renders, the missing website as null"
        );

        row.application_website = Some("https://client.example".to_owned());
        let with_website = render(&row, &Parts::of(&alice), None);
        assert_eq!(
            with_website["application"],
            json!({"name": "WiredClient", "website": "https://client.example"}),
            "both halves of the generator"
        );

        row.application_name = None;
        let nameless = render(&row, &Parts::of(&alice), None);
        assert_eq!(
            nameless["application"],
            Value::Null,
            "no name means no application block, Pleroma's build_application"
        );
    }

    #[test]
    fn stored_details_reach_their_keys() {
        let alice = base_user("alice");
        let mut row = base_row();
        row.sensitive = true;
        row.spoiler_text = "cats".to_owned();
        row.language = Some("de".to_owned());
        row.edited_at = Some(stamped());
        row.expires_at = Some(stamped());
        row.quotes_count = 3;
        row.reblogs_count = 4;
        row.favourites_count = 5;
        row.replies_count = 6;
        let value = render(&row, &Parts::of(&alice), None);
        assert_eq!(value["sensitive"], true);
        assert_eq!(value["spoiler_text"], "cats");
        assert_eq!(value["language"], "de");
        assert_eq!(value["edited_at"], "2024-01-31T12:34:56.789Z");
        assert_eq!(value["reblogs_count"], 4);
        assert_eq!(value["favourites_count"], 5);
        assert_eq!(value["replies_count"], 6);
        assert_eq!(
            value["pleroma"]["spoiler_text"],
            json!({"text/plain": "cats"})
        );
        assert_eq!(value["pleroma"]["expires_at"], "2024-01-31T12:34:56.789Z");
        assert_eq!(value["pleroma"]["quotes_count"], 3);
    }
}
