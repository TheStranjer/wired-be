//! The shapes a note parse answers with: every way a document can fail
//! ([`ParseError`]), the insert-ready result ([`ParsedNote`]) and its
//! satellite records, and the intermediate structs the children hand to
//! each other.

use std::{error::Error, fmt};

use chrono::{DateTime, Utc};

use crate::models::status::NewStatus;

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
    /// A `Question` whose `oneOf`/`anyOf` is absent or holds no option with
    /// a name. A list that is merely too long is truncated, not rejected.
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
pub(super) struct NoteText {
    /// Sanitized, capped HTML.
    pub(super) content: String,
    /// The author's own source text, or the content reduced to text.
    pub(super) raw_text: String,
    /// The `MIME` type of [`raw_text`](Self::raw_text).
    pub(super) content_type: String,
}

/// The timestamps a document arrives with, resolved against the caller's
/// `now`.
#[derive(Clone, Debug)]
pub(super) struct NoteTimes {
    /// The clamped `published`.
    pub(super) inserted_at: DateTime<Utc>,
    /// The `updated`, when it postdates [`inserted_at`](Self::inserted_at).
    pub(super) edited_at: Option<DateTime<Utc>>,
}

/// The three tag lists a document's `tag` array splits into.
#[derive(Clone, Debug, Default)]
pub(super) struct TagSets {
    /// Lowercased hashtag names.
    pub(super) hashtags: Vec<String>,
    /// Mention hrefs.
    pub(super) mention_hrefs: Vec<String>,
    /// Remote custom emojis.
    pub(super) emojis: Vec<RemoteEmoji>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
