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
//!
//! The module is a tree: the envelope checks live in `envelope`, the text
//! fields in `text`, the audience rules in `addressing`, the `tag` array
//! in `tags`, the media in `attachments` and the `Question`'s poll in
//! `polls`; the data shapes live in `types`, and [`parse_note`] stitches
//! their parts together here. The root keeps the shared constants and
//! re-exports the whole outside-facing surface.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{
    federation::json::value_or_id,
    models::{status::NewStatus, user::User},
    util::{format::truncate_chars, sanitize::strip_html},
};

pub mod addressing;
pub mod attachments;
mod envelope;
mod polls;
mod quote_policy;
mod tags;
#[cfg(test)]
mod test_support;
mod text;
mod types;

pub use types::{
    ParseError, ParsedAttachment, ParsedNote, ParsedPoll, ParsedPollOption, RemoteEmoji,
};

pub(crate) use addressing::parse_visibility;
pub(crate) use attachments::pixel_dimension;

use addressing::addressed_actor_ids;
use attachments::parse_attachments;
use envelope::{check_containment, check_size, object_id, object_kind, resolve_url};
use polls::parse_poll;
use quote_policy::parse_quote_policy;
use tags::parse_tags;
use text::{field_reference, parse_language, parse_sensitive, parse_text, parse_times};

/// The `Emoji` entries of `json`'s `tag` array, in `author`'s domain.
///
/// What [`parse_note`] keeps of them, for objects that are no note: a chat
/// message carries emojis but no mentions or hashtags.
pub(crate) fn emoji_tags(json: &Value, author: &User) -> Vec<RemoteEmoji> {
    parse_tags(json, author).emojis
}

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

/// The quote-reference fields, tried in order: the FEP-044f `quote`,
/// Mastodon's and Fedibird's `quoteUrl`/`quoteUri` and Misskey's
/// `_misskey_quote` (Pleroma's `CommonFixes.fix_quote_url`).
const QUOTE_KEYS: [&str; 4] = ["quote", "quoteUrl", "quoteUri", "_misskey_quote"];

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
/// How many attachments are kept at most; the rest of the array is ignored.
const MAX_ATTACHMENTS: usize = 20;
/// How many poll options of a remote `Question` are kept; the rest of the
/// list is dropped, not an error. A federated cap, deliberately above the
/// local posting limit (`MAX_POLL_OPTIONS` in the config): Pleroma, Akkoma
/// and Misskey allow larger polls than this server lets users create.
const MAX_POLL_OPTIONS: usize = 100;
/// The `content_type` a document without `source.mediaType` describes: AP
/// `content` is HTML (decision 6 stores what the remote rendered).
const DEFAULT_HTML_CONTENT_TYPE: &str = "text/html";

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
/// holds no usable option. A list past the option cap is truncated.
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
        language: parse_language(json),
        quote_policy: parse_quote_policy(json),
        ..NewStatus::default()
    };
    Ok(ParsedNote {
        status,
        hashtags: tags.hashtags,
        mention_hrefs: tags.mention_hrefs,
        audience: addressed_actor_ids(json, author),
        emojis: tags.emojis,
        attachments: parse_attachments(json),
        reply_to_uri: json.get("inReplyTo").and_then(value_or_id),
        quote_uri: field_reference(json, &QUOTE_KEYS),
        ap_context: field_reference(json, &CONTEXT_KEYS),
        poll,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{federation::note_parser::test_support::*, models::status::Visibility};

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

    #[test]
    fn the_fep_044f_quote_feeds_the_quote() {
        let mut note = minimal_note();
        note["quote"] = json!("https://a.example/x");
        assert_eq!(
            parse(&note).quote_uri.as_deref(),
            Some("https://a.example/x"),
            "a bare quote property is the reference"
        );
    }

    #[test]
    fn the_can_quote_policy_lands_on_the_status() {
        let mut note = minimal_note();
        assert_eq!(parse(&note).status.quote_policy, None, "no policy stated");
        note["interactionPolicy"] = json!({"canQuote": {
            "automaticApproval": ["https://remote.example/users/bob/followers"],
        }});
        let policy = parse(&note).status.quote_policy.expect("a policy");
        assert_eq!(
            policy.automatic,
            ["https://remote.example/users/bob/followers"],
            "the automatic approvers"
        );
        assert!(policy.manual.is_empty(), "and no manual ones");
    }

    #[test]
    fn the_fep_044f_quote_wins_over_the_legacy_keys() {
        let mut note = minimal_note();
        note["quote"] = json!("https://a.example/new");
        note["quoteUrl"] = json!("https://a.example/old");
        assert_eq!(
            parse(&note).quote_uri.as_deref(),
            Some("https://a.example/new"),
            "quote is tried first"
        );
    }

    #[test]
    fn a_tombstone_quote_falls_back_to_the_next_key() {
        let mut note = minimal_note();
        note["quote"] = json!({"type": "Tombstone", "id": "https://a.example/gone"});
        note["quoteUrl"] = json!("https://a.example/live");
        assert_eq!(
            parse(&note).quote_uri.as_deref(),
            Some("https://a.example/live"),
            "the Tombstone is skipped for quoteUrl"
        );
    }

    #[test]
    fn a_tombstone_quote_alone_references_nothing() {
        let mut note = minimal_note();
        note["quote"] = json!({"type": ["Tombstone"], "id": "https://a.example/gone"});
        assert_eq!(
            parse(&note).quote_uri,
            None,
            "a Tombstone (even typed by array) is no quote"
        );
    }

    #[test]
    fn a_non_tombstone_quote_object_reads_its_id() {
        let mut note = minimal_note();
        note["quote"] = json!({"type": "Note", "id": "https://a.example/obj"});
        assert_eq!(
            parse(&note).quote_uri.as_deref(),
            Some("https://a.example/obj"),
            "an embedded object references its id"
        );
    }
}
