//! The `Status` entity itself: the struct every render of this tree answers
//! with, the pure [`Status::from_source`] that fills its fields, and the
//! three helpers the fields take their values from.
//!
//! Declaration order is emission order, so this file is the entity's shape
//! as clients see it: the core Mastodon keys first and the
//! [`PleromaMetadata`] extension block last, whose own builder lives in
//! `pleroma`. Nothing here reads a database (decision 3): every value comes
//! from the [`StatusSource`] batch, the viewer, the relationship preload or
//! the instance [`Config`].
//!
//! [`Config`]: crate::config::Config

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use super::{
    PlainText, PleromaMetadata, StatusApplication, StatusSource,
    pleroma::{BlockViewer, pleroma_block},
    source::QuoteSlot,
};
use crate::{
    config::Config,
    entities::{
        account::{Account, AccountViewer, JsonBool},
        custom_emoji::CustomEmoji,
        filter::FilterResult,
        media_attachment::MediaAttachment,
        mention::Mention,
        poll::Poll,
        relationship::RelationshipMap,
        tag::Tag,
    },
    models::{
        custom_emoji::CustomEmoji as EmojiRow, hashtag::Hashtag,
        media_attachment::MediaAttachment as AttachmentRow, status::Status as StatusRow,
    },
    services::rich_media::stored_card_json,
    util::sanitize::strip_html,
};

/// The Mastodon `Status` entity (core fields plus the `pleroma` block).
///
/// Build one with [`Status::from_source`]; its [`Serialize`] impl emits
/// exactly the keys listed in the [module docs](crate::entities::status).
/// Presence: every key is always emitted — the nullable ones (`url`,
/// `application`, `reblog`, `in_reply_to_id`, `in_reply_to_account_id`,
/// `language`, `card`, `poll`, `edited_at`, `quote`, `quote_id` and the
/// `pleroma` nulls) render JSON `null` rather than disappearing, as
/// Pleroma's renderer sends them.
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
    /// The custom emojis the post's content, warning or poll options use,
    /// as [`CustomEmoji`] entities in the order they first appear; `[]` for
    /// a post using none, and for a reblog row (its text is the nested
    /// original's, which carries its own).
    pub emojis: Vec<CustomEmoji>,
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
    /// The quoted original's id as the hyphenated lowercase `UUIDv7`
    /// string (decision 4), or `null` when this status quotes nothing —
    /// emitted whatever the viewer may see of the quoted row, so an
    /// invisible quote still names its target.
    pub quote_id: Option<String>,
    /// Mastodon 4.4's Quote entity: `{state, quoted_status}` where the
    /// quoted status is the nested entity the quoting ticket's batch
    /// rendered for this viewer (`accepted`), or `null` with state
    /// `unauthorized` (hidden from the viewer) or `deleted` (gone). The
    /// bare quoted status is mirrored in
    /// [`pleroma.quote`](PleromaMetadata::quote). One level deep: a
    /// quoted status's own `quote` is `{state, quoted_status_id}`. `null`
    /// when this status quotes nothing.
    pub quote: Option<StatusQuote>,
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
            emojis,
            reactions,
            card,
            favourited,
            reblogged,
            bookmarked,
            bookmark_folder,
            pinned_at,
            muted,
            direct_participation,
            quote,
            reblog,
            poll,
            parent_visible,
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
            media_attachments: media_entities(media, config),
            application: application_block(row),
            mentions: mentions.iter().map(Mention::from_user).collect(),
            tags: tag_entities(tags, config),
            emojis: emoji_entities(emojis, config),
            reblogs_count: row.reblogs_count,
            favourites_count: row.favourites_count,
            replies_count: row.replies_count,
            reblog,
            in_reply_to_id: row.in_reply_to_id.map(|id| id.to_string()),
            in_reply_to_account_id: row.in_reply_to_account_id.map(|id| id.to_string()),
            language: row.language.clone(),
            card: card.and_then(|card| stored_card_json(card, config)),
            poll,
            edited_at: row.edited_at,
            favourited: JsonBool(favourited),
            reblogged,
            muted: muted.clone(),
            bookmarked: JsonBool(bookmarked),
            pinned: JsonBool(pinned_at.is_some()),
            quote_id: row.quote_of_id.map(|id| id.to_string()),
            quote: row
                .quote_of_id
                .map(|id| StatusQuote::new(quote.as_ref(), id)),
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
                    quote: quote.and_then(QuoteSlot::into_rendered),
                    parent_visible,
                },
            ),
        }
    }
}

/// The `media_attachments` key: one entity per stored attachment, in the
/// order the batch loaded them (`position` order).
fn media_entities(media: &[AttachmentRow], config: &Config) -> Vec<MediaAttachment> {
    media
        .iter()
        .map(|attachment| MediaAttachment::from_model(attachment, config))
        .collect()
}

/// The `emojis` key: one entity per custom emoji the batch found the post
/// using, in the order it first uses them.
fn emoji_entities(emojis: &[EmojiRow], config: &Config) -> Vec<CustomEmoji> {
    emojis
        .iter()
        .map(|emoji| CustomEmoji::from_model(emoji, config))
        .collect()
}

/// The `tags` key: one entity per hashtag, in the batch's name order.
fn tag_entities(tags: &[Hashtag], config: &Config) -> Vec<Tag> {
    tags.iter()
        .map(|tag| Tag::from_hashtag(&config.base_url, tag))
        .collect()
}

/// Mastodon's Quote entity (4.4+, Pleroma 2.10): the state of the quote
/// and, when the viewer may see it, the quoted status.
#[derive(Debug, Serialize)]
pub struct QuoteEntity {
    /// `accepted`, `unauthorized`, `deleted`, `rejected` or `revoked`.
    pub state: &'static str,
    /// The bare quoted status for `accepted`, `null` otherwise.
    pub quoted_status: Option<Value>,
}

/// A quote reference without the status itself, as a quoted status's
/// own quote renders (one level deep, as Pleroma does).
#[derive(Debug, Serialize)]
pub struct QuoteRef {
    /// `accepted`, `unauthorized` or `deleted`.
    pub state: &'static str,
    /// The id of the quoted status.
    pub quoted_status_id: String,
}

/// The `quote` key: the full [`QuoteEntity`] of a top-level status or the
/// [`QuoteRef`] of a status rendered inside another's quote.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum StatusQuote {
    /// The Quote entity with the status embedded when visible.
    Entity(QuoteEntity),
    /// Just the state and the quoted id.
    Ref(QuoteRef),
}

impl StatusQuote {
    /// The quote of a row quoting `quoted_id`, from what the batch found
    /// behind it; a missing slot reads as a deleted quote.
    fn new(slot: Option<&QuoteSlot>, quoted_id: Uuid) -> Self {
        match slot {
            Some(QuoteSlot::Visible(rendered)) => Self::Entity(QuoteEntity {
                state: "accepted",
                quoted_status: Some(rendered.clone()),
            }),
            Some(QuoteSlot::Hidden) => Self::Entity(QuoteEntity {
                state: "unauthorized",
                quoted_status: None,
            }),
            Some(QuoteSlot::Rejected) => Self::Entity(QuoteEntity {
                state: "rejected",
                quoted_status: None,
            }),
            Some(QuoteSlot::Revoked) => Self::Entity(QuoteEntity {
                state: "revoked",
                quoted_status: None,
            }),
            Some(QuoteSlot::Linked) => Self::reference("accepted", quoted_id),
            Some(QuoteSlot::LinkedHidden) => Self::reference("unauthorized", quoted_id),
            Some(QuoteSlot::Deleted) | None => Self::Entity(QuoteEntity {
                state: "deleted",
                quoted_status: None,
            }),
        }
    }

    /// The nested form: `state` and the quoted id, no status.
    fn reference(state: &'static str, quoted_id: Uuid) -> Self {
        Self::Ref(QuoteRef {
            state,
            quoted_status_id: quoted_id.to_string(),
        })
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
pub(super) fn plain_text(row: &StatusRow) -> PlainText {
    let text = if row.raw_text.is_empty() {
        strip_html(&row.content)
    } else {
        row.raw_text.clone()
    };
    PlainText { text_plain: text }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{entities::status::test_support::*, models::status::Visibility};

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
