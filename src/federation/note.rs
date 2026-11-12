//! Outbound `ActivityPub` documents for local statuses: the `Note` (or
//! `Question`) object and the activities that carry it.
//!
//! [`note_json`] is the object renderer — the counterpart of Pleroma's
//! `ObjectView.render("object.json", …)` merged over the stored object data
//! by `Transmogrifier.prepare_object/1` (`lib/pleroma/web/activity_pub/views/object_view.ex`,
//! `lib/pleroma/web/activity_pub/transmogrifier.ex`), with the field set of
//! the `ArticleNotePageValidator`/`QuestionValidator` family
//! (`lib/pleroma/web/activity_pub/object_validators/`). Like Pleroma's it is
//! a pure document build: every input rides in on [`NoteContext`] and the
//! function returns a [`serde_json::Value`]; [`load_note_context`] is the
//! separate async reader that fills that context from the database.
//!
//! [`create_activity_json`] wraps an object in its `Create`,
//! [`announce_activity_json`] renders a reblog row as the `Announce` it
//! federates as (decision 6: the reblog *is* the activity, and its stored
//! `uri` is the activity id), and [`tombstone_json`] is what a deleted
//! status becomes. [`litepub_context_json`] is the JSON-LD schema document
//! every one of these references from its `@context`, served at
//! [`LITEPUB_SCHEMA_PATH`] by [`crate::api::activitypub`].
//!
//! Deliberate deltas from Pleroma, fixed by the ticket: `likes`, `replies`,
//! `shares` and the `emoji` map are not written into the object (readers
//! discover them through their own collections), the internal `context_id`
//! stays internal, and a `local`-visibility status is never federated at
//! all — [`note_json`] and [`announce_activity_json`] answer `None` for it.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    models::{
        custom_emoji::{self, CustomEmoji},
        hashtag,
        media_attachment::{self, MediaAttachment},
        mention, poll,
        status::{self, Status, Visibility},
        user::{self, User, followers},
    },
    util::time::mastodon_time,
};

/// Path of the JSON-LD context document every document built here
/// references from its `@context`, relative to the instance origin — the
/// address Pleroma serves its `priv/static/schemas/litepub-0.1.jsonld` at.
pub const LITEPUB_SCHEMA_PATH: &str = "/schemas/litepub-0.1.jsonld";
/// The `ActivityStreams` public addressing collection, the one recipient
/// that means "everyone".
const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The `ActivityStreams` core vocabulary, first entry of every `@context`.
const AS_VOCABULARY: &str = "https://www.w3.org/ns/activitystreams";
/// The object type of a plain status.
const TYPE_NOTE: &str = "Note";
/// The object type of a status carrying a poll.
const TYPE_QUESTION: &str = "Question";
/// The type of a `Create` activity.
const TYPE_CREATE: &str = "Create";
/// The type of a reblog's activity.
const TYPE_ANNOUNCE: &str = "Announce";
/// The type of a deleted status's replacement document.
const TYPE_TOMBSTONE: &str = "Tombstone";
/// The tag type of an @-mention.
const TYPE_MENTION: &str = "Mention";
/// The tag type of a hashtag.
const TYPE_HASHTAG: &str = "Hashtag";
/// The tag type of a custom emoji used in the content.
const TYPE_EMOJI: &str = "Emoji";
/// The type of an `icon` value and of image-class attachments.
const TYPE_IMAGE: &str = "Image";
/// The type of a non-image attachment.
const TYPE_DOCUMENT: &str = "Document";
/// The type of an option's vote collection.
const TYPE_COLLECTION: &str = "Collection";
/// What Pleroma writes as the object language in the `@context` language
/// map of a document that declares none: `und`etermined.
const LANGUAGE_UNDETERMINED: &str = "und";

/// Everything [`note_json`] renders one status's document from: the row,
/// its author, and the satellites the addressing and the object body read,
/// all loaded up front by [`load_note_context`].
///
/// The struct is data-only by design (decision 3): the builder never asks
/// the database, so a test hands one of these to [`note_json`] and compares
/// the JSON, and the async half lives entirely in the loader.
#[derive(Debug)]
pub struct NoteContext {
    /// The status being rendered; its `uri` becomes the object id.
    pub status: Status,
    /// The author; their actor id and followers collection drive the
    /// addressing.
    pub author: User,
    /// The answered status's object id, resolved by
    /// [`load_note_context`] from [`Status::in_reply_to_id`] or the still
    /// unresolved [`Status::in_reply_to_ap_id`]; `None` for a root status.
    pub reply_parent_uri: Option<String>,
    /// The mentioned accounts, in mention order, with the replied-to
    /// author appended by [`load_note_context`] when a reply does not
    /// already name them — the fediverse requires an answer to reach its
    /// parent's author.
    pub mentions: Vec<User>,
    /// The status's hashtags, as stored names (lowercase, no `#`).
    pub hashtags: Vec<String>,
    /// The media attachments, in position order.
    pub attachments: Vec<MediaAttachment>,
    /// The local custom-emoji rows this instance has for the shortcodes
    /// the content carries; [`note_json`] re-checks each against the text
    /// and tags only the ones it finds.
    pub emojis: Vec<CustomEmoji>,
    /// The poll when the status asks a question; its presence turns the
    /// object into a `Question`.
    pub poll: Option<PollData>,
    /// The quoted status's object id; `None` for anything but a quote.
    pub quote_uri: Option<String>,
    /// The instance whose origin, schema route and language default the
    /// document is built from.
    pub config: Config,
}

/// The poll [`note_json`] renders into a `Question`: the row facts plus
/// its options and whether voting has already run out — the clock question
/// the loader answers so the builder stays pure.
#[derive(Clone, Debug)]
pub struct PollData {
    /// Whether a voter may mark more than one option (`anyOf` instead of
    /// `oneOf`).
    pub multiple: bool,
    /// When voting closes; `None` for a poll that never expires.
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether voting has closed — [`expires_at`] in the past at load
    /// time. Pleroma writes `closed` only then.
    ///
    /// [`expires_at`]: Self::expires_at
    pub closed: bool,
    /// Distinct voters; the `Question`'s `votersCount`.
    pub voters_count: i32,
    /// The options, in position order, each with its tally.
    pub options: Vec<PollOptionData>,
}

/// One poll option as the `Question` renders it: its title and votes.
#[derive(Clone, Debug)]
pub struct PollOptionData {
    /// The option text as its author wrote it.
    pub title: String,
    /// Votes this option has; its `replies.totalItems`.
    pub votes_count: i32,
}

/// The `Note`/`Question` object of the status in `ctx`, as this instance
/// federates and serves it.
///
/// `None` for a [`Visibility::Local`] status: that visibility exists to
/// keep a post on this instance, and the one thing it must never become is
/// an `ActivityPub` document.
#[must_use]
pub fn note_json(ctx: &NoteContext) -> Option<Value> {
    if ctx.status.visibility == Visibility::Local {
        return None;
    }
    let mut fields = note_fields(ctx);
    if let Some(edited_at) = ctx.status.edited_at {
        fields.push(("updated", json!(mastodon_time(&edited_at))));
    }
    if let Some(poll) = &ctx.poll {
        fields.extend(poll_fields(poll));
    }
    if let Some(quote_uri) = &ctx.quote_uri {
        fields.push(("quoteUrl", json!(quote_uri)));
        fields.push(("quoteUri", json!(quote_uri)));
    }
    Some(Value::Object(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    ))
}

/// The fields every visibility shares, in Pleroma's render order; the
/// optional ones ([`note_json`] adds them: `updated`, the poll's `oneOf`
/// and friends, the quote references).
fn note_fields(ctx: &NoteContext) -> Vec<(&'static str, Value)> {
    let context_uri = format!(
        "{}/contexts/{}",
        ctx.config.base_url, ctx.status.conversation_id
    );
    let (to, cc) = addressing(ctx);
    vec![
        ("@context", json_ld_context(&ctx.config)),
        ("id", json!(&ctx.status.uri)),
        ("type", json!(object_type(ctx))),
        ("attributedTo", json!(&ctx.author.ap_id)),
        ("actor", json!(&ctx.author.ap_id)),
        ("to", Value::Array(to)),
        ("cc", Value::Array(cc)),
        ("content", json!(&ctx.status.content)),
        ("source", source_json(&ctx.status)),
        ("summary", summary_json(&ctx.status)),
        ("sensitive", json!(ctx.status.sensitive)),
        ("published", json!(mastodon_time(&ctx.status.inserted_at))),
        ("inReplyTo", json!(&ctx.reply_parent_uri)),
        ("url", json!(status_url(&ctx.status))),
        ("context", json!(&context_uri)),
        ("conversation", json!(&context_uri)),
        ("tag", Value::Array(tags_json(ctx))),
        (
            "attachment",
            Value::Array(attachments_json(&ctx.attachments)),
        ),
    ]
}

/// `Question` when the status carries a poll, `Note` otherwise.
const fn object_type(ctx: &NoteContext) -> &'static str {
    if ctx.poll.is_some() {
        TYPE_QUESTION
    } else {
        TYPE_NOTE
    }
}

/// The `source` object: the post's own text as written, in the content
/// type it was written in (Pleroma stores the same pair inside the object).
fn source_json(status: &Status) -> Value {
    json!({
        "content": &status.raw_text,
        "mediaType": &status.content_type,
    })
}

/// The `summary` — the content warning line, JSON `null` when the status
/// carries none.
fn summary_json(status: &Status) -> Value {
    if status.spoiler_text.is_empty() {
        Value::Null
    } else {
        json!(&status.spoiler_text)
    }
}

/// The human-facing URL of the post: the stored `{BASE_URL}/notice/{id}`
/// of a local status, falling back to the object id when a row names no
/// page.
fn status_url(status: &Status) -> &str {
    status.url.as_deref().unwrap_or(status.uri.as_str())
}

/// The `to` and `cc` arrays `ctx`'s visibility addresses: `public` talks
/// to the world and shows the post in the author's followers' timelines,
/// `unlisted` inverts the pair, `private` reaches only followers and
/// mentionees, `direct` only mentionees — and `local` addresses nothing,
/// which [`note_json`] has already refused to render. Mentioned accounts
/// dedupe by actor id, keeping first-mention order.
fn addressing(ctx: &NoteContext) -> (Vec<Value>, Vec<Value>) {
    let followers_addr = json!(followers(&ctx.author.ap_id));
    let mentioned = mention_addresses(&ctx.mentions);
    match ctx.status.visibility {
        Visibility::Public => (vec![json!(AS_PUBLIC)], prepend(followers_addr, mentioned)),
        Visibility::Unlisted => (vec![followers_addr], prepend(json!(AS_PUBLIC), mentioned)),
        Visibility::Private => (prepend(followers_addr, mentioned), Vec::new()),
        Visibility::Direct => (mentioned, Vec::new()),
        Visibility::Local => (Vec::new(), Vec::new()),
    }
}

/// `first` followed by `rest`, as one freshly owned vector.
fn prepend(first: Value, rest: Vec<Value>) -> Vec<Value> {
    let mut addresses = Vec::with_capacity(rest.len() + 1);
    addresses.push(first);
    addresses.extend(rest);
    addresses
}

/// The mentioned actors' ids as addressing entries, deduped by actor id in
/// first-mention order.
fn mention_addresses(mentions: &[User]) -> Vec<Value> {
    let mut seen: HashSet<&str> = HashSet::new();
    mentions
        .iter()
        .filter(|person| seen.insert(person.ap_id.as_str()))
        .map(|person| json!(&person.ap_id))
        .collect()
}

/// The `tag` array: hashtags first, then mentions, then the custom emojis
/// the content actually uses — Pleroma's `prepare_object` order
/// (`add_hashtags`, `add_mention_tags`, `add_emoji_tags`).
fn tags_json(ctx: &NoteContext) -> Vec<Value> {
    let base = ctx.config.base_url.as_str();
    let mut tags: Vec<Value> = ctx
        .hashtags
        .iter()
        .map(|name| hashtag_tag(base, name))
        .collect();
    tags.extend(ctx.mentions.iter().map(mention_tag));
    tags.extend(used_emoji_tags(ctx));
    tags
}

/// One hashtag as its tag object, pointing at this instance's tag page.
fn hashtag_tag(base: &str, name: &str) -> Value {
    json!({
        "type": TYPE_HASHTAG,
        "href": format!("{base}/tags/{name}"),
        "name": format!("#{name}"),
    })
}

/// One mentioned account as its tag object: `@nick` for a local account,
/// `@nick@domain` for a remote one, which is exactly how the nickname is
/// stored (decision 7).
fn mention_tag(person: &User) -> Value {
    json!({
        "type": TYPE_MENTION,
        "href": &person.ap_id,
        "name": format!("@{}", person.nickname),
    })
}

/// The emoji tags for the `ctx`'s known local emojis whose `:shortcode:`
/// appears in the post's text (source or rendered HTML).
fn used_emoji_tags(ctx: &NoteContext) -> Vec<Value> {
    ctx.emojis
        .iter()
        .filter(|emoji| content_uses(&ctx.status, &emoji.shortcode))
        .map(emoji_tag)
        .collect()
}

/// Whether either text of `status` carries `:shortcode:` verbatim.
fn content_uses(status: &Status, shortcode: &str) -> bool {
    let needle = format!(":{shortcode}:");
    status.raw_text.contains(&needle) || status.content.contains(&needle)
}

/// One custom emoji as its tag object: the image URL as both `id` and
/// `icon.url`, the colon-wrapped shortcode as the `name`.
fn emoji_tag(emoji: &CustomEmoji) -> Value {
    json!({
        "type": TYPE_EMOJI,
        "id": &emoji.image_url,
        "name": format!(":{}:", emoji.shortcode),
        "icon": { "type": TYPE_IMAGE, "url": &emoji.image_url },
    })
}

/// The `attachment` array: images federate as `Image` objects, everything
/// else as `Document` (the media class follows the stored `MIME` type, as
/// [`crate::entities::media_attachment`] classes it for the client API).
fn attachments_json(attachments: &[MediaAttachment]) -> Vec<Value> {
    attachments.iter().map(attachment_json).collect()
}

/// One media row as its attachment object.
fn attachment_json(media: &MediaAttachment) -> Value {
    let class = if media.content_type.starts_with("image/") {
        TYPE_IMAGE
    } else {
        TYPE_DOCUMENT
    };
    json!({
        "type": class,
        "mediaType": &media.content_type,
        "url": &media.url,
        "name": media.description.clone().unwrap_or_default(),
        "blurhash": &media.blurhash,
    })
}

/// The poll fields of a `Question`: the choices under `oneOf` (single
/// choice) or `anyOf` (multiple), the `votersCount`, and the close pair —
/// `endTime` always names the deadline, `closed` repeats it only once
/// voting has run out (Pleroma's `QuestionValidator` drops a `closed` it
/// cannot ground in an end time).
fn poll_fields(poll_data: &PollData) -> Vec<(&'static str, Value)> {
    let choices = if poll_data.multiple { "anyOf" } else { "oneOf" };
    let mut fields = vec![
        (
            choices,
            Value::Array(poll_data.options.iter().map(poll_option_json).collect()),
        ),
        ("votersCount", json!(poll_data.voters_count)),
    ];
    if let Some(expires_at) = poll_data.expires_at {
        let end_time = json!(mastodon_time(&expires_at));
        fields.push(("endTime", end_time.clone()));
        if poll_data.closed {
            fields.push(("closed", end_time));
        }
    }
    fields
}

/// One poll option as the `Note` answer-object Pleroma federates it as,
/// with its vote tally as a `Collection`'s `totalItems`.
fn poll_option_json(option: &PollOptionData) -> Value {
    json!({
        "type": TYPE_NOTE,
        "name": &option.title,
        "replies": { "type": TYPE_COLLECTION, "totalItems": option.votes_count },
    })
}

/// The `@context` of every document this module and
/// [`activities`](super::activities) build: the `ActivityStreams`
/// vocabulary, this instance's [`litepub_context_json`] schema, and the
/// `und` language map Pleroma's `make_json_ld_header` writes for a
/// document that declares no language.
pub(crate) fn json_ld_context(config: &Config) -> Value {
    json!([
        AS_VOCABULARY,
        format!("{}{LITEPUB_SCHEMA_PATH}", config.base_url),
        { "@language": LANGUAGE_UNDETERMINED },
    ])
}

/// The JSON-LD context document served at [`LITEPUB_SCHEMA_PATH`].
///
/// The prefixes and term definitions this instance's own documents use: the
/// counterpart of Pleroma's `priv/static/schemas/litepub-0.1.jsonld`, cut
/// down to what [`note_json`] and the activity builders actually write.
#[must_use]
pub fn litepub_context_json() -> Value {
    json!({
        "@context": {
            "as": "https://www.w3.org/ns/activitystreams#",
            "fedibird": "http://fedibird.com/ns#",
            "litepub": "http://litepub.social/ns#",
            "ostatus": "http://ostatus.org#",
            "toot": "http://joinmastodon.org/ns#",
            "Hashtag": "as:Hashtag",
            "Emoji": "toot:Emoji",
            "EmojiReact": "litepub:EmojiReact",
            "capabilities": "litepub:capabilities",
            "conversation": { "@id": "ostatus:conversation", "@type": "@id" },
            "directMessage": "litepub:directMessage",
            "quoteUri": "fedibird:quoteUri",
            "quoteUrl": "as:quoteUrl",
            "sensitive": "as:sensitive",
            "votersCount": "toot:votersCount",
        }
    })
}

/// The `Create` activity carrying `note` for `status`, whose `UUIDv7` also
/// names the activity: `{BASE_URL}/activities/{id}`, the address a later
/// ticket will resolve against the status row (decision 6).
///
/// The addressing mirrors the object's — Pleroma's `make_create_data`
/// copies `to`/`cc` from the object, and the actor is the object's
/// `attributedTo` — so the two documents can never disagree.
#[must_use]
pub fn create_activity_json(note: &Value, status: &Status, config: &Config) -> Value {
    json!({
        "@context": json_ld_context(config),
        "id": format!("{}/activities/{}", config.base_url, status.id),
        "type": TYPE_CREATE,
        "actor": note.get("actor").cloned().unwrap_or(Value::Null),
        "to": note.get("to").cloned().unwrap_or_else(|| json!([])),
        "cc": note.get("cc").cloned().unwrap_or_else(|| json!([])),
        "object": note,
        "published": mastodon_time(&status.inserted_at),
    })
}

/// The `Announce` a reblog row federates as.
///
/// Its own stored `uri` is already the `{BASE_URL}/activities/{id}` address
/// (decision 6), so this is that row spoken as an activity of `author`
/// boosting `original_uri` by `original_author_ap_id`.
///
/// The addressing is Pleroma's `make_announce_data`: a public or unlisted
/// boost goes to the booster's followers and the boosted author with the
/// public collection in `cc`, a private or direct one only to the
/// followers — and a `local` reblog is never announced at all (`None`).
#[must_use]
pub fn announce_activity_json(
    reblog: &Status,
    author: &User,
    original_uri: &str,
    original_author_ap_id: &str,
    config: &Config,
) -> Option<Value> {
    let followers_addr = json!(followers(&author.ap_id));
    let (to, cc) = match reblog.visibility {
        Visibility::Public | Visibility::Unlisted => (
            json!([followers_addr, original_author_ap_id]),
            json!([AS_PUBLIC]),
        ),
        Visibility::Private | Visibility::Direct => (json!([followers_addr]), json!([])),
        Visibility::Local => return None,
    };
    Some(json!({
        "@context": json_ld_context(config),
        "id": &reblog.uri,
        "type": TYPE_ANNOUNCE,
        "actor": &author.ap_id,
        "object": original_uri,
        "to": to,
        "cc": cc,
        "published": mastodon_time(&reblog.inserted_at),
    }))
}

/// The `Tombstone` a soft-deleted status is served as.
///
/// It carries the reserved object id, the type the post had lived as, and
/// the moment it went. A row that never recorded a deletion reports its
/// last write instead — callers that mean to tombstone a live status still
/// get a `deleted` they can sign.
#[must_use]
pub fn tombstone_json(status: &Status) -> Value {
    let deleted = status.deleted_at.unwrap_or(status.updated_at);
    json!({
        "id": &status.uri,
        "type": TYPE_TOMBSTONE,
        "formerType": TYPE_NOTE,
        "deleted": mastodon_time(&deleted),
    })
}

/// Reads everything [`note_json`] needs for status `status_id`.
///
/// The row and its author, the resolved parent uri, the mentions with the
/// replied-to author appended when the post does not already name them, the
/// hashtags, the media, the poll with its options and close state, the
/// quote, and the local custom-emoji rows for the shortcodes the text uses.
///
/// A tombstoned or unknown id is [`AppError::NotFound`]: this loads the
/// documents a live status federates as; the `Delete` side has its own
/// builder ([`tombstone_json`]) and reads the row it already holds.
///
/// # Errors
///
/// [`AppError::NotFound`] when no live status (or no author row behind it)
/// carries `status_id`; any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn load_note_context(
    pool: &PgPool,
    config: &Config,
    status_id: Uuid,
) -> AppResult<NoteContext> {
    let status = status::get_by_id(pool, status_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let author = user::get_by_id(pool, status.user_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let reply_parent_uri = parent_uri(pool, &status).await?;
    let mentions = load_mentions(pool, &status).await?;
    let hashtags = load_hashtags(pool, &status).await?;
    let attachments = load_attachments(pool, &status).await?;
    let emojis = load_emojis(pool, &status).await?;
    let poll_data = load_poll(pool, &status).await?;
    let quote_uri = load_quote_uri(pool, &status).await?;
    Ok(NoteContext {
        status,
        author,
        reply_parent_uri,
        mentions,
        hashtags,
        attachments,
        emojis,
        poll: poll_data,
        quote_uri,
        config: config.clone(),
    })
}

/// The answered status's object id: the stored parent's `uri` (a parent
/// deleted after the answer still owns the address), else the
/// [`Status::in_reply_to_ap_id`] the dispatcher parked for a parent this
/// instance has not stored.
async fn parent_uri(pool: &PgPool, status: &Status) -> AppResult<Option<String>> {
    if let Some(parent_id) = status.in_reply_to_id {
        let parent = status::get_by_id_including_deleted(pool, parent_id).await?;
        if let Some(row) = parent {
            return Ok(Some(row.uri));
        }
    }
    Ok(status.in_reply_to_ap_id.clone())
}

/// The status's mentions in link order, with the replied-to author
/// appended when the post answers them without naming them — Pleroma
/// addresses a reply's parent author the same way, and the addressing
/// rules of [`note_json`] read it off this list.
async fn load_mentions(pool: &PgPool, status: &Status) -> AppResult<Vec<User>> {
    let mut mentions = mention::for_statuses(pool, &[status.id])
        .await?
        .remove(&status.id)
        .unwrap_or_default();
    if let Some(parent_author_id) = status.in_reply_to_account_id
        && !mentions.iter().any(|person| person.id == parent_author_id)
        && let Some(parent_author) = user::get_by_id(pool, parent_author_id).await?
    {
        mentions.push(parent_author);
    }
    Ok(mentions)
}

/// The status's hashtag names, in the link read's order.
async fn load_hashtags(pool: &PgPool, status: &Status) -> AppResult<Vec<String>> {
    let tags = hashtag::for_statuses(pool, &[status.id])
        .await?
        .remove(&status.id)
        .unwrap_or_default();
    Ok(tags.into_iter().map(|tag| tag.name).collect())
}

/// The status's media, in position order.
async fn load_attachments(pool: &PgPool, status: &Status) -> AppResult<Vec<MediaAttachment>> {
    Ok(media_attachment::for_statuses(pool, &[status.id])
        .await?
        .remove(&status.id)
        .unwrap_or_default())
}

/// The enabled local emoji rows for every `:shortcode:` the status's text
/// carries — in shortcode order (the table read's), which is all the tag
/// list needs since the builder re-checks each code against the content.
async fn load_emojis(pool: &PgPool, status: &Status) -> AppResult<Vec<CustomEmoji>> {
    let codes = used_shortcodes(status);
    if codes.is_empty() {
        return Ok(Vec::new());
    }
    let rows = custom_emoji::get_by_shortcodes(pool, &codes, None).await?;
    Ok(rows.into_iter().filter(|emoji| !emoji.disabled).collect())
}

/// The poll of `status` as [`PollData`], `None` when it carries none. The
/// close state is read here, against the load-time clock: by the moment
/// the document federates, a poll whose deadline has run out is closed.
async fn load_poll(pool: &PgPool, status: &Status) -> AppResult<Option<PollData>> {
    let Some(row) = poll::get_for_status(pool, status.id).await? else {
        return Ok(None);
    };
    let options = poll::options(pool, row.id).await?;
    Ok(Some(PollData {
        multiple: row.multiple,
        expires_at: row.expires_at,
        closed: row.expires_at.is_some_and(|at| at <= Utc::now()),
        voters_count: row.voters_count,
        options: options
            .iter()
            .map(|option| PollOptionData {
                title: option.title.clone(),
                votes_count: option.votes_count,
            })
            .collect(),
    }))
}

/// The quoted status's object id, `None` for anything but a quote; a
/// quoted post that has since been deleted keeps its reserved uri.
async fn load_quote_uri(pool: &PgPool, status: &Status) -> AppResult<Option<String>> {
    match status.quote_of_id {
        Some(quote_id) => Ok(status::get_by_id_including_deleted(pool, quote_id)
            .await?
            .map(|row| row.uri)),
        None => Ok(None),
    }
}

/// The distinct `:shortcode:` texts the status's raw text and rendered
/// HTML carry, in order of first appearance.
fn used_shortcodes(status: &Status) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut codes: Vec<String> = Vec::new();
    for text in [&status.raw_text, &status.content] {
        for code in shortcodes_in(text) {
            if seen.insert(code.clone()) {
                codes.push(code);
            }
        }
    }
    codes
}

/// Every `:shortcode:` reference in `text`: a colon, one or more
/// `[A-Za-z0-9_]`, a colon — the shape [`crate::util::emoji::shortcode`]
/// recognises, scanned out of running prose. A colon after an incomplete
/// candidate starts a new one (so `:a::b:` names both), and a candidate
/// that never closes is dropped.
fn shortcodes_in(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut candidate: Option<String> = None;
    for symbol in text.chars() {
        match symbol {
            ':' => {
                if let Some(code) = candidate.take()
                    && !code.is_empty()
                {
                    found.push(code);
                }
                candidate = Some(String::new());
            }
            other if other.is_ascii_alphanumeric() || other == '_' => {
                if let Some(code) = candidate.as_mut() {
                    code.push(other);
                }
            }
            _ => candidate = None,
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use chrono::{Duration as TimeDelta, Utc};
    use sqlx::PgPool;

    use super::*;
    use crate::{
        models::{
            custom_emoji::{NewCustomEmoji, upsert as upsert_emoji},
            media_attachment::{NewMediaAttachment, insert as insert_media},
            poll::{PollParams, create as create_poll},
            status::{NewStatus, insert as insert_status, soft_delete},
            user::{Flag, NewUser, insert as insert_user, local_ap_id},
        },
        util::{
            id::new_id,
            time::{mastodon_time, parse_time},
        },
    };

    /// The instance origin of [`Config::for_tests`].
    const BASE: &str = "https://test.example";
    /// The schema URL every document's `@context` names.
    const SCHEMA_URL: &str = "https://test.example/schemas/litepub-0.1.jsonld";
    /// The author fixture's id, a `UUIDv7` shape spelled out for equality.
    const AUTHOR_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// The author's actor id.
    const AUTHOR_AP: &str = "https://test.example/users/alice";
    /// The status fixture's id.
    const STATUS_ID: &str = "01934568-1111-7000-8000-abcdef012345";
    /// The mentioned remote account's actor id.
    const BOB_AP: &str = "https://remote.example/users/bob";
    /// A parent post's object id on a peer instance.
    const PARENT_URI: &str = "https://remote.example/objects/parent-post";
    /// The quoted post's object id on a peer instance.
    const QUOTE_URI: &str = "https://remote.example/objects/quoted-post";
    /// Every fixture's creation moment, in the wire format.
    const PUBLISHED: &str = "2024-01-31T12:34:56.789Z";
    /// The moment the tombstone fixture reports its deletion.
    const DELETED: &str = "2024-02-01T08:00:00.000Z";
    /// A poll deadline in the far future: the open-poll fixture.
    const FAR_END: &str = "2100-01-01T00:00:00.000Z";

    /// Every field of a rendered document, sorted — the exact key-set
    /// assertion the goldens pair with.
    fn json_keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("a document is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    /// A [`User`] row named `nickname` at `ap_id`, local or not as asked:
    /// the mention and author fixtures.
    fn fixture_user(nickname: &str, ap_id: &str, local: bool) -> User {
        User {
            id: Uuid::parse_str(AUTHOR_ID).unwrap(),
            nickname: nickname.to_owned(),
            ap_id: ap_id.to_owned(),
            local: Flag::new(local),
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
            default_scope: user::DEFAULT_SCOPE.to_owned(),
            sensitive_default: Flag::new(false),
            language: None,
            tags: Vec::new(),
            settings_store: json!({}),
            notification_settings: json!({}),
            mfa_settings: user::mfa::MfaSettings::default_value(),
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
            inserted_at: parse_time(PUBLISHED).unwrap(),
            updated_at: parse_time(PUBLISHED).unwrap(),
            preferred_frontend: None,
            is_suggested: Flag::new(false),
            password_reset_pending: Flag::new(false),
            email_notifications: Flag::new(true),
            deleted_at: None,
        }
    }

    /// The author: local `alice` at [`AUTHOR_AP`].
    fn author() -> User {
        fixture_user("alice", AUTHOR_AP, true)
    }

    /// A mentioned remote account named `bob`.
    fn mentioned_bob() -> User {
        let mut bob = fixture_user("bob@remote.example", BOB_AP, false);
        bob.domain = Some("remote.example".to_owned());
        bob
    }

    /// The status fixture: a local `public` post of "hello" whose id, uri,
    /// url and conversation are all the constants above.
    fn base_status(visibility: Visibility) -> Status {
        Status {
            id: Uuid::parse_str(STATUS_ID).unwrap(),
            uri: format!("{BASE}/objects/{STATUS_ID}"),
            url: Some(format!("{BASE}/notice/{STATUS_ID}")),
            user_id: Uuid::parse_str(AUTHOR_ID).unwrap(),
            content: "<p>hello</p>".to_owned(),
            raw_text: "hello".to_owned(),
            content_type: "text/plain".to_owned(),
            spoiler_text: String::new(),
            sensitive: false,
            visibility,
            language: None,
            in_reply_to_id: None,
            in_reply_to_account_id: None,
            in_reply_to_ap_id: None,
            conversation_id: Uuid::parse_str(STATUS_ID).unwrap(),
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
            inserted_at: parse_time(PUBLISHED).unwrap(),
            updated_at: parse_time(PUBLISHED).unwrap(),
        }
    }

    /// A [`NoteContext`] for a bare status of `visibility`: no satellites,
    /// the test instance's config.
    fn note_context(visibility: Visibility) -> NoteContext {
        NoteContext {
            status: base_status(visibility),
            author: author(),
            reply_parent_uri: None,
            mentions: Vec::new(),
            hashtags: Vec::new(),
            attachments: Vec::new(),
            emojis: Vec::new(),
            poll: None,
            quote_uri: None,
            config: Config::for_tests(),
        }
    }

    /// Render the bare note of `visibility`, expecting the refusal of a
    /// `local` one to be tested separately.
    fn render(visibility: Visibility) -> Value {
        let ctx = note_context(visibility);
        note_json(&ctx).expect("a federatable visibility renders")
    }

    /// The `@context` every document of the test instance carries.
    fn expected_context() -> Value {
        json!([
            "https://www.w3.org/ns/activitystreams",
            SCHEMA_URL,
            { "@language": "und" },
        ])
    }

    /// Every key a bare note object carries.
    const NOTE_KEYS: [&str; 18] = [
        "@context",
        "actor",
        "attachment",
        "attributedTo",
        "cc",
        "content",
        "context",
        "conversation",
        "id",
        "inReplyTo",
        "published",
        "sensitive",
        "source",
        "summary",
        "tag",
        "to",
        "type",
        "url",
    ];

    #[test]
    fn a_public_note_renders_the_golden_object() {
        let document = render(Visibility::Public);
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/objects/{STATUS_ID}"),
                "type": "Note",
                "attributedTo": AUTHOR_AP,
                "actor": AUTHOR_AP,
                "to": [AS_PUBLIC],
                "cc": [format!("{AUTHOR_AP}/followers")],
                "content": "<p>hello</p>",
                "source": {"content": "hello", "mediaType": "text/plain"},
                "summary": null,
                "sensitive": false,
                "published": PUBLISHED,
                "inReplyTo": null,
                "url": format!("{BASE}/notice/{STATUS_ID}"),
                "context": format!("{BASE}/contexts/{STATUS_ID}"),
                "conversation": format!("{BASE}/contexts/{STATUS_ID}"),
                "tag": [],
                "attachment": [],
            }),
            "the whole document, field for field"
        );
        assert_eq!(json_keys(&document), NOTE_KEYS, "and nothing else");
    }

    #[test]
    fn an_unlisted_note_moves_the_public_collection_to_cc() {
        let document = render(Visibility::Unlisted);
        assert_eq!(
            document["to"],
            json!([format!("{AUTHOR_AP}/followers")]),
            "an unlisted post is addressed to the followers"
        );
        assert_eq!(
            document["cc"],
            json!([AS_PUBLIC]),
            "and the world only sees it in cc"
        );
    }

    #[test]
    fn a_private_note_reaches_followers_and_mentionees_only() {
        let mut ctx = note_context(Visibility::Private);
        ctx.mentions = vec![mentioned_bob()];
        let document = note_json(&ctx).expect("private federates");
        assert_eq!(
            document["to"],
            json!([format!("{AUTHOR_AP}/followers"), BOB_AP]),
            "followers and the mentionee share the primary line"
        );
        assert_eq!(document["cc"], json!([]), "and nothing is cc-ed");
    }

    #[test]
    fn a_direct_note_addresses_its_mentionees_alone() {
        let mut ctx = note_context(Visibility::Direct);
        ctx.mentions = vec![mentioned_bob()];
        let document = note_json(&ctx).expect("direct federates");
        assert_eq!(document["to"], json!([BOB_AP]), "to is the mentionees");
        assert_eq!(document["cc"], json!([]), "cc stays empty");
        assert!(
            !document["to"]
                .as_array()
                .unwrap()
                .contains(&json!(AS_PUBLIC)),
            "a direct note never addresses the public collection"
        );
    }

    #[test]
    fn a_local_status_is_never_federated() {
        let ctx = note_context(Visibility::Local);
        assert!(
            note_json(&ctx).is_none(),
            "local visibility renders no object"
        );
    }

    #[test]
    fn a_reply_carries_its_parent_and_reaches_its_author() {
        let mut ctx = note_context(Visibility::Public);
        ctx.status.in_reply_to_id = Some(new_id());
        ctx.status.in_reply_to_account_id = Some(Uuid::nil());
        ctx.reply_parent_uri = Some(PARENT_URI.to_owned());
        ctx.mentions = vec![mentioned_bob()];
        let document = note_json(&ctx).expect("a reply federates");
        assert_eq!(document["inReplyTo"], PARENT_URI, "the parent address");
        assert_eq!(
            document["cc"],
            json!([format!("{AUTHOR_AP}/followers"), BOB_AP]),
            "the parent author rides along in cc, through the mentions"
        );
    }

    #[test]
    fn mentions_hashtags_and_used_emojis_become_tags() {
        let mut ctx = note_context(Visibility::Public);
        ctx.hashtags = vec!["fediverse".to_owned()];
        ctx.mentions = vec![mentioned_bob()];
        ctx.emojis = vec![
            fixture_emoji("railroad_track", "railroad_track"),
            fixture_emoji("gone", "gone"),
        ];
        ctx.status.raw_text = "hi @bob :railroad_track: #fediverse".to_owned();
        ctx.status.content = "<p>hi <span class=\"h-card\"><a href=\"https://remote.example/users/bob\" class=\"u-url mention\">@<span>bob</span></a></span> :railroad_track: <a href=\"https://test.example/tags/fediverse\" class=\"mention hashtag\" rel=\"tag\">#<span>fediverse</span></a></p>".to_owned();
        let document = note_json(&ctx).expect("a tagged note federates");
        assert_eq!(
            document["tag"],
            json!([
                {"type": "Hashtag", "href": format!("{BASE}/tags/fediverse"), "name": "#fediverse"},
                {"type": "Mention", "href": BOB_AP, "name": "@bob@remote.example"},
                {"type": "Emoji", "id": format!("{BASE}/emoji/railroad_track.png"), "name": ":railroad_track:",
                 "icon": {"type": "Image", "url": format!("{BASE}/emoji/railroad_track.png")}},
            ]),
            "hashtags, mentions, then the emojis the content actually uses"
        );
    }

    /// A local custom-emoji row named `shortcode` with a URL under `BASE`.
    fn fixture_emoji(shortcode: &str, stem: &str) -> CustomEmoji {
        CustomEmoji {
            id: Uuid::parse_str(AUTHOR_ID).unwrap(),
            shortcode: shortcode.to_owned(),
            domain: None,
            image_url: format!("{BASE}/emoji/{stem}.png"),
            static_url: None,
            category: None,
            tags: Vec::new(),
            visible_in_picker: true,
            disabled: false,
            inserted_at: parse_time(PUBLISHED).unwrap(),
            updated_at: parse_time(PUBLISHED).unwrap(),
        }
    }

    /// An image upload and a document, the attachment fixture pair.
    fn fixture_attachment(url: &str, content_type: &str) -> MediaAttachment {
        MediaAttachment {
            id: Uuid::parse_str(STATUS_ID).unwrap(),
            user_id: Some(Uuid::parse_str(AUTHOR_ID).unwrap()),
            status_id: Some(Uuid::parse_str(STATUS_ID).unwrap()),
            position: 0,
            storage_key: None,
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
            inserted_at: parse_time(PUBLISHED).unwrap(),
            updated_at: parse_time(PUBLISHED).unwrap(),
        }
    }

    #[test]
    fn attachments_federate_as_images_and_documents() {
        let mut ctx = note_context(Visibility::Public);
        let mut photo = fixture_attachment(&format!("{BASE}/media/cat.png"), "image/png");
        photo.description = Some("a cat".to_owned());
        photo.blurhash = Some("UBLHZ;9a".to_owned());
        let file = fixture_attachment(&format!("{BASE}/media/report.pdf"), "application/pdf");
        ctx.attachments = vec![photo, file];
        let document = note_json(&ctx).expect("attachments federate");
        assert_eq!(
            document["attachment"],
            json!([
                {"type": "Image", "mediaType": "image/png", "url": format!("{BASE}/media/cat.png"),
                 "name": "a cat", "blurhash": "UBLHZ;9a"},
                {"type": "Document", "mediaType": "application/pdf", "url": format!("{BASE}/media/report.pdf"),
                 "name": "", "blurhash": null},
            ]),
            "image/* classifies as Image, the rest as Document"
        );
    }

    #[test]
    fn a_quote_carries_both_quote_reference_spellings() {
        let mut ctx = note_context(Visibility::Public);
        ctx.quote_uri = Some(QUOTE_URI.to_owned());
        let document = note_json(&ctx).expect("a quote federates");
        assert_eq!(document["quoteUrl"], QUOTE_URI, "Mastodon's key");
        assert_eq!(document["quoteUri"], QUOTE_URI, "and Fedibird's");
        let mut keys: Vec<&str> = NOTE_KEYS.to_vec();
        keys.extend(["quoteUri", "quoteUrl"]);
        keys.sort_unstable();
        assert_eq!(json_keys(&document), keys, "exactly two keys more");
    }

    #[test]
    fn an_edited_status_reports_when_it_changed() {
        let mut ctx = note_context(Visibility::Public);
        ctx.status.edited_at = Some(parse_time(DELETED).unwrap());
        let document = note_json(&ctx).expect("an edit federates");
        assert_eq!(document["updated"], DELETED, "the update line");
        assert!(
            render(Visibility::Public).get("updated").is_none(),
            "and an unedited status carries none"
        );
    }

    /// The two-option `yes`/`no` choice pair, `yes` holding `first_votes`
    /// and `no` none.
    fn fixture_options(first_votes: i32) -> Vec<PollOptionData> {
        vec![
            PollOptionData {
                title: "yes".to_owned(),
                votes_count: first_votes,
            },
            PollOptionData {
                title: "no".to_owned(),
                votes_count: 0,
            },
        ]
    }

    #[test]
    fn a_single_choice_poll_renders_a_question_with_oneof() {
        let mut ctx = note_context(Visibility::Public);
        ctx.poll = Some(PollData {
            multiple: false,
            expires_at: Some(parse_time(FAR_END).unwrap()),
            closed: false,
            voters_count: 3,
            options: fixture_options(2),
        });
        let document = note_json(&ctx).expect("a question federates");
        assert_eq!(document["type"], "Question", "a poll makes it a Question");
        assert_eq!(
            document["oneOf"],
            json!([
                {"type": "Note", "name": "yes",
                 "replies": {"type": "Collection", "totalItems": 2}},
                {"type": "Note", "name": "no",
                 "replies": {"type": "Collection", "totalItems": 0}},
            ]),
            "the choices, with their tallies as collections"
        );
        assert!(document.get("anyOf").is_none(), "single choice uses oneOf");
        assert_eq!(document["votersCount"], 3, "the voter count");
        assert_eq!(document["endTime"], FAR_END, "the deadline");
        assert!(
            document.get("closed").is_none(),
            "an open poll reports no close"
        );
    }

    #[test]
    fn a_multiple_choice_expired_poll_uses_anyof_and_closes() {
        let mut ctx = note_context(Visibility::Public);
        ctx.poll = Some(PollData {
            multiple: true,
            expires_at: Some(parse_time(DELETED).unwrap()),
            closed: true,
            voters_count: 7,
            options: fixture_options(5),
        });
        let document = note_json(&ctx).expect("a closed question federates");
        assert!(
            document.get("oneOf").is_none() && document["anyOf"].is_array(),
            "multiple choice addresses its options as anyOf"
        );
        assert_eq!(document["endTime"], DELETED, "the deadline it ran out with");
        assert_eq!(document["closed"], DELETED, "closed repeats the end time");
        assert_eq!(document["votersCount"], 7, "the taller voter count");
    }

    #[test]
    fn the_note_context_is_the_documented_schema_chain() {
        let document = render(Visibility::Public);
        let context = document["@context"].as_array().expect("an array context");
        assert_eq!(context.len(), 3, "vocabulary, schema, language map");
        assert_eq!(context[0], json!(AS_VOCABULARY), "the AS vocabulary");
        assert_eq!(
            context[1],
            json!(format!("{BASE}{LITEPUB_SCHEMA_PATH}")),
            "this instance's schema document"
        );
        assert_eq!(context[2], json!({"@language": "und"}), "the language map");
    }

    #[test]
    fn the_litepub_document_defines_the_terms_in_use() {
        let document = litepub_context_json();
        let context = document["@context"].as_object().expect("a term map");
        assert_eq!(
            context["conversation"],
            json!({"@id": "ostatus:conversation", "@type": "@id"}),
            "the ostatus conversation reference"
        );
        assert_eq!(context["EmojiReact"], "litepub:EmojiReact", "litepub terms");
        assert_eq!(context["capabilities"], "litepub:capabilities", "…");
        assert_eq!(context["directMessage"], "litepub:directMessage", "…");
        assert_eq!(context["quoteUrl"], "as:quoteUrl", "the quote pair");
        assert_eq!(context["quoteUri"], "fedibird:quoteUri", "…");
        assert_eq!(context["votersCount"], "toot:votersCount", "the poll count");
        assert_eq!(context["sensitive"], "as:sensitive", "the cw flag");
        assert_eq!(context["Hashtag"], "as:Hashtag", "the tag type");
        assert_eq!(context["Emoji"], "toot:Emoji", "the emoji type");
    }

    #[test]
    fn create_wraps_the_note_with_its_own_addressing() {
        let ctx = note_context(Visibility::Public);
        let note = note_json(&ctx).expect("the fixture renders");
        let status = base_status(Visibility::Public);
        let config = Config::for_tests();
        let activity = create_activity_json(&note, &status, &config);
        assert_eq!(
            json_keys(&activity),
            [
                "@context",
                "actor",
                "cc",
                "id",
                "object",
                "published",
                "to",
                "type"
            ],
            "the create key set"
        );
        assert_eq!(
            activity["id"],
            format!("{BASE}/activities/{STATUS_ID}"),
            "the activity id derives from the status id"
        );
        assert_eq!(activity["type"], "Create", "the activity type");
        assert_eq!(activity["actor"], AUTHOR_AP, "the object's author acts");
        assert_eq!(
            activity["to"], note["to"],
            "the addressing mirrors the object"
        );
        assert_eq!(activity["cc"], note["cc"], "…both lines");
        assert_eq!(activity["object"], note, "the note rides inside verbatim");
        assert_eq!(activity["published"], PUBLISHED, "the status's own time");
    }

    #[test]
    fn a_public_reblog_announces_pleroma_style() {
        let reblog = base_status(Visibility::Public);
        let config = Config::for_tests();
        let activity = announce_activity_json(&reblog, &author(), PARENT_URI, BOB_AP, &config)
            .expect("a public announce renders");
        assert_eq!(
            json_keys(&activity),
            [
                "@context",
                "actor",
                "cc",
                "id",
                "object",
                "published",
                "to",
                "type"
            ],
            "the announce key set"
        );
        assert_eq!(
            activity["id"], reblog.uri,
            "the reblog row's stored uri is the activity id"
        );
        assert_eq!(activity["type"], "Announce", "the activity type");
        assert_eq!(activity["actor"], AUTHOR_AP, "the booster acts");
        assert_eq!(activity["object"], PARENT_URI, "the boosted object");
        assert_eq!(
            activity["to"],
            json!([format!("{AUTHOR_AP}/followers"), BOB_AP]),
            "followers and the boosted author are addressed"
        );
        assert_eq!(activity["cc"], json!([AS_PUBLIC]), "the world is cc-ed");
    }

    #[test]
    fn a_private_reblog_announces_to_followers_only() {
        let reblog = base_status(Visibility::Private);
        let config = Config::for_tests();
        let activity = announce_activity_json(&reblog, &author(), PARENT_URI, BOB_AP, &config)
            .expect("a private announce renders");
        assert_eq!(
            activity["to"],
            json!([format!("{AUTHOR_AP}/followers")]),
            "only the followers"
        );
        assert_eq!(activity["cc"], json!([]), "cc stays empty");
    }

    #[test]
    fn a_local_reblog_is_never_announced() {
        let reblog = base_status(Visibility::Local);
        let config = Config::for_tests();
        assert!(
            announce_activity_json(&reblog, &author(), PARENT_URI, BOB_AP, &config).is_none(),
            "a local boost has no activity"
        );
    }

    #[test]
    fn a_deleted_status_serves_a_tombstone() {
        let mut deleted = base_status(Visibility::Public);
        deleted.deleted_at = Some(parse_time(DELETED).unwrap());
        assert_eq!(
            tombstone_json(&deleted),
            json!({
                "id": format!("{BASE}/objects/{STATUS_ID}"),
                "type": "Tombstone",
                "formerType": "Note",
                "deleted": DELETED,
            }),
            "the documented tombstone stub"
        );

        let live = base_status(Visibility::Public);
        assert_eq!(
            tombstone_json(&live)["deleted"],
            PUBLISHED,
            "a row that recorded no deletion falls back to its last write"
        );
    }

    #[test]
    fn shortcode_scan_reads_complete_references_only() {
        assert_eq!(
            shortcodes_in("a :one: b :two:"),
            vec!["one".to_owned(), "two".to_owned()],
            "two references in running text"
        );
        assert_eq!(
            shortcodes_in(":a::b:"),
            vec!["a".to_owned(), "b".to_owned()],
            "adjacent references split on the second colon"
        );
        assert!(shortcodes_in("no codes here").is_empty(), "no colons");
        assert!(
            shortcodes_in(":unclosed").is_empty(),
            "an open candidate drops"
        );
        assert!(shortcodes_in("::").is_empty(), "an empty code is no code");
        assert!(
            shortcodes_in(":a b:").is_empty(),
            "spaces break a candidate"
        );
        assert_eq!(
            shortcodes_in("text:colon and :ok:"),
            vec!["ok".to_owned()],
            "a bare inner colon does not start what never closes right"
        );
    }

    /// The instance origin rows are stored under — [`BASE`], the same
    /// origin [`Config::for_tests`] serves.
    async fn create_local(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("fixture user inserts")
    }

    /// A mirror of `nickname` on `host`, for mention fixtures.
    async fn create_remote(pool: &PgPool, nickname: &str, host: &str) -> User {
        let ap_id = format!("https://{host}/users/{nickname}");
        insert_user(
            pool,
            &NewUser {
                nickname: format!("{nickname}@{host}"),
                ap_id,
                domain: Some(host.to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("fixture remote user inserts")
    }

    /// A live local post of `author` built from `new`, already inserted.
    async fn post(pool: &PgPool, new: &NewStatus) -> Status {
        insert_status(pool, BASE, new)
            .await
            .expect("fixture status inserts")
    }

    /// The mention list's actor ids, the shape every loader assertion
    /// compares.
    fn mention_ap_ids(ctx: &NoteContext) -> Vec<&str> {
        ctx.mentions
            .iter()
            .map(|person| person.ap_id.as_str())
            .collect()
    }

    #[tokio::test]
    async fn load_note_context_reads_a_bare_status_with_no_satellites() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_local(&pool, "plainalice").await;
        let posted = post(&pool, &NewStatus::local(&alice, "plain hello")).await;
        let config = Config::for_tests();

        let ctx = load_note_context(&pool, &config, posted.id)
            .await
            .expect("the note context loads");
        assert_eq!(ctx.status.id, posted.id, "the row itself");
        assert_eq!(ctx.author.id, alice.id, "its author");
        assert_eq!(ctx.config.base_url, BASE, "the config rides along");
        assert!(
            ctx.reply_parent_uri.is_none()
                && ctx.mentions.is_empty()
                && ctx.hashtags.is_empty()
                && ctx.attachments.is_empty()
                && ctx.emojis.is_empty()
                && ctx.poll.is_none()
                && ctx.quote_uri.is_none(),
            "a plain post carries nothing else"
        );
        let document = note_json(&ctx).expect("and the loaded context renders");
        assert_eq!(document["id"], posted.uri, "the object id is the uri");
        assert_eq!(document["attributedTo"], alice.ap_id, "the author acts");
    }

    #[tokio::test]
    async fn load_note_context_assembles_the_reply_tags_and_quote() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_local(&pool, "loadingalice").await;
        let bob = create_local(&pool, "loadingbob").await;
        let carol = create_remote(&pool, "loadingcarol", "remote.example").await;
        let parent = post(&pool, &NewStatus::local(&bob, "the parent")).await;
        let enabled = upsert_emoji(
            &pool,
            &NewCustomEmoji::local("railroad_track", &format!("{BASE}/emoji/railroad.png")),
        )
        .await
        .expect("enabled emoji row");
        assert_eq!(
            enabled.shortcode, "railroad_track",
            "the enabled row is stored"
        );
        let hidden = upsert_emoji(
            &pool,
            &NewCustomEmoji {
                disabled: true,
                ..NewCustomEmoji::local("hidden_thing", &format!("{BASE}/emoji/hidden.png"))
            },
        )
        .await
        .expect("disabled emoji row");
        assert!(hidden.disabled, "and it is stored disabled");
        let posted = post(
            &pool,
            &NewStatus {
                in_reply_to_id: Some(parent.id),
                in_reply_to_account_id: Some(bob.id),
                mentioned_user_ids: vec![bob.id, carol.id],
                hashtags: vec!["fediverse".to_owned()],
                quote_of_id: Some(parent.id),
                raw_text: "hi :railroad_track: :hidden_thing: #fediverse".to_owned(),
                content: "<p>hi :railroad_track: :hidden_thing: #fediverse</p>".to_owned(),
                ..NewStatus::local(&alice, "answer")
            },
        )
        .await;
        let config = Config::for_tests();

        let ctx = load_note_context(&pool, &config, posted.id)
            .await
            .expect("the full context loads");
        assert_eq!(
            ctx.reply_parent_uri.as_deref(),
            Some(parent.uri.as_str()),
            "the parent resolves to its object uri"
        );
        assert_eq!(
            mention_ap_ids(&ctx),
            vec![bob.ap_id.as_str(), carol.ap_id.as_str()],
            "mentions keep link order and the parent author is not doubled"
        );
        assert_eq!(ctx.hashtags, vec!["fediverse".to_owned()], "the tag name");
        assert_eq!(ctx.emojis.len(), 1, "only the enabled emoji row survives");
        assert_eq!(ctx.emojis[0].shortcode, "railroad_track", "…by name");
        assert_eq!(
            ctx.quote_uri.as_deref(),
            Some(parent.uri.as_str()),
            "the quote resolves to its object uri"
        );

        let document = note_json(&ctx).expect("the assembled note renders");
        assert_eq!(document["inReplyTo"], json!(parent.uri), "the parent line");
        assert_eq!(document["quoteUrl"], json!(parent.uri), "the quote line");
        assert_eq!(
            document["tag"],
            json!([
                {"type": "Hashtag", "href": format!("{BASE}/tags/fediverse"), "name": "#fediverse"},
                {"type": "Mention", "href": bob.ap_id, "name": "@loadingbob"},
                {"type": "Mention", "href": carol.ap_id, "name": "@loadingcarol@remote.example"},
                {"type": "Emoji", "id": format!("{BASE}/emoji/railroad.png"), "name": ":railroad_track:",
                 "icon": {"type": "Image", "url": format!("{BASE}/emoji/railroad.png")}},
            ]),
            "the full tag list, in render order"
        );
    }

    #[tokio::test]
    async fn load_note_context_loads_media_and_the_expired_poll() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_local(&pool, "pollalice").await;
        let media = insert_media(
            &pool,
            &NewMediaAttachment {
                user_id: Some(alice.id),
                url: format!("{BASE}/media/loading/cat.png"),
                content_type: "image/png".to_owned(),
                description: Some("a cat".to_owned()),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .expect("fixture media uploads");
        let posted = post(
            &pool,
            &NewStatus {
                media_ids: vec![media.id],
                ..NewStatus::local(&alice, "vote now")
            },
        )
        .await;
        let mut conn = pool.acquire().await.expect("slot connection");
        let created = create_poll(
            &mut conn,
            posted.id,
            &PollParams {
                options: vec!["yes".to_owned(), "no".to_owned()],
                expires_at: Some(Utc::now() - TimeDelta::hours(1)),
                multiple: true,
                ..PollParams::default()
            },
        )
        .await
        .expect("fixture poll creates");
        assert_eq!(created.status_id, posted.id, "the poll names its status");
        drop(conn);
        let config = Config::for_tests();

        let ctx = load_note_context(&pool, &config, posted.id)
            .await
            .expect("the media-and-poll context loads");
        assert_eq!(ctx.attachments.len(), 1, "the claimed media");
        assert_eq!(ctx.attachments[0].id, media.id, "and it is the row");
        let poll_data = ctx.poll.clone().expect("the poll loaded");
        assert!(poll_data.multiple, "the multiple flag round trips");
        assert!(poll_data.closed, "an expired deadline reads as closed");
        assert_eq!(poll_data.voters_count, 0, "a fresh poll has no voters");
        assert_eq!(poll_data.options.len(), 2, "both options came along");

        let document = note_json(&ctx).expect("the assembled note renders");
        assert_eq!(document["type"], "Question", "the poll makes it a question");
        assert!(
            document.get("anyOf").is_some() && document.get("oneOf").is_none(),
            "multiple choice renders anyOf, never oneOf"
        );
        assert_eq!(
            document["attachment"],
            json!([
                {"type": "Image", "mediaType": "image/png", "url": format!("{BASE}/media/loading/cat.png"),
                 "name": "a cat", "blurhash": null},
            ]),
            "the media as an Image attachment"
        );
        assert_eq!(
            document["endTime"],
            json!(mastodon_time(&poll_data.expires_at.expect("a deadline"))),
            "the rendered end time is the stored one"
        );
    }

    #[tokio::test]
    async fn load_note_context_adds_a_silent_reply_s_parent_author() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_local(&pool, "quietalice").await;
        let bob = create_local(&pool, "quietbob").await;
        let parent = post(&pool, &NewStatus::local(&bob, "answer me")).await;
        let posted = post(
            &pool,
            &NewStatus {
                in_reply_to_id: Some(parent.id),
                in_reply_to_account_id: Some(bob.id),
                ..NewStatus::local(&alice, "okay")
            },
        )
        .await;
        let config = Config::for_tests();

        let ctx = load_note_context(&pool, &config, posted.id)
            .await
            .expect("the reply context loads");
        assert_eq!(
            mention_ap_ids(&ctx),
            vec![bob.ap_id.as_str()],
            "the parent author joins the addressing even unmentioned"
        );
        let document = note_json(&ctx).expect("and renders");
        assert_eq!(
            document["cc"],
            json!([format!("{}/followers", alice.ap_id), bob.ap_id]),
            "the public note cc-ed the parent author as well as the followers"
        );
    }

    #[tokio::test]
    async fn load_note_context_resolves_stored_and_unstored_parents_alike() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_local(&pool, "eitheralice").await;
        let posted = post(
            &pool,
            &NewStatus {
                in_reply_to_ap_id: Some(PARENT_URI.to_owned()),
                ..NewStatus::local(&alice, "remote answer")
            },
        )
        .await;
        let config = Config::for_tests();
        let stored_ctx = load_note_context(&pool, &config, posted.id)
            .await
            .expect("the unresolved-parent reply loads");
        assert_eq!(
            stored_ctx.reply_parent_uri.as_deref(),
            Some(PARENT_URI),
            "a parent this instance never stored rides on as its ap_id"
        );

        let bob = create_local(&pool, "eitherbob").await;
        let parent = post(&pool, &NewStatus::local(&bob, "a deleted parent")).await;
        let reply = post(
            &pool,
            &NewStatus {
                in_reply_to_id: Some(parent.id),
                in_reply_to_account_id: Some(bob.id),
                ..NewStatus::local(&alice, "still answering")
            },
        )
        .await;
        let tombstone = soft_delete(&pool, parent.id)
            .await
            .expect("the parent is soft-deleted")
            .expect("and was live");
        assert_eq!(tombstone.id, parent.id, "the parent is a tombstone now");
        let tombstoned_ctx = load_note_context(&pool, &config, reply.id)
            .await
            .expect("a reply to a deleted parent still loads");
        assert_eq!(
            tombstoned_ctx.reply_parent_uri.as_deref(),
            Some(parent.uri.as_str()),
            "and still names the tombstone's reserved uri"
        );
    }

    #[tokio::test]
    async fn load_note_context_refuses_unknown_and_deleted_statuses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_local(&pool, "gonealice").await;
        let config = Config::for_tests();

        let unknown_err = load_note_context(&pool, &config, new_id())
            .await
            .expect_err("an unknown id loads nothing");
        assert!(
            matches!(unknown_err, AppError::NotFound),
            "the unknown id is a 404"
        );

        let posted = post(&pool, &NewStatus::local(&alice, "delete me")).await;
        let tombstone = soft_delete(&pool, posted.id)
            .await
            .expect("the status is soft-deleted")
            .expect("and was live");
        assert_eq!(tombstone.id, posted.id, "the row is a tombstone now");
        let deleted_err = load_note_context(&pool, &config, posted.id)
            .await
            .expect_err("a tombstone loads no document");
        assert!(
            matches!(deleted_err, AppError::NotFound),
            "and a deleted status is the same 404"
        );
    }
}
