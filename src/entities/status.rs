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
//! `favourited`, `reblogged`, `muted`, `bookmarked`, `pinned` and
//! `pleroma.pinned_at` (the favourite/reblog/mute/bookmark/pin tables);
//! `card` (rich-media cards); `poll` (polls); `emojis` and
//! `pleroma.emoji_reactions` (custom emojis and reactions); `quote` (the
//! quoting ticket, whose original rows the service already batch-loads);
//! and `pleroma.direct_conversation_id` / `pleroma.bookmark_folder`
//! (chats, bookmark folders).

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::{
    config::Config,
    entities::{
        account::{Account, AccountViewer, JsonBool},
        media_attachment::MediaAttachment,
        mention::Mention,
        relationship::RelationshipMap,
        tag::Tag,
    },
    models::{
        hashtag::Hashtag, media_attachment::MediaAttachment as AttachmentRow,
        status::Status as StatusRow, user::User,
    },
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
    /// `direct_conversation_id`: the viewer's chat participation id for a
    /// direct message; the chat-conversations ticket sets this, so it
    /// renders JSON `null` for now.
    pub direct_conversation_id: Option<Value>,
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
    /// the thread-mute ticket sets it, so it renders `false` for now.
    pub thread_muted: JsonBool,
    /// `emoji_reactions`: the post's reactions; the reactions ticket
    /// fills this, so it renders `[]` for now.
    pub emoji_reactions: Vec<Value>,
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
    /// `bookmark_folder`: the viewer's folder for a bookmarked post; the
    /// bookmarks ticket sets it, so it renders JSON `null` for now.
    pub bookmark_folder: Option<Value>,
    /// `pinned_at`: when the account pinned this post to their profile;
    /// the pinning ticket fills it, so it renders `null` for now.
    #[serde(with = "crate::util::time::serde_mastodon_time::option")]
    pub pinned_at: Option<DateTime<Utc>>,
}

/// One row plus every batch-loaded part its render needs, gathered by
/// [`crate::services::render_status`] with one query per part for the
/// whole page (no N+1).
///
/// [`Status::from_source`] consumes it. Later tickets add one field (and
/// the service's one loader feeding it) per rendered extension: the
/// viewer's flags for this row, its card, its poll, its emojis, its
/// rendered quote.
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
    /// The already-rendered original, boxed, when [`row`](Self::row) is
    /// a reblog; `None` for anything else. The nested entity is rendered
    /// from the original's own `StatusSource`, one level deep — Pleroma's
    /// recursion into `show.json` for an `Announce`.
    pub reblog: Option<Box<Status>>,
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
    /// The rich-media card for the post's first link; `null` until the
    /// link-preview ticket renders it from one more batched loader.
    pub card: Option<Value>,
    /// The poll attached to the post; `null` until the polls ticket
    /// renders it.
    pub poll: Option<Value>,
    /// When the content last changed, in the Mastodon time format, or
    /// `null` for an unedited post.
    #[serde(with = "crate::util::time::serde_mastodon_time::option")]
    pub edited_at: Option<DateTime<Utc>>,
    /// Whether the viewer favourited this post; `false` until the
    /// favourites ticket preloads them and passes them in.
    pub favourited: JsonBool,
    /// Whether the viewer reblogged this post; `false` until the same
    /// ticket's batched read of the viewer's own reblogs lands.
    pub reblogged: JsonBool,
    /// Whether the viewer muted this post's thread; `false` until the
    /// thread-mute ticket lands.
    pub muted: JsonBool,
    /// Whether the viewer bookmarked this post; `false` until the
    /// bookmarks ticket lands.
    pub bookmarked: JsonBool,
    /// Whether this post is pinned on its author's profile; `false` (and
    /// `pleroma.pinned_at` `null`) until the pinning ticket lands.
    pub pinned: JsonBool,
    /// The quoted status as a nested entity; `null` until the quoting
    /// ticket renders it — from the originals the service already
    /// batch-loads. (`quote_id` is not emitted at all.)
    pub quote: Option<Value>,
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
            reblog,
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
            card: None,
            poll: None,
            edited_at: row.edited_at,
            favourited: JsonBool(false),
            reblogged: JsonBool(false),
            muted: JsonBool(false),
            bookmarked: JsonBool(false),
            pinned: JsonBool(false),
            quote: None,
            pleroma: pleroma_block(row, reply_author),
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

/// Builds the `pleroma` block of `row`, whose replied-to author is
/// `reply_author` when the batch knew one; see [`PleromaMetadata`] for
/// where each key comes from and which later tickets fill the
/// placeholders.
fn pleroma_block(row: &StatusRow, reply_author: Option<&User>) -> PleromaMetadata {
    PleromaMetadata {
        content: plain_text(row),
        spoiler_text: PlainText {
            text_plain: row.spoiler_text.clone(),
        },
        conversation_id: row.conversation_id.to_string(),
        direct_conversation_id: None,
        in_reply_to_account_acct: reply_author.map(|author| author.nickname.clone()),
        local: JsonBool(row.local),
        expires_at: row.expires_at,
        thread_muted: JsonBool(false),
        emoji_reactions: Vec::new(),
        parent_visible: JsonBool(true),
        quotes_count: row.quotes_count,
        bookmark_folder: None,
        pinned_at: None,
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
    /// Every JSON key the entity must emit, sorted.
    const EXPECTED_KEYS: [&str; 31] = [
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
    /// whose replied-to author, and the three per-status lists.
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
            reblog: reblog.map(Box::new),
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
                reblog: None,
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
        assert_eq!(value["card"], Value::Null, "cards are a later ticket");
        assert_eq!(value["poll"], Value::Null, "polls are a later ticket");
        assert_eq!(value["edited_at"], Value::Null);
        for flag in ["favourited", "reblogged", "muted", "bookmarked", "pinned"] {
            assert_eq!(value[flag], false, "{flag} is a later ticket");
        }
        assert_eq!(value["quote"], Value::Null, "quotes are a later ticket");
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
        assert_eq!(pleroma["emoji_reactions"], json!([]));
        assert_eq!(pleroma["parent_visible"], true);
        assert_eq!(pleroma["quotes_count"], 0);
        assert_eq!(pleroma["bookmark_folder"], Value::Null);
        assert_eq!(pleroma["pinned_at"], Value::Null);
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
