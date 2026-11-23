//! Remote actors: parsing their documents and mirroring them into `users`.
//!
//! [`parse_actor`] turns the JSON of a fetched `ActivityPub` actor document
//! into a [`NewRemoteUser`], and [`upsert_remote_user`] stores it as the
//! instance's mirror row (decision 7). The reference behaviour is Pleroma's
//! `Pleroma.Web.ActivityPub.ActivityPub.user_data_from_user_object/1`
//! (`lib/pleroma/web/activity_pub/activity_pub.ex`, the extraction feeding
//! `make_user_from_ap_id`) and `Pleroma.User.remote_user_changeset`
//! (`lib/pleroma/user.ex`, the stored subset); the ticket rules here are
//! authoritative where the two differ.
//!
//! Validation is deliberately narrow: `id`, `preferredUsername`, `inbox`
//! and `publicKey.publicKeyPem` are required, the actor `type` must be one
//! of the five supported kinds (`Person`, `Service`, `Group`,
//! `Organization`, `Application`), a `publicKey.owner` that is present must
//! name the actor, and every URL that is stored must be an absolute
//! `http(s)` URL.
//! There is no origin-containment check between hosts: an `inbox` on a
//! different host from `id` is accepted (later delivery tickets decide
//! where activity is sent), matching the ticket's "only require
//! scheme-valid URLs".
//!
//! Optional fields that are present but malformed — a non-http `outbox`, an
//! unparseable `published`, an `icon` without a usable `url` — are dropped
//! rather than rejected, so one broken field cannot lose the whole profile.
//! Fields that the upsert does not store (`movedTo`,
//! `capabilities.acceptsChatMessages`, `published`) are still
//! extracted onto [`NewRemoteUser`]: the fetch and move tickets consume
//! them, and parsing them here is what validates their shape.
//!
//! A remote actor recreated under a new id (an origin that reinstalled,
//! moved its actor paths, or deleted and recreated the account) keeps its
//! `preferredUsername@host` handle. [`upsert_remote_user`] frees the handle
//! by renaming the stale mirror of the same domain to `<old id>.<nickname>`
//! and stores the new actor, as Pleroma's `maybe_handle_clashing_nickname`
//! does; [`ActorError::NicknameConflict`] is kept for a holder that is not
//! such a mirror (a local row, or another domain).
//!
//! Network access is out of scope for this module on purpose: documents
//! arrive already fetched; retrieving them is the next ticket.

mod error;
mod fields;
mod parse;
#[cfg(test)]
mod test_support;
mod upsert;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::models::user::Flag;

pub use self::{error::ActorError, parse::parse_actor, upsert::upsert_remote_user};

/// Actor `type`s this instance mirrors, the `ActivityStreams` kinds
/// Mastodon-family actors send (Pleroma's `object_to_user_data` stores any
/// string it sees; the ticket fixes a closed set instead, so a document of
/// an unknown kind is rejected rather than half-mirrored).
const ACTOR_TYPES: [&str; 5] = ["Person", "Service", "Group", "Organization", "Application"];
/// The `type` an `attachment` entry must carry to be stored as a profile
/// field (Mastodon's profile metadata, Pleroma's
/// `Enum.filter(..., & &1["type"] == "PropertyValue")`).
const PROPERTY_VALUE_TYPE: &str = "PropertyValue";

/// Longest display name kept, in characters (Mastodon's `name` cap).
const MAX_REMOTE_NAME_CHARS: usize = 2048;
/// Longest raw `summary` kept, in characters, applied before sanitizing.
const MAX_REMOTE_BIO_CHARS: usize = 20_000;
/// Most profile fields kept; later `PropertyValue` entries are dropped.
const MAX_REMOTE_FIELDS: usize = 50;
/// Longest profile-field name kept, in characters.
const MAX_FIELD_NAME_CHARS: usize = 512;
/// Longest profile-field value kept, in characters.
const MAX_FIELD_VALUE_CHARS: usize = 2048;
/// Most `alsoKnownAs` aliases kept; later entries are dropped.
const MAX_ALIASES: usize = 256;
/// Longest `preferredUsername` accepted, in characters; longer is rejected.
const MAX_USERNAME_CHARS: usize = 2048;

/// The longest prefix of `text` of at most `max` characters, cut on a char
/// boundary.
fn truncate_chars(text: &str, max: usize) -> &str {
    text.char_indices()
        .nth(max)
        .and_then(|(end, _)| text.get(..end))
        .unwrap_or(text)
}

/// A remote actor document parsed into upsert-ready data.
///
/// The field set is what [`parse_actor`] validates and extracts: the
/// [`User`](crate::models::user::User) columns a document can fill, plus the
/// three fields [`upsert_remote_user`] does not store yet
/// ([`moved_to`](Self::moved_to),
/// [`accepts_chat_messages`](Self::accepts_chat_messages),
/// [`published`](Self::published)). The first two have `users` columns
/// since the Pleroma extension migration, `published` has none at all;
/// storing any of them belongs to the move and chat tickets.
/// [`also_known_as`](Self::also_known_as) is stored: the `Move` handler
/// reads it off the mirror.
#[derive(Clone, Debug)]
pub struct NewRemoteUser {
    /// `{preferredUsername}@{host of ap_id}`, decision 7.
    pub nickname: String,
    /// The actor `id`, stored verbatim as the remote sent it.
    pub ap_id: String,
    /// Always unset: the row is a mirror, never owned here.
    pub local: Flag,
    /// The host of [`ap_id`](Self::ap_id), lowercased by URL parsing.
    pub domain: String,
    /// The validated actor `type`.
    pub actor_type: String,
    /// Display name as sent; empty when absent.
    pub name: String,
    /// Profile note as sanitized HTML: the `summary` run through
    /// [`sanitize_html`](crate::util::sanitize::sanitize_html).
    pub bio: String,
    /// Profile note as source text: the `summary` run through
    /// [`strip_html`](crate::util::sanitize::strip_html).
    pub raw_bio: String,
    /// The `icon` URL, when the field carries a usable one.
    pub avatar_url: Option<String>,
    /// The `image` URL, when the field carries a usable one.
    pub banner_url: Option<String>,
    /// The first string of `url` (the human-facing profile page).
    pub profile_url: Option<String>,
    /// `manuallyApprovesFollowers`.
    pub locked: Flag,
    /// `discoverable`.
    pub discoverable: Flag,
    /// The `PropertyValue` entries of `attachment` as `{name, value}`
    /// objects, the shape of the `fields` column.
    pub fields: Value,
    /// The required `inbox` endpoint.
    pub inbox: String,
    /// The `endpoints.sharedInbox` endpoint, when present and usable.
    pub shared_inbox: Option<String>,
    /// The `outbox` collection id, when present and usable.
    pub outbox: Option<String>,
    /// The `followers` collection id, when present and usable.
    pub followers_url: Option<String>,
    /// The `following` collection id, when present and usable.
    pub following_url: Option<String>,
    /// The `featured` collection id, when present and usable.
    pub featured_url: Option<String>,
    /// The required `publicKey.publicKeyPem` signing key.
    pub public_key: String,
    /// The `alsoKnownAs` entries, stored by the upsert on every fetch.
    pub also_known_as: Vec<String>,
    /// The `movedTo` actor id (the column exists; the upsert does not write it).
    pub moved_to: Option<String>,
    /// `capabilities.acceptsChatMessages`. The column exists but the upsert
    /// does not write it, so a mirror keeps the column default — which is
    /// what `pleroma.accepts_chat_messages` renders as JSON `null`.
    pub accepts_chat_messages: Flag,
    /// The actor's `published` creation time (no column yet;
    /// unparseable or absent is `None`).
    pub published: Option<DateTime<Utc>>,
}
