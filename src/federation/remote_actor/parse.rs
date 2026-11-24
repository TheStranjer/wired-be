//! [`parse_actor`]: validating the required fields of an actor document and
//! assembling the [`NewRemoteUser`] from them.

use serde_json::Value;
use url::Url;

use super::{
    ACTOR_TYPES, ActorError, MAX_REMOTE_BIO_CHARS, MAX_REMOTE_NAME_CHARS, MAX_USERNAME_CHARS,
    NewRemoteUser,
    fields::{
        bool_field, optional_http_url, parse_accepts_chat_messages, parse_also_known_as,
        parse_fields, parse_media_url, parse_published, required_http_url, string_field,
    },
    truncate_chars,
};
use crate::{
    federation::json::{as_array, first_string, value_or_id},
    models::user::{DEFAULT_ACTOR_TYPE, Flag},
    util::{
        keys::normalize_pem,
        sanitize::{sanitize_html, strip_html},
        url::{host_with_port, http_host, is_http_url, parse_http_url},
    },
};

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
    let public_key_id = parse_public_key_id(json, &ap_id);
    let summary = string_field(json, "summary");
    let summary = truncate_chars(&summary, MAX_REMOTE_BIO_CHARS);
    let name = string_field(json, "name");
    Ok(NewRemoteUser {
        nickname: format!("{username}@{domain}"),
        ap_id,
        local: Flag::new(false),
        domain,
        actor_type: parse_actor_type(json)?,
        name: truncate_chars(&name, MAX_REMOTE_NAME_CHARS).to_owned(),
        bio: sanitize_html(summary),
        raw_bio: strip_html(summary),
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
        public_key_id,
        also_known_as: parse_also_known_as(json),
        moved_to: optional_http_url(json, "movedTo"),
        accepts_chat_messages: parse_accepts_chat_messages(json),
        published: parse_published(json),
    })
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
    // The shared check decides; the checks below only pick which message
    // names the failing part.
    if parse_http_url(raw).is_none() {
        let message = if matches!(url.scheme(), "http" | "https") {
            "`id` has no host"
        } else {
            "`id` is not an http(s) URL"
        };
        return Err(ActorError::Invalid(message.to_owned()));
    }
    // A non-default port belongs to the host the handle names: Mastodon's
    // `Addressable` domain keeps it, and decision 7's `user@host` nickname
    // has to be the handle `WebFinger` was asked for — which for a peer on
    // an explicit port carries that port. `host_with_port` appends only a
    // real, non-default port.
    Ok((raw.to_owned(), host_with_port(&url)))
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
        .and_then(|name| {
            if name.chars().count() > MAX_USERNAME_CHARS {
                Err(ActorError::Invalid(format!(
                    "`preferredUsername` is longer than {MAX_USERNAME_CHARS} characters"
                )))
            } else {
                Ok(name)
            }
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

/// The signing key of `publicKey`, normalized for storage.
///
/// The field is required and may be given as an array (Pleroma has emitted
/// `[key]`). Its `publicKeyPem` must be a non-blank string and is returned
/// through [`normalize_pem`], so the stored form ends in exactly one newline
/// even when the peer sends Pleroma's `\n\n`. A `publicKey.owner` that is
/// present must equal `id` — the key only stands for the actor whose id it
/// names. An absent
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
        .filter(|pem| !pem.trim().is_empty())
        .map(normalize_pem)
        .ok_or_else(|| ActorError::Invalid("`publicKey.publicKeyPem` is missing".to_owned()))
}

/// The `publicKey.id`, kept only when it is an `http(s)` URL on the actor's host.
///
/// The id may be a string or a reference object, read from the first element
/// of an array `publicKey`. It is optional and never fatal: a missing,
/// malformed or foreign-host id is dropped, and the signer is then found by
/// the actor URL the `keyId` strips to.
fn parse_public_key_id(json: &Value, id: &str) -> Option<String> {
    let key_id = json
        .get("publicKey")
        .and_then(|public_key| as_array(public_key).first())
        .and_then(|key| key.get("id"))
        .and_then(value_or_id)
        .filter(|key_id| is_http_url(key_id))?;
    let actor_host = http_host(id)?;
    (http_host(&key_id)? == actor_host).then_some(key_id)
}

#[cfg(test)]
mod tests;
