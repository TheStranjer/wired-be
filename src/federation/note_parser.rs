//! Inbound `Note`-family objects: parsing a federated post document.
//!
//! [`parse_note`] turns the JSON of a received `ActivityPub` object into a
//! [`ParsedNote`]: a [`NewStatus`] ready for the `statuses` table plus the
//! satellites the insert cannot carry yet — hashtag names, mention hrefs,
//! custom emojis, media attachments, the thread/quote/context references and
//! a `Question`'s poll. The reference behaviour is Pleroma's
//! `Pleroma.Web.ActivityPub.ObjectValidators.ArticleNotePageValidator` and
//! `CommonFixes` (`lib/pleroma/web/activity_pub/object_validators/`), plus
//! the object arm of `Pleroma.Web.ActivityPub.Transmogrifier`
//! (`lib/pleroma/web/activity_pub/transmogrifier.ex`); the ticket rules here
//! are authoritative where the two differ.
//!
//! The function is pure: no database, no network, no clock. The author is
//! the already-stored [`User`] mirror the activity was addressed by, and
//! `now` is the caller's current time, used for the `published` clamping.
//! Folding a [`ParsedNote`] into rows — resolving mentions, fetching media,
//! writing the status — is the dispatcher ticket's work, not this module's.
//!
//! Validation is deliberately narrow, mirroring Pleroma's stance: the
//! document must be an accepted object `type`, carry a usable `id`, pass the
//! containment rules of [`ParseError::Containment`], and — for a `Question`
//! only — hold a valid poll. Everything optional that is malformed is
//! dropped rather than rejected: a non-object `attachment` entry, a tag
//! without a `type`, an unparseable `updated`. The `ActivityPub` shape
//! freedom (string vs one-element array vs reference object) is resolved by
//! the shared normalizers of [`crate::federation::json`].

use std::{error::Error, fmt};

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value;
use url::Url;

use crate::{
    federation::json::{as_array, first_string, value_or_id},
    models::{
        status::{NewStatus, Visibility},
        user::User,
    },
    util::{
        format::truncate_chars,
        sanitize::{escape_html, sanitize_html, strip_html},
        time::parse_time,
    },
};

/// Object `type`s this parser accepts: the eight document kinds Mastodon,
/// Pleroma and Misskey deliver as createable posts (Pleroma's transmogrifier
/// object arm accepts these plus `Answer` and `ChatMessage`, which this
/// instance's ticket folds in later). The same list is the key set of the
/// dispatcher's `Create` sub-registry: exactly these types are what
/// [`crate::federation::handlers::create`] registers for.
pub(crate) const OBJECT_TYPES: [&str; 8] = [
    TYPE_NOTE,
    TYPE_QUESTION,
    TYPE_ARTICLE,
    TYPE_PAGE,
    TYPE_VIDEO,
    TYPE_AUDIO,
    TYPE_IMAGE,
    TYPE_EVENT,
];
/// The plain `ActivityStreams` note.
const TYPE_NOTE: &str = "Note";
/// The poll-carrying question object.
const TYPE_QUESTION: &str = "Question";
/// A long-form article object.
const TYPE_ARTICLE: &str = "Article";
/// A generic page object.
const TYPE_PAGE: &str = "Page";
/// A video post object.
const TYPE_VIDEO: &str = "Video";
/// An audio post object.
const TYPE_AUDIO: &str = "Audio";
/// An image post object.
const TYPE_IMAGE: &str = "Image";
/// A calendar event object.
const TYPE_EVENT: &str = "Event";

/// The `attachment` entry `type`s that become media rows; everything else
/// in the array (including Pleroma's `Hashtag`-typed leftovers) is skipped.
const ATTACHMENT_TYPES: [&str; 5] = ["Document", "Image", "Video", "Audio", "Link"];

/// The three spellings of the public addressing recipient: the full
/// `ActivityStreams` iri, the prefix form Pleroma emits, and the bare word
/// older software sends (Pleroma's `fix_addressing_public` normalizes all
/// three to the iri). Shared with
/// [`crate::federation::publisher`], which likewise never treats any of
/// them as a deliverable recipient.
pub(crate) const PUBLIC_RECIPIENTS: [&str; 3] = [
    "https://www.w3.org/ns/activitystreams#Public",
    "as:Public",
    "Public",
];

/// The quote-reference fields, tried in order: Mastodon's and Fedibird's
/// `quoteUrl`/`quoteUri` and Misskey's `_misskey_quote` (Pleroma's
/// `CommonFixes.fix_quote_url`).
const QUOTE_KEYS: [&str; 3] = ["quoteUrl", "quoteUri", "_misskey_quote"];

/// The thread-reference fields, tried in order: `context`, the generic
/// `conversation`, and the `ostatus:conversation` Pleroma writes (whose
/// object form carries its target under `href`).
const CONTEXT_KEYS: [&str; 3] = ["context", "conversation", "ostatus:conversation"];

/// The size cap of a received document: anything whose serialization is
/// longer than this is refused outright (Pleroma's inbox caps bodies; this
/// is the same budget measured on the parsed value).
const MAX_NOTE_BYTES: usize = 2 * 1024 * 1024;
/// The stored-content cap: sanitized HTML longer than this is truncated
/// silently rather than rejected.
const MAX_CONTENT_CHARS: usize = 200_000;
/// The [`NewStatus::spoiler_text`] cap, truncated silently.
const MAX_SPOILER_CHARS: usize = 1000;
/// How many `tag` entries are examined at most (Pleroma caps the tag list
/// before folding; extra entries are ignored, not an error).
const MAX_TAGS: usize = 100;
/// The longest hashtag name kept, `#` removed (Pleroma's
/// `:max_hashtag_length` default of 100; longer names are not tags).
const MAX_HASHTAG_CHARS: usize = 100;
/// How many attachments are kept at most; the rest of the array is ignored.
const MAX_ATTACHMENTS: usize = 20;
/// The fewest poll options a `Question` may carry (Mastodon's client-side
/// two-option minimum, enforced here on the remote document).
const MIN_POLL_OPTIONS: usize = 2;
/// The most poll options a `Question` may carry.
const MAX_POLL_OPTIONS: usize = 20;
/// How far into the future a `published` timestamp may lead [`now`] before
/// it is treated as clock skew and clamped (Pleroma's
/// `ObjectValidators.Utils`-style tolerance, the Mastodon-family norm).
const CLOCK_SKEW: TimeDelta = TimeDelta::minutes(5);
/// The `content_type` a document without `source.mediaType` describes: AP
/// `content` is HTML (decision 6 stores what the remote rendered).
const DEFAULT_HTML_CONTENT_TYPE: &str = "text/html";

/// Every way a received object document can fail to become a status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The document is not a usable object: a required field is missing or
    /// malformed. The string names the offending field.
    Invalid(String),
    /// The object's `type` is not one of the eight accepted object types.
    /// The string is the type seen, or `"<missing>"` when the field carries
    /// none.
    UnsupportedType(String),
    /// The object does not belong to the author it arrived under:
    /// `attributedTo` is unusable or names someone else, or its host differs
    /// from the object id's host (Pleroma's `validate_host_match`).
    Containment(String),
    /// A `Question` whose `oneOf`/`anyOf` is absent or holds fewer than
    /// two or more than twenty options.
    InvalidPoll(String),
    /// The document, serialized, is larger than two mebibytes.
    TooLarge,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(detail) => write!(f, "invalid note object: {detail}"),
            Self::UnsupportedType(kind) => write!(f, "unsupported object type: {kind}"),
            Self::Containment(detail) => write!(f, "note fails containment: {detail}"),
            Self::InvalidPoll(detail) => write!(f, "invalid poll: {detail}"),
            Self::TooLarge => write!(f, "the note document exceeds the size cap"),
        }
    }
}

impl Error for ParseError {}

/// A received object document parsed into insert-ready parts.
///
/// [`status`](Self::status) carries everything that belongs on the
/// `statuses` row itself (the author's `user_id` included); the rest are the
/// references only the dispatcher can resolve — mention hrefs to users,
/// `reply_to_uri` to a stored status, attachment urls to fetched media,
/// `quote_uri` to an object it may have to fetch.
#[derive(Clone, Debug)]
pub struct ParsedNote {
    /// The status row to insert: `uri`, `url`, text, `spoiler_text`,
    /// `sensitive`, `visibility`, `inserted_at` / `edited_at` /
    /// `expires_at`. Everything keyed by rows this parse cannot see
    /// (replies, quotes, media, mentions) is left at its default for the
    /// dispatcher to fill.
    pub status: NewStatus,
    /// Hashtag names from the `tag` list, without `#`, lowercased.
    pub hashtags: Vec<String>,
    /// The `href`s of the `Mention` tags, in document order; resolving them
    /// to user ids is the dispatcher's job.
    pub mention_hrefs: Vec<String>,
    /// The `Emoji` tags as remote custom emojis to upsert.
    pub emojis: Vec<RemoteEmoji>,
    /// The `attachment` entries kept as media to store, in document order.
    pub attachments: Vec<ParsedAttachment>,
    /// The object `inReplyTo` names, string or reference object; `None` for
    /// a root post.
    pub reply_to_uri: Option<String>,
    /// The object the post quotes (`quoteUrl` / `quoteUri` /
    /// `_misskey_quote`), `None` for a plain post.
    pub quote_uri: Option<String>,
    /// The thread collection `context` (or `conversation` /
    /// `ostatus:conversation`) names.
    pub ap_context: Option<String>,
    /// The poll of a `Question`; `None` for every other object type.
    pub poll: Option<ParsedPoll>,
}

/// A custom emoji announced by a remote post's `Emoji` tag, as the
/// dispatcher feeds [`NewCustomEmoji`](crate::models::custom_emoji::NewCustomEmoji).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEmoji {
    /// The emoji's name with the surrounding colons removed.
    pub shortcode: String,
    /// The image url of the tag's `icon`, an absolute `http(s)` url.
    pub url: String,
    /// The origin domain: the announcing author's stored `domain`.
    pub domain: Option<String>,
}

/// One kept `attachment` entry, the media columns a remote post states.
///
/// The dispatcher turns each of these into a
/// [`NewMediaAttachment`](crate::models::media_attachment::NewMediaAttachment)
/// — the `position` within the status is that call's, the parse keeps the
/// document order in the enclosing [`Vec`].
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedAttachment {
    /// The `http(s)` url of the media or link.
    pub url: String,
    /// The declared `mediaType`, or the type guessed from the url's file
    /// extension.
    pub content_type: String,
    /// The entry's `name`: the alt text of media, the label of a link.
    pub description: Option<String>,
    /// The `blurhash` placeholder string.
    pub blurhash: Option<String>,
    /// Width in pixels, when the entry states a positive whole one.
    pub width: Option<i32>,
    /// Height in pixels, when the entry states a positive whole one.
    pub height: Option<i32>,
    /// Horizontal focal point from `focalPoint`, if the entry carries one.
    pub focus_x: Option<f32>,
    /// Vertical focal point from `focalPoint`, if the entry carries one.
    pub focus_y: Option<f32>,
}

/// A `Question`'s poll as the document states it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedPoll {
    /// The choices, in document order.
    pub options: Vec<ParsedPollOption>,
    /// Whether voters may pick more than one option (`anyOf` vs `oneOf`).
    pub multiple: bool,
    /// When voting closes, from `endTime` or a timestamp `closed`.
    pub expires_at: Option<DateTime<Utc>>,
    /// `votersCount` as sent.
    pub voters_count: Option<i64>,
}

/// One choice of a [`ParsedPoll`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedPollOption {
    /// The option's text.
    pub title: String,
    /// Its vote count (`replies.totalItems`) as the remote reported it.
    pub votes_count: i64,
}

/// The text fields of a parsed document: rendered `content` with the
/// `source` it claims.
#[derive(Clone, Debug)]
struct NoteText {
    /// Sanitized, capped HTML.
    content: String,
    /// The author's own source text, or the content reduced to text.
    raw_text: String,
    /// The `MIME` type of [`raw_text`](Self::raw_text).
    content_type: String,
}

/// The timestamps a document arrives with, resolved against the caller's
/// `now`.
#[derive(Clone, Debug)]
struct NoteTimes {
    /// The clamped `published`.
    inserted_at: DateTime<Utc>,
    /// The `updated`, when it postdates [`inserted_at`](Self::inserted_at).
    edited_at: Option<DateTime<Utc>>,
}

/// The three tag lists a document's `tag` array splits into.
#[derive(Clone, Debug, Default)]
struct TagSets {
    /// Lowercased hashtag names.
    hashtags: Vec<String>,
    /// Mention hrefs.
    mention_hrefs: Vec<String>,
    /// Remote custom emojis.
    emojis: Vec<RemoteEmoji>,
}

/// Parses a received `Note`-family object into the parts of a status.
///
/// `author` is the stored mirror of the actor that sent the activity: the
/// document's `attributedTo` must name exactly that actor and share the
/// object id's host, or the parse fails containment. `now` anchors the
/// `published` clamp. The returned [`NewStatus`] is remote (`local` unset)
/// with its `uri`, `url`, text, `spoiler_text`, `sensitive`, `visibility`
/// and timestamps filled; every field that depends on other rows — reply,
/// quote, mentions, media, poll — stays on [`ParsedNote`] for the caller.
///
/// # Errors
///
/// [`ParseError::TooLarge`] when the serialized document exceeds two
/// mebibytes; [`ParseError::UnsupportedType`] for any `type` outside the
/// eight accepted object kinds; [`ParseError::Invalid`] when `id` is missing
/// or not an absolute `http(s)` url; [`ParseError::Containment`] when
/// `attributedTo` fails the authorship or host rules;
/// [`ParseError::InvalidPoll`] when a `Question`'s option list is missing or
/// holds fewer than two or more than twenty options.
pub fn parse_note(
    json: &Value,
    author: &User,
    now: DateTime<Utc>,
) -> Result<ParsedNote, ParseError> {
    check_size(json)?;
    let kind = object_kind(json)?;
    let (id, id_host) = object_id(json)?;
    check_containment(json, author, &id_host)?;
    let url = resolve_url(json, &id);
    let text = parse_text(json, kind, &url);
    let summary = json
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let times = parse_times(json, now);
    let poll = parse_poll(json, kind)?;
    let tags = parse_tags(json, author);
    let status = NewStatus {
        uri: Some(id),
        url: Some(url),
        user_id: author.id,
        content: text.content,
        raw_text: text.raw_text,
        content_type: text.content_type,
        spoiler_text: truncate_chars(&strip_html(summary), MAX_SPOILER_CHARS),
        sensitive: parse_sensitive(json, !summary.trim().is_empty()),
        visibility: parse_visibility(json, author),
        edited_at: times.edited_at,
        expires_at: poll.as_ref().and_then(|parsed| parsed.expires_at),
        inserted_at: Some(times.inserted_at),
        ..NewStatus::default()
    };
    Ok(ParsedNote {
        status,
        hashtags: tags.hashtags,
        mention_hrefs: tags.mention_hrefs,
        emojis: tags.emojis,
        attachments: parse_attachments(json),
        reply_to_uri: json.get("inReplyTo").and_then(value_or_id),
        quote_uri: field_reference(json, &QUOTE_KEYS),
        ap_context: field_reference(json, &CONTEXT_KEYS),
        poll,
    })
}

/// Refuses documents whose serialization is larger than [`MAX_NOTE_BYTES`].
fn check_size(json: &Value) -> Result<(), ParseError> {
    let bytes = serde_json::to_vec(json)
        .map_err(|err| ParseError::Invalid(format!("the document cannot be serialized: {err}")))?;
    if bytes.len() > MAX_NOTE_BYTES {
        return Err(ParseError::TooLarge);
    }
    Ok(())
}

/// The document's `type`, validated against [`OBJECT_TYPES`]; anything else
/// — including a missing or non-string type — is
/// [`ParseError::UnsupportedType`].
fn object_kind(json: &Value) -> Result<&'static str, ParseError> {
    let seen = json.get("type").and_then(Value::as_str);
    OBJECT_TYPES
        .into_iter()
        .find(|kind| Some(*kind) == seen)
        .ok_or_else(|| ParseError::UnsupportedType(seen.unwrap_or("<missing>").to_owned()))
}

/// The object's `id` and its host: a plain string (objects name themselves,
/// like actors) spelling an absolute `http(s)` url. The id is kept verbatim,
/// the host as URL parsing lowercases it.
fn object_id(json: &Value) -> Result<(String, String), ParseError> {
    let raw = json
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ParseError::Invalid("`id` is missing or not a string".to_owned()))?;
    let host = http_host(raw)
        .ok_or_else(|| ParseError::Invalid("`id` is not an absolute http(s) URL".to_owned()))?;
    Ok((raw.to_owned(), host))
}

/// The authorship rules of the ticket: the first `id` (or bare string)
/// reachable in `attributedTo` must equal `author.ap_id`, and the host it
/// names must equal the object id's host. Pleroma applies the same pair in
/// `validate_fields_match` and `validate_host_match`.
fn check_containment(json: &Value, author: &User, id_host: &str) -> Result<(), ParseError> {
    let attributed = json
        .get("attributedTo")
        .map_or_default(as_array)
        .iter()
        .find_map(value_or_id)
        .ok_or_else(|| {
            ParseError::Containment("`attributedTo` is missing or names nothing".to_owned())
        })?;
    if attributed != author.ap_id {
        return Err(ParseError::Containment(format!(
            "`attributedTo` {attributed} is not the author {}",
            author.ap_id
        )));
    }
    let actor_host = http_host(&attributed).ok_or_else(|| {
        ParseError::Containment("`attributedTo` is not an http(s) URL".to_owned())
    })?;
    if actor_host != id_host {
        return Err(ParseError::Containment(format!(
            "the object host {id_host} differs from the actor host {actor_host}"
        )));
    }
    Ok(())
}

/// The host of an absolute `http(s)` URL, `None` for anything else; URL
/// parsing lowercases the host, so hosts compare case-insensitively.
fn http_host(text: &str) -> Option<String> {
    let parsed = Url::parse(text).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    parsed.host_str().map(str::to_owned)
}

/// True when `text` is an absolute `http(s)` URL with a host.
fn is_http_url(text: &str) -> bool {
    http_host(text).is_some()
}

/// The human-facing url: the first string or `Link` `href` of `url` when it
/// is an `http(s)` URL, the object id otherwise (Pleroma's `fix_url`).
fn resolve_url(json: &Value, id: &str) -> String {
    json.get("url")
        .and_then(first_string)
        .filter(|candidate| is_http_url(candidate))
        .unwrap_or_else(|| id.to_owned())
}

/// The document's rendered text: `content`, or the first value of
/// `contentMap` when `content` is absent; `Article` and `Page` get the
/// title/link header Pleroma's clients render for them (the ticket's shape).
/// The result is sanitized and capped; the source text and its MIME type
/// come from `source` when it is an object stating them.
fn parse_text(json: &Value, kind: &str, url: &str) -> NoteText {
    let body = document_body(json);
    let markup = if matches!(kind, TYPE_ARTICLE | TYPE_PAGE) {
        format!("{}{body}", article_header(json.get("title"), url))
    } else {
        body
    };
    let content = truncate_chars(&sanitize_html(&markup), MAX_CONTENT_CHARS);
    let source = json.get("source");
    let raw_text = source
        .and_then(|object| object.get("content"))
        .and_then(Value::as_str)
        .map_or_else(|| strip_html(&content), str::to_owned);
    let content_type = source
        .and_then(|object| object.get("mediaType"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_HTML_CONTENT_TYPE)
        .to_owned();
    NoteText {
        content,
        raw_text,
        content_type,
    }
}

/// The document's HTML body: the string under `content`, else the first
/// value of `contentMap`, else empty (a non-string `content` reads as
/// absent).
fn document_body(json: &Value) -> String {
    json.get("content")
        .and_then(Value::as_str)
        .or_else(|| {
            json.get("contentMap")
                .and_then(Value::as_object)
                .and_then(|entries| entries.values().next())
                .and_then(Value::as_str)
        })
        .unwrap_or_default()
        .to_owned()
}

/// The header `Article` and `Page` documents render in front of their body:
/// the title as a bold first paragraph, then the post's url as a link. A
/// titleless article gets only the link paragraph. The result goes through
/// [`sanitize_html`] with the body, so the link gains the forced `rel` and
/// any hostile url loses its `href`.
fn article_header(title: Option<&Value>, url: &str) -> String {
    match title.and_then(Value::as_str) {
        Some(title) if !title.trim().is_empty() => format!(
            r#"<p><strong>{}</strong></p><p><a href="{url}">{url}</a></p>"#,
            escape_html(title)
        ),
        _ => format!(r#"<p><a href="{url}">{url}</a></p>"#),
    }
}

/// The timestamps: `published` clamped to `now` when it leads `now` by more
/// than [`CLOCK_SKEW`] (a remote clock cannot push a post into the future),
/// and `updated` kept as the edit time only when it postdates that.
fn parse_times(json: &Value, now: DateTime<Utc>) -> NoteTimes {
    let ceiling = now.checked_add_signed(CLOCK_SKEW).unwrap_or(now);
    let inserted_at = timestamp_field(json, "published")
        .filter(|published| *published <= ceiling)
        .unwrap_or(now);
    let edited_at = timestamp_field(json, "updated").filter(|updated| *updated > inserted_at);
    NoteTimes {
        inserted_at,
        edited_at,
    }
}

/// The RFC 3339 timestamp under `field`, `None` when absent or
/// unparseable.
fn timestamp_field(json: &Value, field: &str) -> Option<DateTime<Utc>> {
    json.get(field).and_then(Value::as_str).and_then(parse_time)
}

/// The `sensitive` flag with Pleroma's summary rule: the field's boolean
/// value when it carries one; otherwise a post with a non-empty `summary`
/// is sensitive, and everything else is not.
fn parse_sensitive(json: &Value, has_spoiler: bool) -> bool {
    json.get("sensitive")
        .and_then(Value::as_bool)
        .unwrap_or(has_spoiler)
}

/// The reference carried by the first of `keys` that yields one: values are
/// read through [`value_or_id`], so a `null` (or any reference-less value)
/// under an earlier key falls through to a later one, like Pleroma's `||`
/// chains in `fix_object_defaults` and `fix_quote_url` do.
fn field_reference(json: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| json.get(*key))
        .find_map(value_or_id)
}

/// The visibility from `to` and `cc`: the public recipient in `to` is
/// public, in `cc` alone unlisted, the author's followers collection (and
/// nobody public) private, anything else direct.
fn parse_visibility(json: &Value, author: &User) -> Visibility {
    let to = recipients(json, "to");
    let cc = recipients(json, "cc");
    if has_public(&to) {
        Visibility::Public
    } else if has_public(&cc) {
        Visibility::Unlisted
    } else if addresses_followers(&to, &cc, author.followers_url.as_deref()) {
        Visibility::Private
    } else {
        Visibility::Direct
    }
}

/// The recipient strings of an addressing field: every string, reference
/// object and array of either, flattened through [`as_array`] and
/// [`value_or_id`].
fn recipients(json: &Value, field: &str) -> Vec<String> {
    json.get(field)
        .map_or_default(as_array)
        .iter()
        .filter_map(value_or_id)
        .collect()
}

/// True when any recipient is the public address, in any of its
/// [`PUBLIC_RECIPIENTS`] spellings.
fn has_public(recipients: &[String]) -> bool {
    recipients
        .iter()
        .any(|recipient| PUBLIC_RECIPIENTS.contains(&recipient.as_str()))
}

/// True when either list names the author's followers collection.
fn addresses_followers(to: &[String], cc: &[String], followers_url: Option<&str>) -> bool {
    followers_url.is_some_and(|url| {
        to.iter().any(|recipient| recipient == url) || cc.iter().any(|recipient| recipient == url)
    })
}

/// The first `tag` entry of each kind, [`MAX_TAGS`] entries examined at
/// most; unknown tag types and non-object entries are ignored (Pleroma's
/// `fix_tag` keeps only objects).
fn parse_tags(json: &Value, author: &User) -> TagSets {
    let mut sets = TagSets::default();
    for entry in json
        .get("tag")
        .map_or_default(as_array)
        .iter()
        .take(MAX_TAGS)
    {
        match entry.get("type").and_then(Value::as_str) {
            Some("Mention") => sets
                .mention_hrefs
                .extend(entry.get("href").and_then(value_or_id)),
            Some("Hashtag") => sets.hashtags.extend(parse_hashtag(entry)),
            Some("Emoji") => sets.emojis.extend(parse_emoji(entry, author)),
            Some(_) | None => {}
        }
    }
    sets
}

/// One `Hashtag` tag: its `name` without the leading `#`, lowercased;
/// empty or over-long ([`MAX_HASHTAG_CHARS`] characters) names are skipped.
fn parse_hashtag(entry: &Value) -> Option<String> {
    let name = entry.get("name").and_then(Value::as_str)?;
    let tag = name.strip_prefix('#').unwrap_or(name);
    if tag.is_empty() || tag.chars().count() > MAX_HASHTAG_CHARS {
        return None;
    }
    Some(tag.to_lowercase())
}

/// One `Emoji` tag: the colon-stripped `name` and the first `http(s)` url
/// under `icon`, in the author's domain (Pleroma's `fix_emoji` reads the
/// same fields). A tag with neither is skipped.
fn parse_emoji(entry: &Value, author: &User) -> Option<RemoteEmoji> {
    let shortcode = entry
        .get("name")
        .and_then(Value::as_str)?
        .trim_matches(':')
        .to_owned();
    if shortcode.is_empty() {
        return None;
    }
    let url = entry
        .get("icon")
        .map_or_default(as_array)
        .iter()
        .filter_map(first_string)
        .find(|candidate| is_http_url(candidate))?;
    Some(RemoteEmoji {
        shortcode,
        url,
        domain: author.domain.clone(),
    })
}

/// The kept `attachment` entries, at most [`MAX_ATTACHMENTS`] of them, in
/// document order.
fn parse_attachments(json: &Value) -> Vec<ParsedAttachment> {
    json.get("attachment")
        .map_or_default(as_array)
        .iter()
        .filter_map(parse_attachment)
        .take(MAX_ATTACHMENTS)
        .collect()
}

/// One `attachment` entry: of an [`ATTACHMENT_TYPES`] kind, carrying an
/// `http(s)` url and either a declared `mediaType` or one guessable from
/// the url's extension; anything else is skipped rather than rejected.
fn parse_attachment(entry: &Value) -> Option<ParsedAttachment> {
    let kind = entry.get("type").and_then(Value::as_str)?;
    if !ATTACHMENT_TYPES.contains(&kind) {
        return None;
    }
    let url_value = entry.get("url")?;
    let url = first_string(url_value).filter(|candidate| is_http_url(candidate))?;
    let content_type =
        declared_media_type(entry, url_value).or_else(|| guess_content_type(&url))?;
    let (focus_x, focus_y) = focal_point(entry.get("focalPoint"));
    Some(ParsedAttachment {
        url,
        content_type,
        description: entry.get("name").and_then(Value::as_str).map(str::to_owned),
        blurhash: entry
            .get("blurhash")
            .and_then(Value::as_str)
            .map(str::to_owned),
        width: pixel_dimension(entry.get("width")),
        height: pixel_dimension(entry.get("height")),
        focus_x,
        focus_y,
    })
}

/// The declared `mediaType`: the entry's own, or the one its `url` object
/// carries (Mastodon links can hide it there), `None` when neither is a
/// non-empty string.
fn declared_media_type(entry: &Value, url_value: &Value) -> Option<String> {
    entry
        .get("mediaType")
        .or_else(|| url_value.get("mediaType"))
        .and_then(Value::as_str)
        .filter(|declared| !declared.trim().is_empty())
        .map(str::to_owned)
}

/// The `MIME` type guessed from the last path segment's file extension of
/// an `http(s)` url, `None` for an unknown one (such an attachment is
/// skipped: no type, no row).
fn guess_content_type(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let file = parsed.path().rsplit('/').next()?;
    let (_, extension) = file.rsplit_once('.')?;
    let guessed = match extension.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        _ => return None,
    };
    Some(guessed.to_owned())
}

/// A positive whole pixel measure, `None` for a missing, non-numeric,
/// zero, negative or unrepresentable value. Also read by the chat handler,
/// whose attachment entries state dimensions the same way.
pub(crate) fn pixel_dimension(value: Option<&Value>) -> Option<i32> {
    let whole = value?.as_i64()?;
    if whole <= 0 {
        return None;
    }
    i32::try_from(whole).ok()
}

/// The `focalPoint` `[x, y]` pair, each axis `None` unless the array
/// carries a JSON number at that position; numbers are read at the `f32`
/// width the media columns store, so strings and other shapes drop the
/// axis rather than failing the entry.
fn focal_point(value: Option<&Value>) -> (Option<f32>, Option<f32>) {
    let points = value.map_or_default(as_array);
    let axis = |index: usize| {
        points
            .get(index)
            .and_then(|number| serde_json::from_value::<f32>(number.clone()).ok())
    };
    (axis(0), axis(1))
}

/// The poll of a `Question` — `oneOf` for a single-choice, `anyOf` for a
/// multi-choice — or `None` for every other type, whose option lists (if
/// any) are ignored. An invalid `Question` list fails [`ParseError::InvalidPoll`].
fn parse_poll(json: &Value, kind: &str) -> Result<Option<ParsedPoll>, ParseError> {
    if kind != TYPE_QUESTION {
        return Ok(None);
    }
    let (entries, multiple) = match (json.get("oneOf"), json.get("anyOf")) {
        (Some(options), _) => (options, false),
        (None, Some(options)) => (options, true),
        (None, None) => {
            return Err(ParseError::InvalidPoll(
                "the question carries neither oneOf nor anyOf".to_owned(),
            ));
        }
    };
    let options: Vec<ParsedPollOption> = as_array(entries).iter().filter_map(poll_option).collect();
    if options.len() < MIN_POLL_OPTIONS || options.len() > MAX_POLL_OPTIONS {
        return Err(ParseError::InvalidPoll(format!(
            "the question carries {} options, {MIN_POLL_OPTIONS} to {MAX_POLL_OPTIONS} are required",
            options.len()
        )));
    }
    Ok(Some(ParsedPoll {
        options,
        multiple,
        expires_at: poll_expiry(json),
        voters_count: json.get("votersCount").and_then(Value::as_i64),
    }))
}

/// One poll option: its `name` and the `replies.totalItems` count as
/// reported; entries without a name are skipped.
fn poll_option(entry: &Value) -> Option<ParsedPollOption> {
    let title = entry.get("name").and_then(Value::as_str)?;
    if title.is_empty() {
        return None;
    }
    let votes_count = entry
        .get("replies")
        .and_then(|replies| replies.get("totalItems"))
        .and_then(Value::as_i64)
        .unwrap_or_default();
    Some(ParsedPollOption {
        title: title.to_owned(),
        votes_count,
    })
}

/// When voting closes: `endTime`, else a `closed` given as a timestamp
/// (Mastodon closes polls by rewriting `closed`; a boolean there carries
/// no time).
fn poll_expiry(json: &Value) -> Option<DateTime<Utc>> {
    ["endTime", "closed"]
        .iter()
        .find_map(|key| timestamp_field(json, key))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::models::user::{DEFAULT_SCOPE, Flag};

    /// The id every fixture author row carries.
    const ACCOUNT_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// The actor the fixtures are authored by.
    const AUTHOR_AP_ID: &str = "https://remote.example/users/bob";
    /// The fixture author's followers collection.
    const FOLLOWERS: &str = "https://remote.example/users/bob/followers";
    /// The public recipient, full `ActivityStreams` spelling.
    const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

    /// The instant every parse is anchored at: 2024-03-01T00:00:00Z.
    fn now() -> DateTime<Utc> {
        parse_time("2024-03-01T00:00:00Z").unwrap()
    }

    /// The stored mirror of the fixture author: remote `bob@remote.example`
    /// with its domain and followers collection known.
    fn author() -> User {
        User {
            id: Uuid::parse_str(ACCOUNT_ID).unwrap(),
            nickname: "bob@remote.example".to_owned(),
            ap_id: AUTHOR_AP_ID.to_owned(),
            local: Flag::new(false),
            domain: Some("remote.example".to_owned()),
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
            followers_url: Some(FOLLOWERS.to_owned()),
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
            default_scope: DEFAULT_SCOPE.to_owned(),
            sensitive_default: Flag::new(false),
            language: None,
            tags: Vec::new(),
            settings_store: json!({}),
            notification_settings: json!({}),
            mfa_settings: crate::models::user::mfa::MfaSettings::default_value(),
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
            inserted_at: parse_time("2024-01-31T12:34:56.789Z").unwrap(),
            updated_at: parse_time("2024-01-31T12:34:56.789Z").unwrap(),
            preferred_frontend: None,
            is_suggested: Flag::new(false),
            password_reset_pending: Flag::new(false),
            email_notifications: Flag::new(true),
            deleted_at: None,
        }
    }

    /// The Mastodon fixture: every field in its common single-value or
    /// `Link`-object spelling, hostile markup included.
    const MASTODON: &str = include_str!("../../tests/fixtures/ap/mastodon_note.json");
    /// The Pleroma fixture: the array, object and bare-field spellings,
    /// `contentMap`, `as:Public` and the `ostatus:conversation` context.
    const PLEROMA: &str = include_str!("../../tests/fixtures/ap/pleroma_note.json");
    /// The `Question` fixture: a finished single-choice poll with counts.
    const QUESTION: &str = include_str!("../../tests/fixtures/ap/question.json");
    /// The `Article` fixture: a title and body to render.
    const ARTICLE: &str = include_str!("../../tests/fixtures/ap/article.json");
    /// The Misskey fixture: `_misskey_quote`, `_misskey_content`, a
    /// `javascript:` icon and a non-url `url`.
    const MISSKEY: &str = include_str!("../../tests/fixtures/ap/misskey_note.json");

    /// Reads a fixture string as JSON; a broken fixture fails the test.
    fn fixture(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    /// Parses `document` by the fixture author at [`now`]; documents the
    /// ticket expects to parse.
    fn parse(document: &Value) -> ParsedNote {
        parse_note(document, &author(), now()).unwrap()
    }

    /// The smallest document that parses: a public note with one sentence.
    fn minimal_note() -> Value {
        json!({
            "id": "https://remote.example/objects/minimal",
            "type": "Note",
            "attributedTo": AUTHOR_AP_ID,
            "to": [PUBLIC],
            "cc": [FOLLOWERS],
            "content": "<p>hello</p>",
        })
    }

    /// Removes `field` from the document's top-level object.
    fn remove(document: &mut Value, field: &str) {
        let _gone = document.as_object_mut().unwrap().remove(field);
    }

    #[test]
    fn parses_a_mastodon_note() {
        let parsed = parse(&fixture(MASTODON));
        assert_eq!(
            parsed.status.uri.as_deref(),
            Some("https://remote.example/users/bob/statuses/01HQXYZ"),
            "the id is the uri"
        );
        assert_eq!(
            parsed.status.url.as_deref(),
            Some("https://remote.example/@bob/01HQXYZ"),
            "the Link array resolves to its href"
        );
        assert_eq!(parsed.status.user_id, author().id, "the author owns it");
        assert!(!parsed.status.local, "a federated post is not local");
        assert_eq!(parsed.status.visibility, Visibility::Public, "to is public");
        assert_eq!(parsed.status.spoiler_text, "trigger warnings", "the cw");
        assert!(parsed.status.sensitive, "and the flag");
        assert_eq!(parsed.status.content_type, "text/plain", "source.mediaType");
        assert_eq!(
            parsed.status.raw_text, "Hello world @carol https://example.org/post #Rust",
            "source.content verbatim"
        );
        assert_eq!(
            parsed.reply_to_uri.as_deref(),
            Some("https://remote.example/users/carol/statuses/01HR"),
            "inReplyTo object reads its id, not its href"
        );
        assert_eq!(
            parsed.ap_context.as_deref(),
            Some("https://remote.example/contexts/01HS"),
            "context wins over conversation"
        );
        assert_eq!(
            parsed.quote_uri.as_deref(),
            Some("https://remote.example/users/dave/statuses/01HT"),
            "quoteUrl"
        );
        assert!(parsed.poll.is_none(), "a Note carries no poll");
    }

    #[test]
    fn the_mastodon_note_is_sanitized_and_stamped() {
        let parsed = parse(&fixture(MASTODON));
        assert!(
            parsed.status.content.contains(
                r#"<a href="https://example.org/post" rel="ugc nofollow noopener noreferrer">link</a>"#
            ),
            "the link keeps its href with the forced rel, got {}",
            parsed.status.content
        );
        assert!(
            !parsed.status.content.contains("steal"),
            "script bodies and event handlers are gone, got {}",
            parsed.status.content
        );
        assert!(!parsed.status.content.contains("target="), "target goes");
        assert_eq!(
            parsed.status.inserted_at,
            Some(parse_time("2024-02-01T10:00:00.000Z").unwrap()),
            "published is the creation time"
        );
        assert_eq!(
            parsed.status.edited_at,
            Some(parse_time("2024-02-01T10:05:00.000Z").unwrap()),
            "a later updated is the edit time"
        );
    }

    #[test]
    fn the_mastodon_note_splits_its_tags() {
        let parsed = parse(&fixture(MASTODON));
        assert_eq!(
            parsed.hashtags,
            ["rust"],
            "lowercased, # dropped; the \
                   over-long name, the Show type and the bare string skipped"
        );
        assert_eq!(
            parsed.mention_hrefs,
            ["https://remote.example/users/carol"],
            "the Mention href"
        );
        assert_eq!(
            parsed.emojis,
            [RemoteEmoji {
                shortcode: "ralph".to_owned(),
                url: "https://remote.example/emoji/ralph.png".to_owned(),
                domain: Some("remote.example".to_owned()),
            }],
            "the Emoji tag, colons stripped, in the author's domain"
        );
    }

    #[test]
    fn the_mastodon_note_keeps_three_of_five_attachments() {
        let parsed = parse(&fixture(MASTODON));
        assert_eq!(
            parsed.attachments.len(),
            3,
            "the unknown extension and the Tombstone are skipped"
        );
        assert_eq!(
            parsed.attachments[0],
            ParsedAttachment {
                url: "https://remote.example/media/a.png".to_owned(),
                content_type: "image/png".to_owned(),
                description: Some("A chart".to_owned()),
                blurhash: Some("UBLf:XfQ00fQ".to_owned()),
                width: Some(1920),
                height: Some(1080),
                focus_x: Some(0.25),
                focus_y: Some(-0.5),
            },
            "the full Document entry"
        );
        assert_eq!(
            parsed.attachments[1].content_type, "video/mp4",
            "the mediaType under the url Link object is read"
        );
        assert_eq!(
            parsed.attachments[2],
            ParsedAttachment {
                url: "https://example.com/article".to_owned(),
                content_type: "text/html".to_owned(),
                description: Some("An article".to_owned()),
                blurhash: None,
                width: None,
                height: None,
                focus_x: None,
                focus_y: None,
            },
            "the Link entry keeps its declared type"
        );
    }

    #[test]
    fn parses_a_pleroma_note() {
        let parsed = parse(&fixture(PLEROMA));
        assert_eq!(
            parsed.status.content, "contentMap <b>wins</b> here",
            "the contentMap value, the unknown divs unwrapped"
        );
        assert_eq!(
            parsed.status.raw_text, "contentMap wins here",
            "a bare-string source is no source"
        );
        assert_eq!(
            parsed.status.content_type, "text/html",
            "no source.mediaType means html"
        );
        assert_eq!(
            parsed.status.visibility,
            Visibility::Unlisted,
            "as:Public in cc only"
        );
        assert_eq!(
            parsed.ap_context.as_deref(),
            Some("https://remote.example/contexts/01DEF"),
            "a null context falls through to the litepub ostatus:conversation href"
        );
        assert_eq!(
            parsed.reply_to_uri.as_deref(),
            Some("https://remote.example/objects/f19266fb-1e2f-4a4a-9c7d-2f9a9a0e9d5e"),
            "a bare-string inReplyTo"
        );
        assert_eq!(
            parsed.mention_hrefs,
            ["https://remote.example/users/carol"],
            "a bare-object tag list"
        );
        assert_eq!(parsed.attachments.len(), 1, "a bare-object attachment");
        assert_eq!(
            parsed.attachments[0].content_type, "image/jpeg",
            "guessed from the extension"
        );
        assert_eq!(
            parsed.attachments[0].width, None,
            "a string width is no width"
        );
        assert!(!parsed.status.sensitive, "no summary, no flag");
    }

    #[test]
    fn parses_a_question_with_a_poll() {
        let parsed = parse(&fixture(QUESTION));
        assert_eq!(
            parsed.status.content, "<p>Which is better?</p>",
            "plain content"
        );
        let poll = parsed.poll.expect("a Question carries a poll");
        assert!(!poll.multiple, "oneOf is a single-choice poll");
        assert_eq!(
            poll.options,
            [
                ParsedPollOption {
                    title: "Red".to_owned(),
                    votes_count: 3
                },
                ParsedPollOption {
                    title: "Blue".to_owned(),
                    votes_count: 7
                },
            ],
            "names and reported counts, in order"
        );
        assert_eq!(
            poll.expires_at,
            Some(parse_time("2024-02-10T00:00:00.000Z").unwrap()),
            "endTime closes the vote"
        );
        assert_eq!(poll.voters_count, Some(10), "votersCount");
        assert_eq!(
            parsed.status.expires_at, poll.expires_at,
            "the status expires with its poll"
        );
    }

    #[test]
    fn any_of_is_a_multiple_choice_poll() {
        let mut document = fixture(QUESTION);
        document["anyOf"] = document["oneOf"].take();
        remove(&mut document, "oneOf");
        let poll = parse(&document).poll.expect("the poll survives the rename");
        assert!(poll.multiple, "anyOf lets voters tick twice");
        assert_eq!(poll.options.len(), 2, "and carries the same options");
    }

    #[test]
    fn parses_an_article_into_a_rendered_post() {
        let parsed = parse(&fixture(ARTICLE));
        assert_eq!(
            parsed.status.content,
            format!(
                "<p><strong>On Parsing</strong></p><p><a href=\"{url}\" rel=\"ugc nofollow noopener noreferrer\">{url}</a></p><p>Body <em>text</em> with an <a href=\"https://example.org/ref\" rel=\"ugc nofollow noopener noreferrer\">external reference</a>.</p>",
                url = "https://remote.example/@bob/article/1"
            ),
            "bold title paragraph, url link, then the body"
        );
        assert_eq!(
            parsed.status.raw_text,
            "On Parsing\nhttps://remote.example/@bob/article/1\nBody text with an external reference.\n",
            "the rendered form as text"
        );
    }

    #[test]
    fn parses_a_misskey_note() {
        let parsed = parse(&fixture(MISSKEY));
        assert_eq!(
            parsed.quote_uri.as_deref(),
            Some("https://remote.example/notes/target"),
            "_misskey_quote feeds the quote"
        );
        assert_eq!(
            parsed.status.url.as_deref(),
            Some("https://remote.example/notes/mk0001"),
            "a non-url url falls back to the id"
        );
        assert_eq!(
            parsed.status.raw_text, "Quoting https://remote.example/notes/target silently\n",
            "_misskey_content is not a source: strip_html of the content, \
             its p-end becoming the documented newline"
        );
        assert_eq!(
            parsed.status.visibility,
            Visibility::Direct,
            "no public, no followers collection"
        );
        assert!(
            !parsed.status.sensitive,
            "the explicit false beats the summary"
        );
        assert_eq!(parsed.status.spoiler_text, "cw", "the summary still lands");
        assert_eq!(parsed.ap_context, None, "no context fields at all");
        assert_eq!(parsed.emojis.len(), 1, "the Emoji tag with a usable icon");
        assert_eq!(
            parsed.emojis[0].url, "https://remote.example/emoji/custom/ten.gif",
            "the javascript: icon entry is skipped"
        );
        assert_eq!(
            parsed.emojis[0].shortcode, "plainname",
            "colons are optional"
        );
    }

    /// One visibility case: the `to` and `cc` values and the outcome.
    type AddressingCase = (&'static str, Value, Value, Visibility);

    /// The addressing shapes of the ticket, string vs array vs object.
    fn addressing_cases() -> Vec<AddressingCase> {
        vec![
            (
                "public iri in to",
                json!([PUBLIC]),
                json!([]),
                Visibility::Public,
            ),
            (
                "as:Public in to",
                json!(["as:Public"]),
                json!([]),
                Visibility::Public,
            ),
            (
                "bare Public in to",
                json!(["Public"]),
                json!([]),
                Visibility::Public,
            ),
            (
                "public in cc",
                json!([FOLLOWERS]),
                json!([PUBLIC]),
                Visibility::Unlisted,
            ),
            (
                "to as a bare string",
                json!(PUBLIC),
                Value::Null,
                Visibility::Public,
            ),
            (
                "recipient as an object",
                json!([{"id": PUBLIC, "type": "Collection"}]),
                json!([]),
                Visibility::Public,
            ),
            (
                "followers in to",
                json!([FOLLOWERS]),
                json!([]),
                Visibility::Private,
            ),
            (
                "followers in cc",
                json!([AUTHOR_AP_ID]),
                json!([FOLLOWERS]),
                Visibility::Private,
            ),
            (
                "only a mention",
                json!([AUTHOR_AP_ID]),
                json!([]),
                Visibility::Direct,
            ),
            ("empty lists", json!([]), json!([]), Visibility::Direct),
            ("no lists", Value::Null, Value::Null, Visibility::Direct),
            (
                "public outranks followers",
                json!([PUBLIC, FOLLOWERS]),
                json!([]),
                Visibility::Public,
            ),
            (
                "a foreign followers list is not private",
                json!(["https://other.example/users/x/followers"]),
                json!([]),
                Visibility::Direct,
            ),
        ]
    }

    #[test]
    fn visibility_follows_to_and_cc() {
        for (name, to, cc, expected) in addressing_cases() {
            let mut document = minimal_note();
            if to == Value::Null {
                remove(&mut document, "to");
            } else {
                document["to"] = to;
            }
            if cc == Value::Null {
                remove(&mut document, "cc");
            } else {
                document["cc"] = cc;
            }
            let parsed = parse(&document);
            assert_eq!(parsed.status.visibility, expected, "{name}");
        }
    }

    /// One rejection case: what to call it, how to break a copy of the
    /// Mastodon fixture, and the expected failure.
    type Rejection = (&'static str, fn(&mut Value), fn(&ParseError) -> bool);

    /// Documents built from the Mastodon fixture by breaking exactly one
    /// rule each; none of them may parse.
    fn rejections() -> Vec<Rejection> {
        vec![
            (
                "attributedTo missing",
                |doc| remove(doc, "attributedTo"),
                |err| matches!(err, ParseError::Containment(_)),
            ),
            (
                "attributedTo names someone else",
                |doc| doc["attributedTo"] = json!("https://remote.example/users/carol"),
                |err| matches!(err, ParseError::Containment(_)),
            ),
            (
                "attributedTo is an empty array",
                |doc| doc["attributedTo"] = json!([]),
                |err| matches!(err, ParseError::Containment(_)),
            ),
            (
                "attributedTo is a number",
                |doc| doc["attributedTo"] = json!(7),
                |err| matches!(err, ParseError::Containment(_)),
            ),
            (
                "the object lives on another host",
                |doc| doc["id"] = json!("https://evil.example/users/bob/statuses/1"),
                |err| matches!(err, ParseError::Containment(_)),
            ),
            (
                "no id",
                |doc| remove(doc, "id"),
                |err| matches!(err, ParseError::Invalid(_)),
            ),
            (
                "array id",
                |doc| doc["id"] = json!(["https://remote.example/objects/1"]),
                |err| matches!(err, ParseError::Invalid(_)),
            ),
            (
                "ftp id",
                |doc| doc["id"] = json!("ftp://remote.example/objects/1"),
                |err| matches!(err, ParseError::Invalid(_)),
            ),
            (
                "relative id",
                |doc| doc["id"] = json!("/objects/1"),
                |err| matches!(err, ParseError::Invalid(_)),
            ),
            (
                "a Tombstone",
                |doc| doc["type"] = json!("Tombstone"),
                |err| matches!(err, ParseError::UnsupportedType(_)),
            ),
            (
                "a lowercase type",
                |doc| doc["type"] = json!("note"),
                |err| matches!(err, ParseError::UnsupportedType(_)),
            ),
            (
                "an array type",
                |doc| doc["type"] = json!(["Note"]),
                |err| matches!(err, ParseError::UnsupportedType(_)),
            ),
            (
                "no type",
                |doc| remove(doc, "type"),
                |err| matches!(err, ParseError::UnsupportedType(_)),
            ),
        ]
    }

    #[test]
    fn documents_that_are_not_own_posts_are_rejected() {
        for (name, break_it, expected) in rejections() {
            let mut document = fixture(MASTODON);
            break_it(&mut document);
            let error = parse_note(&document, &author(), now()).unwrap_err();
            assert!(
                expected(&error),
                "{name}: rejected with the wrong error: {error}"
            );
        }
    }

    #[test]
    fn every_accepted_object_type_parses() {
        for kind in OBJECT_TYPES {
            let mut document = minimal_note();
            document["type"] = json!(kind);
            if kind == TYPE_QUESTION {
                document["oneOf"] = json!([{"name": "a"}, {"name": "b"}]);
            }
            let parsed = parse(&document);
            assert!(
                parsed.status.content.contains("<p>hello</p>"),
                "{kind} keeps its body as content, got {}",
                parsed.status.content
            );
        }
    }

    #[test]
    fn non_object_documents_are_unsupported() {
        for document in [
            json!("https://x.example/o"),
            json!([]),
            json!(null),
            json!(1),
        ] {
            let error = parse_note(&document, &author(), now()).unwrap_err();
            assert!(
                matches!(error, ParseError::UnsupportedType(_)),
                "a document that is not an object fails UnsupportedType, got {error}"
            );
        }
    }

    #[test]
    fn oversized_documents_are_rejected() {
        let mut document = minimal_note();
        document["content"] = json!("x".repeat(2_500_000));
        let error = parse_note(&document, &author(), now()).unwrap_err();
        assert_eq!(error, ParseError::TooLarge, "2.5 MB goes over the cap");
        document["content"] = json!("x".repeat(2_000_000));
        let kept = parse(&document);
        assert_eq!(
            kept.status.content.chars().count(),
            200_000,
            "just under the cap parses, and only the content caps"
        );
    }

    /// One timestamp case: the `published`/`updated` pair to try.
    fn note_with_times(published: Value, updated: Value) -> Value {
        let mut document = json!({
            "id": "https://remote.example/objects/times",
            "type": "Note",
            "attributedTo": AUTHOR_AP_ID,
        });
        if published != Value::Null {
            document["published"] = published;
        }
        if updated != Value::Null {
            document["updated"] = updated;
        }
        document
    }

    #[test]
    fn future_publish_times_clamp_to_now() {
        let skew = parse_time("2024-03-01T00:04:00Z").unwrap();
        let cases = [
            (
                "missing published is now",
                note_with_times(Value::Null, Value::Null),
                now(),
            ),
            (
                "within the skew survives",
                note_with_times(json!("2024-03-01T00:04:00Z"), Value::Null),
                skew,
            ),
            (
                "beyond the skew clamps",
                note_with_times(json!("2024-03-01T00:10:00Z"), Value::Null),
                now(),
            ),
            (
                "a broken stamp is now",
                note_with_times(json!("yesterday"), Value::Null),
                now(),
            ),
            (
                "the past is kept",
                note_with_times(json!("2024-02-29T23:00:00Z"), Value::Null),
                parse_time("2024-02-29T23:00:00Z").unwrap(),
            ),
        ];
        for (name, document, expected) in cases {
            assert_eq!(
                parse(&document).status.inserted_at,
                Some(expected),
                "{name}"
            );
        }
    }

    #[test]
    fn only_a_later_updated_edits_the_post() {
        let published = "2024-02-28T12:00:00Z";
        let edited = parse(&note_with_times(
            json!(published),
            json!("2024-02-28T12:30:00Z"),
        ));
        assert_eq!(
            edited.status.edited_at,
            Some(parse_time("2024-02-28T12:30:00Z").unwrap()),
            "a later updated is an edit"
        );
        let stale = parse(&note_with_times(json!(published), json!(published)));
        assert_eq!(stale.status.edited_at, None, "an equal updated is not");
        let broken = parse(&note_with_times(json!(published), json!("soon")));
        assert_eq!(
            broken.status.edited_at, None,
            "an unparseable updated is dropped"
        );
    }

    #[test]
    fn content_and_spoiler_are_capped() {
        let mut document = minimal_note();
        document["content"] = json!("<p>a</p>".repeat(50_000));
        document["summary"] = json!("w".repeat(1200));
        let parsed = parse(&document);
        assert_eq!(
            parsed.status.content.chars().count(),
            200_000,
            "content caps at 200k chars"
        );
        assert_eq!(
            parsed.status.spoiler_text.chars().count(),
            1000,
            "spoiler caps at 1k chars"
        );
    }

    #[test]
    fn a_sourceless_note_strips_its_html() {
        let mut document = minimal_note();
        document["content"] = json!("<p>one</p><p>two</p>");
        let parsed = parse(&document);
        assert_eq!(
            parsed.status.raw_text, "one\ntwo\n",
            "strip_html of the content"
        );
        assert_eq!(parsed.status.content_type, "text/html", "the html default");
        remove(&mut document, "content");
        let empty = parse(&document);
        assert_eq!(
            empty.status.content, "",
            "no content and no contentMap is empty"
        );
        assert_eq!(empty.status.raw_text, "", "and so is its text");
    }

    /// One `sensitive` case: the field value to try and the outcome.
    fn note_with_sensitive(value: Option<Value>) -> Value {
        let mut document = minimal_note();
        document["summary"] = json!("cw");
        if let Some(field) = value {
            document["sensitive"] = field;
        } else {
            remove(&mut document, "sensitive");
        }
        document
    }

    #[test]
    fn sensitive_follows_the_field_then_the_summary() {
        let cases = [
            (
                "the field says true",
                note_with_sensitive(Some(json!(true))),
                true,
            ),
            (
                "the field says false",
                note_with_sensitive(Some(json!(false))),
                false,
            ),
            (
                "a string field is no field",
                note_with_sensitive(Some(json!("yes"))),
                true,
            ),
            ("no field, a summary", note_with_sensitive(None), true),
        ];
        for (name, document, expected) in cases {
            assert_eq!(parse(&document).status.sensitive, expected, "{name}");
        }
        let mut document = minimal_note();
        remove(&mut document, "summary");
        assert!(!parse(&document).status.sensitive, "no field, no summary");
    }

    #[test]
    fn the_tag_list_caps_at_one_hundred() {
        let mut document = minimal_note();
        document["tag"] = json!(
            (0..120)
                .map(|n| json!({"type": "Mention", "href": format!("{AUTHOR_AP_ID}?n={n}")}))
                .collect::<Vec<Value>>()
        );
        let parsed = parse(&document);
        assert_eq!(
            parsed.mention_hrefs.len(),
            100,
            "only the first hundred tags"
        );
    }

    #[test]
    fn hashtag_names_lose_their_hash_and_their_case() {
        let mut document = minimal_note();
        document["tag"] = json!([
            {"type": "Hashtag", "name": "#RuSt"},
            {"type": "Hashtag", "name": "plain"},
            {"type": "Hashtag", "name": "#"},
            {"type": "Hashtag", "name": "#tada"},
        ]);
        let parsed = parse(&document);
        assert_eq!(
            parsed.hashtags,
            ["rust", "plain", "tada"],
            "hash dropped, lowercased; a bare hash is no tag"
        );
    }

    /// An attachment entry with only a type and url.
    fn attachment_entry(kind: &str, url: &Value) -> Value {
        json!({"type": kind, "url": url})
    }

    #[test]
    fn attachment_shapes_are_the_loose_ones() {
        let mut document = minimal_note();
        document["attachment"] = json!([
            attachment_entry("Document", &json!(["https://x.example/a.png", "junk"])),
            attachment_entry("Image", &json!({"href": "https://x.example/b.GIF"})),
            attachment_entry("Document", &json!({"url": "https://x.example/c.webp"})),
            attachment_entry("Document", &json!("https://x.example/no-dot")),
            attachment_entry("Document", &json!("mailto:whomever@example.org")),
            attachment_entry("PropertyValue", &json!("https://x.example/d.png")),
            {"type": "Document"},
            "a bare string",
        ]);
        let parsed = parse(&document);
        assert_eq!(
            parsed
                .attachments
                .iter()
                .map(|kept| (kept.url.as_str(), kept.content_type.as_str()))
                .collect::<Vec<(&str, &str)>>(),
            [
                ("https://x.example/a.png", "image/png"),
                ("https://x.example/b.GIF", "image/gif"),
                ("https://x.example/c.webp", "image/webp"),
            ],
            "arrays and Link objects resolve; unknown extensions, non-http \
             urls, foreign types and non-objects drop out; case is folded"
        );
    }

    #[test]
    fn only_twenty_attachments_survive() {
        let mut document = minimal_note();
        document["attachment"] = json!(
            (0..25)
                .map(|n| {
                    attachment_entry("Document", &json!(format!("https://x.example/f{n}.png")))
                })
                .collect::<Vec<Value>>()
        );
        assert_eq!(parse(&document).attachments.len(), 20, "the cap is twenty");
    }

    /// A `Question` note carrying the given option list under `field`.
    fn question_with(field: &str, options: Value) -> Value {
        let mut document = minimal_note();
        document["type"] = json!("Question");
        document[field] = options;
        document
    }

    #[test]
    fn questions_need_two_to_twenty_options() {
        let one = json!([{"name": "only"}]);
        let twenty_one = json!(
            (0..21)
                .map(|n| json!({"name": format!("o{n}")}))
                .collect::<Vec<Value>>()
        );
        for (name, document) in [
            ("no option list at all", minimal_note_with_question_type()),
            ("one option", question_with("oneOf", one)),
            ("twenty-one options", question_with("oneOf", twenty_one)),
            ("an empty list", question_with("anyOf", json!([]))),
            ("a null list", question_with("anyOf", Value::Null)),
        ] {
            let error = parse_note(&document, &author(), now()).unwrap_err();
            assert!(
                matches!(error, ParseError::InvalidPoll(_)),
                "{name}: a bad question fails InvalidPoll, got {error}"
            );
        }
        let two = json!([{"name": "a"}, {"name": "b"}]);
        let ok = parse(&question_with("anyOf", two));
        assert_eq!(
            ok.poll
                .expect("two options is the minimum, accepted")
                .options
                .len(),
            2,
            "exactly two options parses"
        );
    }

    /// A public note with the `Question` type and no option lists.
    fn minimal_note_with_question_type() -> Value {
        let mut document = minimal_note();
        document["type"] = json!("Question");
        document
    }

    #[test]
    fn a_non_question_ignores_its_option_lists() {
        let mut document = minimal_note();
        document["oneOf"] = json!([{"name": "stray"}]);
        let parsed = parse(&document);
        assert!(parsed.poll.is_none(), "a Note is never a poll");
    }

    #[test]
    fn a_closed_question_expires_at_its_closed_stamp() {
        let mut document = question_with("oneOf", json!([{"name": "a"}, {"name": "b"}]));
        document["closed"] = json!("2024-02-09T00:00:00Z");
        let poll = parse(&document).poll.expect("the poll parses");
        assert_eq!(
            poll.expires_at,
            Some(parse_time("2024-02-09T00:00:00Z").unwrap()),
            "a timestamp closed names the end"
        );
        assert_eq!(poll.voters_count, None, "no votersCount, no count");
        assert_eq!(
            poll.options[0].votes_count, 0,
            "no replies block is zero votes"
        );
    }

    #[test]
    fn references_read_their_first_usable_field() {
        let mut document = minimal_note();
        document["quoteUri"] = json!({"id": "https://x.example/q1"});
        document["context"] = Value::Null;
        document["conversation"] = json!("https://x.example/thread/9");
        let parsed = parse(&document);
        assert_eq!(
            parsed.quote_uri.as_deref(),
            Some("https://x.example/q1"),
            "quoteUri reads an object id"
        );
        assert_eq!(
            parsed.ap_context.as_deref(),
            Some("https://x.example/thread/9"),
            "a null context falls through to conversation"
        );
    }

    #[test]
    fn parse_errors_render_their_causes() {
        assert_eq!(
            ParseError::Containment("bad host".to_owned()).to_string(),
            "note fails containment: bad host"
        );
        assert_eq!(
            ParseError::UnsupportedType("Tombstone".to_owned()).to_string(),
            "unsupported object type: Tombstone"
        );
        assert_eq!(
            ParseError::Invalid("no id".to_owned()).to_string(),
            "invalid note object: no id"
        );
        assert_eq!(
            ParseError::InvalidPoll("one option".to_owned()).to_string(),
            "invalid poll: one option"
        );
        assert_eq!(
            ParseError::TooLarge.to_string(),
            "the note document exceeds the size cap"
        );
        assert!(
            Error::source(&ParseError::TooLarge).is_none(),
            "a plain parse error has no source"
        );
    }
}
