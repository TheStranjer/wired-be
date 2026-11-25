//! Inbound `Listen` (+ its `Undo`): what a peer announcing what one of its
//! accounts is listening to does to this instance.
//!
//! The reference is Pleroma's `Listen` activity
//! (`Pleroma.Web.ActivityPub.ActivityPub.listen/1` and `ActivityDraft.listen/2`),
//! an activity wrapping an `Audio` object that carries the media metadata,
//! the document [`crate::federation::activities::listen_json`] renders for
//! this instance's own scrobbles. The ticket rules are authoritative where
//! the two differ:
//!
//! * [`handle_listen`] is the route of the `Listen` type. The signer must be
//!   a remote account (containment has already proved it is the activity's
//!   `actor`); the object is the embedded `Audio`, or — when it is a bare
//!   id — the document [`fetch_object_json`] dereferences under the
//!   per-activity fetch cap, ignored when the fetch fails. The `Audio`'s
//!   `actor` / `attributedTo` must name the signer (a fetched document must
//!   name it at least once), its `type` must be `Audio`, its `title` a
//!   non-blank string (truncated, like `artist` and `album`, to the limits
//!   the local API enforces), its `length` a non-negative 32-bit integer or
//!   absent and its `externalLink` an `http(s)` URL within the local limit or
//!   absent. Anything else is ignored.
//! * The visibility is `note_parser::parse_visibility` over the activity's
//!   own `to` / `cc`, the listen time is the `published` stamp clamped like a
//!   note's ([`json::clamped_time`]) and the row is stored under the
//!   `Listen`'s id ([`scrobble::insert_remote`]), a unique key, so a
//!   redelivery stores nothing more.
//! * [`handle_undo_listen`] is the `Undo` route: it removes the signer's own
//!   scrobble of the withdrawn `Listen`'s id and nothing else. The `Delete`
//!   of the account itself needs no route of its own: the `scrobbles` rows
//!   cascade with their user.

use chrono::Utc;
use futures::future::BoxFuture;
use serde_json::Value;
use tracing::debug;

use crate::{
    federation::{
        dispatch::{
            ActivityView, FederationError, Handlers, ObjectRef, Outcome, TransientExt as _,
        },
        fetch_object::fetch_object_json,
        json, note_parser,
    },
    models::{
        scrobble::{self, MAX_LINK_CHARS, MAX_TEXT_CHARS, MAX_TITLE_CHARS, NewScrobble},
        user::User,
    },
    state::AppState,
    util::url::is_http_url,
};

/// The activity type of a scrobble.
const TYPE_LISTEN: &str = "Listen";
/// The object type of a scrobble's media.
const TYPE_AUDIO: &str = "Audio";

/// Folds this module's routes into the dispatcher's registry: the `Listen`
/// by its own name and the `Undo` of a `Listen`.
///
/// `crate::federation::handlers::registry` calls it, which is the whole
/// composition of the live registry.
pub fn register(handlers: &mut Handlers) {
    handlers.register(TYPE_LISTEN, handle_listen);
    handlers.register_undo(TYPE_LISTEN, handle_undo_listen);
}

/// The `Listen` handler: stores one remote scrobble.
///
/// # Errors
///
/// [`FederationError::Transient`] for a database failure the retry may
/// outlive; every refusal of shape or standing arrives as
/// [`Outcome::Ignored`].
pub fn handle_listen(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_listen(state, view, signer).await })
}

/// The `Undo` of a `Listen`: removes the signer's scrobble of that `Listen`.
///
/// # Errors
///
/// [`FederationError::Transient`] for a database failure the retry may
/// outlive.
pub fn handle_undo_listen(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_undo(&state, &view, &signer).await })
}

/// The body of [`handle_undo_listen`].
async fn apply_undo(
    state: &AppState,
    view: &ActivityView,
    signer: &User,
) -> Result<Outcome, FederationError> {
    let Some(id) = view.object.id() else {
        return Ok(Outcome::Ignored("an Undo of a Listen naming no id"));
    };
    let deleted = scrobble::delete_by_ap_id(&state.db, signer.id, id)
        .await
        .transient("the scrobble delete failed")?;
    if deleted == 0 {
        return Ok(Outcome::Ignored("no stored scrobble of that Listen"));
    }
    debug!(listen = %id, "an Undo removed a remote scrobble");
    Ok(Outcome::Handled)
}

/// The body of [`handle_listen`].
async fn apply_listen(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored("a Listen signed by a local account"));
    }
    let (audio, referenced) = match audio_document(&state, &view.object).await {
        Ok(found) => found,
        Err(reason) => return Ok(Outcome::Ignored(reason)),
    };
    let mut parsed = match parse_audio(&audio, &signer.ap_id, referenced) {
        Ok(parsed) => parsed,
        Err(reason) => return Ok(Outcome::Ignored(reason)),
    };
    parsed.visibility = note_parser::parse_visibility(&view.raw, &signer);
    let now = Utc::now();
    let published = audio.get("published").or_else(|| view.raw.get("published"));
    let listened_at = json::clamped_time(published, now).unwrap_or(now);
    let stored = scrobble::insert_remote(&state.db, signer.id, &view.id, &parsed, listened_at)
        .await
        .transient("the scrobble write failed")?;
    let Some(row) = stored else {
        return Ok(Outcome::Ignored("a Listen already stored"));
    };
    debug!(scrobble = %row.id, "a Listen folded a new remote scrobble");
    Ok(Outcome::Handled)
}

/// Reads the `Audio` a `Listen` carries, fetching it when only an id came.
///
/// An embedded document is taken as delivered (the flag is `false`); a bare
/// id is dereferenced under the per-activity fetch cap (the flag is `true`).
/// The `Err` is the static reason the listen is ignored.
async fn audio_document(
    state: &AppState,
    object: &ObjectRef,
) -> Result<(Value, bool), &'static str> {
    match object {
        ObjectRef::Embedded(document) => Ok((document.clone(), false)),
        ObjectRef::Id(id) => fetch_object_json(state, id)
            .await
            .map(|document| (document, true))
            .map_err(|err| {
                debug!(object = %id, %err, "a Listen's referenced Audio could not be fetched");
                "referenced Audio could not be fetched"
            }),
    }
}

/// Reads one `Audio` document into the scrobble it describes.
///
/// The visibility is the default, for the caller to replace.
///
/// `actor` is the delivering account: a document that names a creator (under
/// `actor` or `attributedTo`) must name `actor`, and one fetched by
/// reference (`require_creator`) must name a creator at all. The `Err` is
/// the static reason the listen is ignored.
fn parse_audio(
    audio: &Value,
    actor: &str,
    require_creator: bool,
) -> Result<NewScrobble, &'static str> {
    let mut named = false;
    for key in ["actor", "attributedTo"] {
        if let Some(creator) = audio.get(key).and_then(json::value_or_id) {
            if creator != actor {
                return Err("the Audio is attributed to someone else");
            }
            named = true;
        }
    }
    if require_creator && !named {
        return Err("the fetched Audio names no creator");
    }
    if audio.get("type").and_then(json::first_type).as_deref() != Some(TYPE_AUDIO) {
        return Err("the Listen's object is not an Audio");
    }
    let Some(title) = text(audio, "title", MAX_TITLE_CHARS) else {
        return Err("the Audio has no title");
    };
    Ok(NewScrobble {
        title,
        artist: text(audio, "artist", MAX_TEXT_CHARS),
        album: text(audio, "album", MAX_TEXT_CHARS),
        length: length_of(audio)?,
        external_link: link_of(audio)?,
        ..NewScrobble::default()
    })
}

/// The non-blank string at `key`, cut to `max` characters.
fn text(audio: &Value, key: &str, max: usize) -> Option<String> {
    let value = audio.get(key).and_then(Value::as_str)?;
    (!value.trim().is_empty()).then(|| value.chars().take(max).collect())
}

/// The `length` in milliseconds, or `None` when absent.
///
/// A non-negative integer that fits the column is kept; anything else refuses
/// the listen.
fn length_of(audio: &Value) -> Result<Option<i32>, &'static str> {
    match audio.get("length") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .and_then(|millis| i32::try_from(millis).ok())
            .filter(|millis| *millis >= 0)
            .map(Some)
            .ok_or("the Audio's length is not a non-negative integer"),
    }
}

/// The `externalLink`: an `http(s)` URL within the local limit, or `None`.
///
/// Absent, `null` or blank is `None`; anything else refuses the listen.
fn link_of(audio: &Value) -> Result<Option<String>, &'static str> {
    match audio.get("externalLink") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(link)) if link.trim().is_empty() => Ok(None),
        Some(Value::String(link))
            if link.chars().count() <= MAX_LINK_CHARS && is_http_url(link) =>
        {
            Ok(Some(link.clone()))
        }
        Some(_) => Err("the Audio's externalLink is not an http(s) URL"),
    }
}

#[cfg(test)]
mod tests;
