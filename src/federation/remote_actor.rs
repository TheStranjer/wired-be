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
//! Fields that the `users` table has no column for (`alsoKnownAs`,
//! `movedTo`, `capabilities.acceptsChatMessages`, `published`) are still
//! extracted onto [`NewRemoteUser`]: the fetch and move tickets consume
//! them, and parsing them here is what validates their shape.
//!
//! Network access is out of scope for this module on purpose: documents
//! arrive already fetched; retrieving them is the next ticket.

use std::{error::Error, fmt};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{Error as SqlxError, PgPool};
use url::Url;

use crate::{
    error::AppError,
    federation::json::{as_array, first_string, value_or_id},
    models::user::{DEFAULT_ACTOR_TYPE, Flag, User},
    util::{
        id::new_id,
        sanitize::{sanitize_html, strip_html},
        time::parse_time,
    },
};

/// Actor `type`s this instance mirrors, the `ActivityStreams` kinds
/// Mastodon-family actors send (Pleroma's `object_to_user_data` stores any
/// string it sees; the ticket fixes a closed set instead, so a document of
/// an unknown kind is rejected rather than half-mirrored).
const ACTOR_TYPES: [&str; 5] = ["Person", "Service", "Group", "Organization", "Application"];
/// The `type` an `attachment` entry must carry to be stored as a profile
/// field (Mastodon's profile metadata, Pleroma's
/// `Enum.filter(..., & &1["type"] == "PropertyValue")`).
const PROPERTY_VALUE_TYPE: &str = "PropertyValue";

/// The upsert behind [`upsert_remote_user`]: insert keyed by a fresh
/// `UUIDv7`, or, when `ap_id` already has a row, overwrite exactly the
/// profile, key and endpoint columns the actor document describes plus
/// `last_fetched_at` and `updated_at`. `local`, `bot`, the login columns,
/// the moderation roles and the denormalised counters are deliberately not
/// touched: re-fetching a remote actor must not undo anything this instance
/// knows beyond the document itself.
const UPSERT_SQL: &str = "INSERT INTO users (
     id, nickname, ap_id, local, domain, actor_type, name, bio, raw_bio,
     avatar_url, banner_url, profile_url, locked, discoverable, fields,
     inbox, shared_inbox, outbox, followers_url, following_url,
     featured_url, public_key, last_fetched_at
 ) VALUES (
     $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
     $16, $17, $18, $19, $20, $21, $22, now()
 ) ON CONFLICT (ap_id) DO UPDATE SET
     nickname = EXCLUDED.nickname,
     domain = EXCLUDED.domain,
     actor_type = EXCLUDED.actor_type,
     name = EXCLUDED.name,
     bio = EXCLUDED.bio,
     raw_bio = EXCLUDED.raw_bio,
     avatar_url = EXCLUDED.avatar_url,
     banner_url = EXCLUDED.banner_url,
     profile_url = EXCLUDED.profile_url,
     locked = EXCLUDED.locked,
     discoverable = EXCLUDED.discoverable,
     fields = EXCLUDED.fields,
     inbox = EXCLUDED.inbox,
     shared_inbox = EXCLUDED.shared_inbox,
     outbox = EXCLUDED.outbox,
     followers_url = EXCLUDED.followers_url,
     following_url = EXCLUDED.following_url,
     featured_url = EXCLUDED.featured_url,
     public_key = EXCLUDED.public_key,
     last_fetched_at = now(),
     updated_at = now()
 RETURNING *";

/// The ownership probe behind [`upsert_remote_user`]'s
/// [`ActorError::NicknameConflict`]: the actor id (if any) of a row whose
/// citext nickname is the one the document claims but which is keyed by a
/// *different* `ap_id`. Matching the same row's own id is the ordinary
/// update path, including when the remote renamed `preferredUsername`.
const NICKNAME_HOLDER_SQL: &str =
    "SELECT ap_id FROM users WHERE nickname = $1::citext AND ap_id <> $2";

/// Every way a remote actor document can fail to become a stored user.
#[derive(Debug)]
pub enum ActorError {
    /// The document is not a usable actor: a required field is missing or
    /// malformed, or the `publicKey` names a different actor. The string
    /// names the offending field.
    Invalid(String),
    /// The nickname the document implies (`preferredUsername@host`) is
    /// already stored under a different `ap_id`: the account moved or was
    /// recreated while the old mirror still holds the handle. Pleroma
    /// renames the stale row in this case
    /// (`ActivityPub.maybe_handle_clashing_nickname`); this instance
    /// refuses the upsert instead, so a takeover attempt is visible to the
    /// caller rather than silently rewritten.
    NicknameConflict,
    /// A server-side failure while storing the row: the database calls of
    /// [`upsert_remote_user`].
    Failed(AppError),
}

impl fmt::Display for ActorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(detail) => write!(f, "invalid remote actor: {detail}"),
            Self::NicknameConflict => {
                write!(f, "the nickname is owned by a different actor id")
            }
            Self::Failed(err) => write!(f, "could not store the remote actor: {err}"),
        }
    }
}

impl Error for ActorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Invalid(_) | Self::NicknameConflict => None,
            Self::Failed(err) => Some(err),
        }
    }
}

/// Any [`AppError`] is a server-side [`ActorError::Failed`], so `?` works
/// inside [`upsert_remote_user`].
impl From<AppError> for ActorError {
    fn from(err: AppError) -> Self {
        Self::Failed(err)
    }
}

/// A database failure converts through [`AppError`], keeping the
/// row-not-found mapping of `impl From<SqlxError> for AppError` intact.
impl From<SqlxError> for ActorError {
    fn from(err: SqlxError) -> Self {
        Self::Failed(AppError::from(err))
    }
}

/// A remote actor document parsed into upsert-ready data.
///
/// The field set is what [`parse_actor`] validates and extracts: the
/// [`User`] columns a document can fill, plus the four fields
/// [`upsert_remote_user`] does not store yet ([`also_known_as`](Self::also_known_as),
/// [`moved_to`](Self::moved_to),
/// [`accepts_chat_messages`](Self::accepts_chat_messages),
/// [`published`](Self::published)). The first three have `users` columns
/// since the Pleroma extension migration, `published` has none at all;
/// storing any of them belongs to the alias, move and chat tickets.
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
    /// [`sanitize_html`].
    pub bio: String,
    /// Profile note as source text: the `summary` run through
    /// [`strip_html`].
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
    /// The `alsoKnownAs` entries (the column exists; the upsert does not write it).
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

/// Parses a fetched actor document into the data of a mirror row.
///
/// The required fields are `id` (an absolute `http(s)` URL, kept verbatim),
/// `preferredUsername` (non-empty, only `[A-Za-z0-9_.-]`), `inbox` and
/// `publicKey.publicKeyPem`; a `publicKey` given as an array is read from
/// its first element and a `publicKey.owner` that is present must equal
/// `id`. A missing `type` defaults to [`DEFAULT_ACTOR_TYPE`]; a present one
/// must be one of the five supported kinds. Optional fields take the
/// string-or-array spellings of [`crate::federation::json`] and are dropped
/// when malformed rather than failing the parse.
///
/// # Errors
///
/// [`ActorError::Invalid`] naming the field when a required field is
/// missing or malformed, when `type` is not a supported actor kind, or when
/// `publicKey.owner` names a different actor.
pub fn parse_actor(json: &Value) -> Result<NewRemoteUser, ActorError> {
    let (ap_id, domain) = parse_actor_id(json)?;
    let username = parse_preferred_username(json)?;
    let inbox = required_http_url(json, "inbox")?;
    let public_key = parse_public_key(json, &ap_id)?;
    let summary = string_field(json, "summary");
    Ok(NewRemoteUser {
        nickname: format!("{username}@{domain}"),
        ap_id,
        local: Flag::new(false),
        domain,
        actor_type: parse_actor_type(json)?,
        name: string_field(json, "name"),
        bio: sanitize_html(&summary),
        raw_bio: strip_html(&summary),
        avatar_url: parse_media_url(json, "icon"),
        banner_url: parse_media_url(json, "image"),
        profile_url: json
            .get("url")
            .and_then(first_string)
            .filter(|url| is_http_url(url)),
        locked: bool_field(json, "manuallyApprovesFollowers"),
        discoverable: bool_field(json, "discoverable"),
        fields: parse_fields(json),
        inbox,
        shared_inbox: json
            .get("endpoints")
            .and_then(|endpoints| endpoints.get("sharedInbox"))
            .and_then(value_or_id)
            .filter(|url| is_http_url(url)),
        outbox: optional_http_url(json, "outbox"),
        followers_url: optional_http_url(json, "followers"),
        following_url: optional_http_url(json, "following"),
        featured_url: optional_http_url(json, "featured"),
        public_key,
        also_known_as: parse_also_known_as(json),
        moved_to: optional_http_url(json, "movedTo"),
        accepts_chat_messages: parse_accepts_chat_messages(json),
        published: parse_published(json),
    })
}

/// Stores `new` as this instance's mirror of the actor, exactly once per
/// actor id (decision 7).
///
/// The first fetch inserts a row with a fresh `UUIDv7` (decision 4); later
/// fetches apply to the row with the same `ap_id`, updating the profile
/// fields, keys and inbox endpoints in place and stamping
/// `last_fetched_at = now()`.
///
/// Before writing, the probe `NICKNAME_HOLDER_SQL` checks who owns the
/// nickname: a row other than this actor's own wins
/// [`ActorError::NicknameConflict`] instead of being renamed or overwritten.
///
/// # Errors
///
/// [`ActorError::NicknameConflict`] when another `ap_id` already holds this
/// nickname; [`ActorError::Failed`] for any database failure.
pub async fn upsert_remote_user(pool: &PgPool, new: NewRemoteUser) -> Result<User, ActorError> {
    let holder: Option<String> = sqlx::query_scalar(NICKNAME_HOLDER_SQL)
        .bind(new.nickname.as_str())
        .bind(new.ap_id.as_str())
        .fetch_optional(pool)
        .await?;
    if holder.is_some() {
        return Err(ActorError::NicknameConflict);
    }
    let user = sqlx::query_as::<_, User>(UPSERT_SQL)
        .bind(new_id())
        .bind(new.nickname.as_str())
        .bind(new.ap_id.as_str())
        .bind(new.local)
        .bind(new.domain.as_str())
        .bind(new.actor_type.as_str())
        .bind(new.name.as_str())
        .bind(new.bio.as_str())
        .bind(new.raw_bio.as_str())
        .bind(new.avatar_url.as_deref())
        .bind(new.banner_url.as_deref())
        .bind(new.profile_url.as_deref())
        .bind(new.locked)
        .bind(new.discoverable)
        .bind(new.fields)
        .bind(new.inbox.as_str())
        .bind(new.shared_inbox.as_deref())
        .bind(new.outbox.as_deref())
        .bind(new.followers_url.as_deref())
        .bind(new.following_url.as_deref())
        .bind(new.featured_url.as_deref())
        .bind(new.public_key.as_str())
        .fetch_one(pool)
        .await?;
    Ok(user)
}

/// The document's `id` and the host parsed out of it, as the `(ap_id,
/// domain)` pair [`NewRemoteUser`] starts from.
///
/// The `id` is a plain string (not a reference object): actor documents
/// name themselves. It is stored verbatim because fediverse ids are
/// compared byte-wise everywhere they resurface (signature `keyId`s,
/// activity `actor` fields), while the host is taken from the parsed URL,
/// so `Mastodon.Example` below mirrors as `mastodon.example`.
fn parse_actor_id(json: &Value) -> Result<(String, String), ActorError> {
    let raw = json
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ActorError::Invalid("`id` is missing or not a string".to_owned()))?;
    let url = Url::parse(raw).map_err(|_parse_err| {
        ActorError::Invalid("`id` is not an absolute http(s) URL".to_owned())
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ActorError::Invalid("`id` is not an http(s) URL".to_owned()));
    }
    let host = url
        .host_str()
        .ok_or_else(|| ActorError::Invalid("`id` has no host".to_owned()))?;
    Ok((raw.to_owned(), host.to_owned()))
}

/// The `preferredUsername`: required, non-empty, and limited to the handle
/// alphabet `[A-Za-z0-9_.-]` — anything else cannot form the
/// `user@host` nickname of decision 7 (Pleroma rejects the same shapes in
/// its user validations).
fn parse_preferred_username(json: &Value) -> Result<&str, ActorError> {
    json.get("preferredUsername")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty() && name.chars().all(is_username_char))
        .ok_or_else(|| {
            ActorError::Invalid(
                "`preferredUsername` is missing, empty, or uses characters outside \
                 [A-Za-z0-9_.-]"
                    .to_owned(),
            )
        })
}

/// One character of the handle alphabet [`parse_preferred_username`]
/// accepts: ASCII alphanumerics plus `.`, `_` and `-`.
const fn is_username_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// The actor `type`, validated against [`ACTOR_TYPES`]; a missing (or
/// `null`) type defaults to [`DEFAULT_ACTOR_TYPE`] the way Pleroma's
/// `object_to_user_data` does. An unknown type means the document is not
/// an actor this instance knows how to mirror.
fn parse_actor_type(json: &Value) -> Result<String, ActorError> {
    match json.get("type") {
        None | Some(Value::Null) => Ok(DEFAULT_ACTOR_TYPE.to_owned()),
        Some(Value::String(kind)) if ACTOR_TYPES.contains(&kind.as_str()) => Ok(kind.clone()),
        Some(_) => Err(ActorError::Invalid(
            "`type` is not a supported actor type".to_owned(),
        )),
    }
}

/// The signing key of `publicKey`: the field is required, may be given as
/// an array (Pleroma has emitted `[key]`), its `publicKeyPem` must be a
/// non-empty string, and a `publicKey.owner` that is present must equal
/// `id` — the key only stands for the actor whose id it names. An absent
/// `owner` is tolerated; Pleroma's `object_to_user_data` never reads the
/// field.
fn parse_public_key(json: &Value, id: &str) -> Result<String, ActorError> {
    let key = json
        .get("publicKey")
        .and_then(|public_key| as_array(public_key).first())
        .ok_or_else(|| ActorError::Invalid("`publicKey` is missing".to_owned()))?;
    if key
        .get("owner")
        .and_then(Value::as_str)
        .is_some_and(|owner| owner != id)
    {
        return Err(ActorError::Invalid(
            "`publicKey.owner` does not match the actor id".to_owned(),
        ));
    }
    key.get("publicKeyPem")
        .and_then(Value::as_str)
        .filter(|pem| !pem.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ActorError::Invalid("`publicKey.publicKeyPem` is missing".to_owned()))
}

/// The required URL of a field (`inbox`): a string or reference object
/// spelling an absolute `http(s)` URL, via [`value_or_id`].
fn required_http_url(json: &Value, field: &str) -> Result<String, ActorError> {
    optional_http_url(json, field)
        .ok_or_else(|| ActorError::Invalid(format!("`{field}` is missing or not an http(s) URL")))
}

/// An optional URL field (`outbox`, `followers`, `following`, `featured`,
/// `movedTo`): [`value_or_id`] reads strings, `{"id": ...}` collection
/// objects and one-or-many arrays; a value that carries no `http(s)` URL is
/// dropped rather than rejected.
fn optional_http_url(json: &Value, field: &str) -> Option<String> {
    json.get(field)
        .and_then(value_or_id)
        .filter(|url| is_http_url(url))
}

/// True when `text` parses as an absolute URL whose scheme is `http` or
/// `https` and that has a host. The raw text, not the re-rendered URL, is
/// what callers store.
fn is_http_url(text: &str) -> bool {
    Url::parse(text)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
}

/// A text field (`name`, `summary`): the string under `field`, empty for
/// any other shape.
fn string_field(json: &Value, field: &str) -> String {
    json.get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// A boolean flag field (`manuallyApprovesFollowers`, `discoverable`):
/// `true`/`false` as sent, and `false` for anything else, Pleroma's
/// `data["key"] || false`.
fn bool_field(json: &Value, field: &str) -> Flag {
    Flag::new(json.get(field).and_then(Value::as_bool).unwrap_or_default())
}

/// The `icon`/`image` URL of `field`: the value may be a bare URL string,
/// an `Image` object whose `url` is a string, a `Link` object or an array
/// of either (all three spellings are on the wire), or an array of any of
/// those; the first entry yielding an `http(s)` URL wins, anything else is
/// dropped.
fn parse_media_url(json: &Value, field: &str) -> Option<String> {
    let value = json.get(field)?;
    as_array(value).iter().find_map(|element| {
        let url = element
            .as_object()
            .and_then(|object| object.get("url"))
            .map_or_else(|| first_string(element), first_string);
        url.filter(|url| is_http_url(url))
    })
}

/// The `attachment` list as the stored `fields` value: a JSON array of the
/// `{name, value}` objects of every `PropertyValue` entry, in order
/// (Pleroma's `Map.take(fields, ["name", "value"])`). Entries of any other
/// shape are skipped.
fn parse_fields(json: &Value) -> Value {
    let properties = json
        .get("attachment")
        .map(as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(property_value)
        .collect();
    Value::Array(properties)
}

/// One `attachment` entry as a stored `{name, value}` field, `None` unless
/// it is a `PropertyValue` object carrying both as strings.
fn property_value(entry: &Value) -> Option<Value> {
    if entry.get("type").and_then(Value::as_str) != Some(PROPERTY_VALUE_TYPE) {
        return None;
    }
    let name = entry.get("name").and_then(Value::as_str)?;
    let value = entry.get("value").and_then(Value::as_str)?;
    Some(json!({ "name": name, "value": value }))
}

/// The `alsoKnownAs` aliases as strings: a single string or an array,
/// `[]` when absent or of another shape (Pleroma's
/// `normalize_also_known_as`); non-string entries are skipped.
fn parse_also_known_as(json: &Value) -> Vec<String> {
    json.get("alsoKnownAs")
        .map(as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// `capabilities.acceptsChatMessages` (Pleroma's `litepub:capabilities`
/// object): `true` only when the nested flag literally says so.
fn parse_accepts_chat_messages(json: &Value) -> Flag {
    bool_at(json, "capabilities", "acceptsChatMessages")
}

/// The boolean under `key` inside the object under `parent`, `false` when
/// either layer is absent or the value is not a boolean.
fn bool_at(json: &Value, parent: &str, key: &str) -> Flag {
    Flag::new(
        json.get(parent)
            .and_then(|object| object.get(key))
            .and_then(Value::as_bool)
            .unwrap_or_default(),
    )
}

/// The actor's `published` creation time as an RFC 3339 timestamp
/// ([`parse_time`] handles the `Z` and `+hh:mm` spellings); `None` when
/// absent or unparseable.
fn parse_published(json: &Value) -> Option<DateTime<Utc>> {
    json.get("published")
        .and_then(Value::as_str)
        .and_then(parse_time)
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::models::user::{
        CountDelta, NewUser, get_by_ap_id, get_by_id, get_by_nickname, insert, update_counts,
    };

    /// The Mastodon-style fixture: every field in its common single-value
    /// spelling, plus a hostile `summary` for the sanitizer.
    const MASTODON: &str = include_str!("../../tests/fixtures/mastodon_actor.json");
    /// The Pleroma-style fixture: the array and object spellings —
    /// `publicKey` and `icon` as arrays, `url` as `Link` objects, `outbox`
    /// as a collection object, `image` as a bare string.
    const PLEROMA: &str = include_str!("../../tests/fixtures/pleroma_actor.json");

    /// Parses fixture text into JSON; broken fixture files fail the test.
    fn fixture(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    /// Removes `field` from the document's top-level object.
    fn remove(document: &mut Value, field: &str) {
        let _gone = document.as_object_mut().unwrap().remove(field);
    }

    #[test]
    fn parses_a_mastodon_style_actor() {
        let document = fixture(MASTODON);
        let parsed = parse_actor(&document).unwrap();
        assert_eq!(
            parsed.nickname, "alice@mastodon.example",
            "preferredUsername@host of id"
        );
        assert_eq!(
            parsed.ap_id, "https://mastodon.example/users/alice",
            "ap_id"
        );
        assert!(!parsed.local.get(), "a remote mirror is never local");
        assert_eq!(parsed.domain, "mastodon.example", "the id's host");
        assert_eq!(parsed.actor_type, "Person", "the actor type");
        assert_eq!(parsed.name, "Alice \u{1f98a}", "the display name");
        assert!(
            parsed.bio.contains(
                r#"<a href="https://alice.example" rel="ugc nofollow noopener noreferrer">"#
            ),
            "the link survives with the forced rel, got {}",
            parsed.bio
        );
        assert!(!parsed.bio.contains("target="), "target is stripped");
        assert!(
            !parsed.bio.contains("steal"),
            "script contents are stripped"
        );
        assert_eq!(
            parsed.raw_bio, "A curator of https://alice.example/feed\nBio & notes \n",
            "the source note is the stripped text with p-ends as newlines"
        );
        assert!(!parsed.locked.get(), "manuallyApprovesFollowers: false");
        assert!(parsed.discoverable.get(), "discoverable: true");
        assert_eq!(
            parsed.public_key,
            document["publicKey"]["publicKeyPem"].as_str().unwrap(),
            "the PEM as sent"
        );
        assert_eq!(
            parsed.also_known_as,
            ["https://old.example/users/alice"],
            "alsoKnownAs"
        );
        assert_eq!(parsed.moved_to, None, "no movedTo in the document");
        assert!(
            !parsed.accepts_chat_messages.get(),
            "no capabilities object"
        );
        assert_eq!(
            parsed.published,
            Some(parse_time("2022-01-31T12:34:56Z").unwrap()),
            "published"
        );
    }

    #[test]
    fn a_mastodon_actor_fills_the_endpoint_and_media_columns() {
        let parsed = parse_actor(&fixture(MASTODON)).unwrap();
        assert_eq!(
            parsed.avatar_url.as_deref(),
            Some("https://mastodon.example/avatar/alice.png"),
            "icon object url"
        );
        assert_eq!(
            parsed.banner_url.as_deref(),
            Some("https://mastodon.example/banner/alice.png"),
            "image object url"
        );
        assert_eq!(
            parsed.profile_url.as_deref(),
            Some("https://mastodon.example/@alice"),
            "url"
        );
        assert_eq!(
            parsed.inbox, "https://mastodon.example/users/alice/inbox",
            "inbox"
        );
        assert_eq!(
            parsed.shared_inbox.as_deref(),
            Some("https://mastodon.example/inbox"),
            "endpoints.sharedInbox"
        );
        assert_eq!(
            parsed.outbox.as_deref(),
            Some("https://mastodon.example/users/alice/outbox"),
            "outbox"
        );
        assert_eq!(
            parsed.followers_url.as_deref(),
            Some("https://mastodon.example/users/alice/followers"),
            "followers"
        );
        assert_eq!(
            parsed.following_url.as_deref(),
            Some("https://mastodon.example/users/alice/following"),
            "following"
        );
        assert_eq!(
            parsed.featured_url.as_deref(),
            Some("https://mastodon.example/users/alice/collections/featured"),
            "featured"
        );
        assert_eq!(
            parsed.fields,
            json!([
                {"name": "Site", "value": "<a href=\"https://alice.example\" rel=\"nofollow noopener noreferrer\" target=\"_blank\">alice.example</a>"},
                {"name": "Pronouns", "value": "she/her"},
            ]),
            "the two PropertyValues with name and value only; the typeless entry skipped"
        );
    }

    #[test]
    fn parses_a_pleroma_style_actor() {
        let document = fixture(PLEROMA);
        let parsed = parse_actor(&document).unwrap();
        assert_eq!(
            parsed.nickname, "bob.the-bot@pleroma.example",
            "dots are legal in the handle alphabet"
        );
        assert_eq!(
            parsed.ap_id, "https://pleroma.example/users/bob.the-bot",
            "the id is kept verbatim"
        );
        assert_eq!(parsed.domain, "pleroma.example", "the id's host");
        assert_eq!(parsed.actor_type, "Service", "the bot actor type");
        assert!(parsed.locked.get(), "manuallyApprovesFollowers: true");
        assert!(
            !parsed.discoverable.get(),
            "an absent discoverable defaults to false"
        );
        assert_eq!(
            parsed.profile_url.as_deref(),
            Some("https://pleroma.example/users/bob.the-bot"),
            "url given as an array of Link objects yields the first href"
        );
        assert_eq!(
            parsed.outbox.as_deref(),
            Some("https://pleroma.example/users/bob.the-bot/outbox"),
            "outbox given as the collection object yields its id"
        );
        assert_eq!(
            parsed.avatar_url.as_deref(),
            Some("https://pleroma.example/images/bob/avatar.png"),
            "icon: array -> Image object -> url array -> Link href"
        );
        assert_eq!(
            parsed.banner_url.as_deref(),
            Some("https://pleroma.example/images/bob/banner.png"),
            "image given as a bare string"
        );
        assert_eq!(
            parsed.public_key,
            document["publicKey"][0]["publicKeyPem"].as_str().unwrap(),
            "publicKey given as an array"
        );
        assert!(
            parsed.bio.contains(r#"<span class="h-card">"#),
            "h-card is an allowed span class, got {}",
            parsed.bio
        );
        assert!(!parsed.bio.contains("alert"), "the script body is gone");
        assert!(!parsed.bio.contains("p{}"), "the style body is gone");
        assert_eq!(
            parsed.raw_bio, "Hi @alice\nbot here\nbye\n",
            "the note as text"
        );
        assert_eq!(
            parsed.fields,
            json!([{"name": "web", "value": "https://bob.example"}]),
            "only the PropertyValue entry is kept"
        );
        assert_eq!(
            parsed.also_known_as,
            ["https://old.example/users/bob"],
            "alsoKnownAs given as a single string"
        );
        assert_eq!(
            parsed.moved_to.as_deref(),
            Some("https://new.example/users/bob.the-bot"),
            "movedTo"
        );
        assert!(parsed.accepts_chat_messages.get(), "capabilities flag");
        assert_eq!(
            parsed.published,
            Some(parse_time("2019-06-01T00:00:00.000Z").unwrap()),
            "the published timestamp parses"
        );
    }

    #[test]
    fn a_minimal_actor_gets_the_documented_defaults() {
        let document = json!({
            "id": "https://remote.example/users/carol",
            "preferredUsername": "carol",
            "inbox": "https://remote.example/users/carol/inbox",
            "publicKey": {
                "owner": "https://remote.example/users/carol",
                "publicKeyPem": "PEM-without-an-owner",
            },
        });
        let parsed = parse_actor(&document).unwrap();
        assert_eq!(
            parsed.nickname, "carol@remote.example",
            "the handle is username@host"
        );
        assert_eq!(
            parsed.actor_type, DEFAULT_ACTOR_TYPE,
            "a missing type is Person"
        );
        assert_eq!(parsed.name, "", "no display name");
        assert_eq!(parsed.bio, "", "no note");
        assert_eq!(parsed.raw_bio, "", "no source note");
        assert_eq!(
            parsed.public_key, "PEM-without-an-owner",
            "owner may be absent"
        );
        assert_eq!(parsed.fields, json!([]), "no attachments");
        assert!(parsed.also_known_as.is_empty(), "no aliases");
        assert_eq!(parsed.published, None, "no creation time");
        for optional in [
            parsed.avatar_url,
            parsed.banner_url,
            parsed.profile_url,
            parsed.shared_inbox,
            parsed.outbox,
            parsed.followers_url,
            parsed.following_url,
            parsed.featured_url,
            parsed.moved_to,
        ] {
            assert_eq!(optional, None, "absent optional fields are None");
        }
        assert!(
            !parsed.locked.get() && !parsed.discoverable.get(),
            "flags default to false"
        );
        assert!(
            !parsed.accepts_chat_messages.get(),
            "no capabilities object"
        );
        assert!(!parsed.local.get(), "local is always false");
    }

    /// One rejection case: what to call it, how to break a copy of the
    /// Mastodon fixture, and a fragment of the field name the error must
    /// carry.
    type Rejection = (&'static str, fn(&mut Value), &'static str);

    /// Documents built from the Mastodon fixture by breaking exactly one
    /// required rule each; none of them may parse.
    fn rejections() -> Vec<Rejection> {
        vec![
            ("no id at all", |doc| remove(doc, "id"), "`id`"),
            (
                "relative id",
                |doc| doc["id"] = json!("/users/alice"),
                "`id`",
            ),
            (
                "non-http id",
                |doc| doc["id"] = json!("ftp://mastodon.example/users/alice"),
                "`id`",
            ),
            (
                "array id",
                |doc| doc["id"] = json!(["https://mastodon.example/users/alice"]),
                "`id`",
            ),
            ("no inbox", |doc| remove(doc, "inbox"), "`inbox`"),
            (
                "mailto inbox",
                |doc| doc["inbox"] = json!("mailto:alice@mastodon.example"),
                "`inbox`",
            ),
            (
                "no username",
                |doc| remove(doc, "preferredUsername"),
                "preferredUsername",
            ),
            (
                "empty username",
                |doc| doc["preferredUsername"] = json!(""),
                "preferredUsername",
            ),
            (
                "username with a space",
                |doc| doc["preferredUsername"] = json!("alice smith"),
                "preferredUsername",
            ),
            (
                "username with an at-sign",
                |doc| doc["preferredUsername"] = json!("alice@mastodon.example"),
                "preferredUsername",
            ),
            (
                "non-ascii username",
                |doc| doc["preferredUsername"] = json!("müller"),
                "preferredUsername",
            ),
            (
                "unknown actor type",
                |doc| doc["type"] = json!("Spaceship"),
                "`type`",
            ),
            ("non-string type", |doc| doc["type"] = json!(7), "`type`"),
            (
                "no publicKey",
                |doc| remove(doc, "publicKey"),
                "`publicKey`",
            ),
            (
                "empty publicKey array",
                |doc| doc["publicKey"] = json!([]),
                "`publicKey`",
            ),
            (
                "no PEM",
                |doc| remove(&mut doc["publicKey"], "publicKeyPem"),
                "publicKeyPem",
            ),
            (
                "empty PEM",
                |doc| doc["publicKey"]["publicKeyPem"] = json!(""),
                "publicKeyPem",
            ),
            (
                "owner names another actor",
                |doc| doc["publicKey"]["owner"] = json!("https://evil.example/users/alice"),
                "owner",
            ),
        ]
    }

    #[test]
    fn documents_that_are_not_actors_are_rejected() {
        for (name, break_it, fragment) in rejections() {
            let mut document = fixture(MASTODON);
            break_it(&mut document);
            let error = parse_actor(&document).unwrap_err();
            let ActorError::Invalid(detail) = error else {
                panic!("{name}: must fail as Invalid, not {error:?}");
            };
            assert!(
                detail.contains(fragment),
                "{name}: the message `{detail}` must name {fragment}"
            );
        }
    }

    #[test]
    fn every_supported_actor_type_round_trips() {
        for kind in ACTOR_TYPES {
            let mut document = fixture(MASTODON);
            document["type"] = json!(kind);
            let parsed = parse_actor(&document).unwrap();
            assert_eq!(parsed.actor_type, kind, "{kind} is accepted verbatim");
        }
    }

    #[test]
    fn the_summary_is_sanitized_and_the_source_kept() {
        let mut document = fixture(MASTODON);
        document["summary"] = json!(
            r#"<p onclick="x()">Hello <script>steal()</script>there <a href="javascript:alert(1)">x</a>!</p>"#
        );
        let parsed = parse_actor(&document).unwrap();
        assert_eq!(
            parsed.bio, r#"<p>Hello there <a rel="ugc nofollow noopener noreferrer">x</a>!</p>"#,
            "onclick, the script and the javascript: href are gone"
        );
        assert_eq!(parsed.raw_bio, "Hello there x!\n", "the note as plain text");
    }

    #[test]
    fn media_fields_take_a_string_an_object_or_an_array() {
        let mut document = fixture(MASTODON);
        document["icon"] = json!("https://cdn.example/a.png");
        assert_eq!(
            parse_actor(&document).unwrap().avatar_url.as_deref(),
            Some("https://cdn.example/a.png"),
            "a bare string icon"
        );
        document["icon"] = json!([Value::Null, {"url": "https://cdn.example/b.png"}]);
        assert_eq!(
            parse_actor(&document).unwrap().avatar_url.as_deref(),
            Some("https://cdn.example/b.png"),
            "an array icon whose first entry is filler"
        );
        document["image"] =
            json!({"type": "Image", "url": [{"href": "https://cdn.example/c.png"}]});
        assert_eq!(
            parse_actor(&document).unwrap().banner_url.as_deref(),
            Some("https://cdn.example/c.png"),
            "an object image with a url array of Links"
        );
    }

    #[test]
    fn malformed_optional_fields_are_dropped_not_rejected() {
        let mut document = fixture(MASTODON);
        document["outbox"] = json!(7);
        document["followers"] = json!("not a url");
        document["url"] = json!({"nothing": true});
        document["icon"] = json!({"url": []});
        document["endpoints"] = json!("sharedInbox");
        document["discoverable"] = json!("yes");
        document["alsoKnownAs"] = json!(42);
        document["movedTo"] = json!(["who"]);
        document["published"] = json!("yesterday");
        let parsed = parse_actor(&document).unwrap();
        assert_eq!(parsed.outbox, None, "a number outbox");
        assert_eq!(parsed.followers_url, None, "a relative followers");
        assert_eq!(parsed.profile_url, None, "a url with no reference");
        assert_eq!(parsed.avatar_url, None, "an icon with no usable url");
        assert_eq!(parsed.shared_inbox, None, "endpoints not an object");
        assert!(!parsed.discoverable.get(), "a string is not the flag");
        assert!(parsed.also_known_as.is_empty(), "a number alias list");
        assert_eq!(parsed.moved_to, None, "a non-url move target");
        assert_eq!(parsed.published, None, "an unparseable timestamp");
    }

    #[test]
    fn endpoints_on_other_hosts_are_accepted_without_containment() {
        let mut document = fixture(MASTODON);
        document["inbox"] = json!("https://relay.example/inbox/alice");
        document["outbox"] = json!("https://other.example/outbox/alice");
        let parsed = parse_actor(&document).unwrap();
        assert_eq!(
            parsed.inbox, "https://relay.example/inbox/alice",
            "inbox host may differ"
        );
        assert_eq!(
            parsed.outbox.as_deref(),
            Some("https://other.example/outbox/alice"),
            "outbox host may differ"
        );
        assert_eq!(
            parsed.domain, "mastodon.example",
            "the domain is the id's host"
        );
        assert_eq!(
            parsed.nickname, "alice@mastodon.example",
            "and so the nickname's"
        );
    }

    #[test]
    fn ids_are_stored_verbatim_while_hosts_are_lowercased() {
        let mut document = fixture(MASTODON);
        let mixed = "https://Mastodon.Example/users/alice";
        document["id"] = json!(mixed);
        document["publicKey"]["owner"] = json!(mixed);
        let parsed = parse_actor(&document).unwrap();
        assert_eq!(parsed.ap_id, mixed, "the id bytes the remote sent");
        assert_eq!(
            parsed.domain, "mastodon.example",
            "the host comes back lowercased"
        );
        assert_eq!(parsed.nickname, "alice@mastodon.example");
    }

    #[test]
    fn non_object_documents_are_rejected() {
        for document in [
            json!("https://x.example/users/a"),
            json!([]),
            json!(null),
            json!(1),
        ] {
            let error = parse_actor(&document).unwrap_err();
            assert!(
                matches!(error, ActorError::Invalid(_)),
                "a document that is not an object fails Invalid, got {error:?}"
            );
        }
    }

    #[test]
    fn actor_errors_render_their_causes() {
        assert_eq!(
            ActorError::Invalid("`id` is missing".to_owned()).to_string(),
            "invalid remote actor: `id` is missing",
            "Invalid shows its detail"
        );
        assert_eq!(
            ActorError::NicknameConflict.to_string(),
            "the nickname is owned by a different actor id"
        );
        let failed = ActorError::from(SqlxError::PoolClosed);
        let message = failed.to_string();
        assert!(
            message.starts_with("could not store the remote actor: "),
            "Failed prefixes its cause, got {message}"
        );
        assert!(
            message.contains("closed pool"),
            "the Display of the wrapped AppError shows through, got {message}"
        );
        assert!(Error::source(&failed).is_some(), "Failed keeps the source");
        assert!(
            Error::source(&ActorError::NicknameConflict).is_none(),
            "a plain conflict has no source"
        );
    }

    /// The mirror row the first fetch of a Mastodon actor stores: every
    /// column read off the document, and none of the secrets a local account
    /// has.
    fn assert_mirror_of(first: &User, document: &Value) {
        assert_eq!(
            first.nickname, "alice@mastodon.example",
            "the parsed handle"
        );
        assert_eq!(
            first.ap_id, "https://mastodon.example/users/alice",
            "the parsed id"
        );
        assert!(!first.local.get(), "a mirror row is not local");
        assert_eq!(
            first.domain.as_deref(),
            Some("mastodon.example"),
            "the host"
        );
        assert!(
            first.private_key.is_none() && first.email.is_none() && first.password_hash.is_none(),
            "a remote row carries no local secrets"
        );
        assert_eq!(
            first.public_key.as_deref(),
            document["publicKey"]["publicKeyPem"].as_str(),
            "the key is stored"
        );
        assert_eq!(
            first.inbox.as_deref(),
            Some("https://mastodon.example/users/alice/inbox"),
            "the inbox endpoint"
        );
        assert_eq!(
            first.shared_inbox.as_deref(),
            Some("https://mastodon.example/inbox"),
            "the shared inbox endpoint"
        );
        assert!(first.last_fetched_at.is_some(), "the fetch is stamped");
        assert!(
            first.discoverable.get() && !first.locked.get(),
            "the flags moved"
        );
    }

    #[tokio::test]
    async fn upsert_inserts_then_updates_the_same_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut document = fixture(MASTODON);
        let first = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();
        assert_mirror_of(&first, &document);

        // Counters move through update_counts only: a re-fetch must not
        // reset them (nor touch the login/role columns, absent here).
        update_counts(
            &pool,
            first.id,
            CountDelta {
                followers: 5,
                following: 0,
                statuses: 0,
            },
        )
        .await
        .unwrap();

        document["name"] = json!("Alice the Second");
        document["summary"] = json!("<p>new <b>bio</b></p>");
        let second = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();
        assert_eq!(
            second.id, first.id,
            "the existing row is updated, not replaced"
        );
        assert_eq!(second.name, "Alice the Second", "the new name landed");
        assert_eq!(second.bio, "<p>new <b>bio</b></p>", "the new note landed");
        assert_eq!(second.raw_bio, "new bio\n", "with its source");
        assert_eq!(second.followers_count, 5, "the counter survived the update");
        assert!(
            second.updated_at >= first.inserted_at,
            "updated_at refreshed"
        );

        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows.0, 1, "upserting twice leaves exactly one row");
        let by_nickname = get_by_nickname(&pool, "alice@mastodon.example")
            .await
            .unwrap()
            .expect("the handle finds the mirror");
        assert_eq!(by_nickname.id, first.id, "one row, both lookups");
    }

    #[tokio::test]
    async fn upsert_follows_a_rename_of_the_same_actor() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut document = fixture(MASTODON);
        let first = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();
        document["preferredUsername"] = json!("alice2");
        let renamed = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();
        assert_eq!(renamed.id, first.id, "a rename updates, it does not insert");
        assert_eq!(
            renamed.nickname, "alice2@mastodon.example",
            "the handle follows"
        );
        assert!(
            get_by_nickname(&pool, "alice@mastodon.example")
                .await
                .unwrap()
                .is_none(),
            "the old handle is free again"
        );
    }

    #[tokio::test]
    async fn upsert_reports_a_nickname_conflict() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let squatter = insert(
            &pool,
            &NewUser::local(
                "alice@mastodon.example",
                "https://other.example/users/alice",
            ),
        )
        .await
        .unwrap();
        let error = upsert_remote_user(&pool, parse_actor(&fixture(MASTODON)).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(error, ActorError::NicknameConflict),
            "another ap_id owns the handle: {error:?}"
        );
        assert!(
            get_by_ap_id(&pool, "https://mastodon.example/users/alice")
                .await
                .unwrap()
                .is_none(),
            "the mirror was not stored"
        );
        let still = get_by_id(&pool, squatter.id)
            .await
            .unwrap()
            .expect("the squatter stands");
        assert_eq!(
            still.ap_id, "https://other.example/users/alice",
            "and was not renamed"
        );
    }

    #[tokio::test]
    async fn upsert_keeps_distinct_actors_distinct() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = upsert_remote_user(&pool, parse_actor(&fixture(MASTODON)).unwrap())
            .await
            .unwrap();
        let bob = upsert_remote_user(&pool, parse_actor(&fixture(PLEROMA)).unwrap())
            .await
            .unwrap();
        assert_ne!(alice.id, bob.id, "two actors, two rows");
        assert_eq!(bob.actor_type, "Service", "each keeps its document");
        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows.0, 2, "and nothing else was inserted");
    }
}
