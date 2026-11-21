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
//! [`update_activity_json`] the same object re-drafted as an `Update` after
//! an edit, [`delete_activity_json`] the row's [`tombstone_json`] as the
//! `Delete` that withdraws it, [`announce_activity_json`] renders a reblog
//! row as the `Announce` it federates as (decision 6: the reblog *is* the
//! activity, and its stored `uri` is the activity id), and
//! [`tombstone_json`] is what a deleted status becomes.
//! [`litepub_context_json`] is the JSON-LD schema document
//! every one of these references from its `@context`, served at
//! [`LITEPUB_SCHEMA_PATH`] by [`crate::api::activitypub`].
//!
//! Deliberate deltas from Pleroma, fixed by the ticket: `likes`, `replies`,
//! `shares` and the `emoji` map are not written into the object (readers
//! discover them through their own collections), the internal `context_id`
//! stays internal, and a `local`-visibility status is never federated at
//! all — [`note_json`] and [`announce_activity_json`] answer `None` for it.
//!
//! The module is a thin root: the vocabulary constants and the [`NoteContext`]
//! data live here, the object body is built in the `fields` and `tags`
//! children, the documents that carry or retire it in `activities`, and the
//! database reads in `load`; the root re-exports the whole outside-facing
//! surface.

use chrono::{DateTime, Utc};

use crate::{
    config::Config,
    models::{
        custom_emoji::CustomEmoji, media_attachment::MediaAttachment, status::Status, user::User,
    },
};

pub mod activities;
pub mod fields;
pub mod load;
pub mod tags;
#[cfg(test)]
mod test_support;

pub use activities::{
    NoteDeletion, announce_activity_json, create_activity_json, delete_activity_json,
    litepub_context_json, tombstone_json, update_activity_json,
};
pub use fields::note_json;
pub use load::load_note_context;

pub(crate) use activities::json_ld_context;
pub(crate) use fields::address_lines;
pub(crate) use load::load_mentions;
pub(crate) use tags::attachment_json;

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
/// The type of an edit's activity.
const TYPE_UPDATE: &str = "Update";
/// The type of a status withdrawal's activity.
const TYPE_DELETE: &str = "Delete";
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
