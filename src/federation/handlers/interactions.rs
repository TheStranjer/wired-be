//! Inbound `Like`, `EmojiReact`, `Announce` and their `Undo`s: a peer's
//! interactions with a post, folded onto this instance's rows.
//!
//! The reference is Pleroma's `Pleroma.Web.ActivityPub.SideEffects`
//! (`lib/pleroma/web/activity_pub/side_effects.ex`) — its `Like`,
//! `EmojiReact` and `Announce` clauses add the like, the reaction and the
//! announce to the acted-on object and raise the author's notifications, and
//! its `handle_undoing` takes exactly that back — with
//! `object_validators/like_validator.ex`, `emoji_react_validator.ex` and
//! `announce_validator.ex` as the documents' own gates. The ticket rules are
//! authoritative where the two differ:
//!
//! * The shared step, `resolve_target`, is every fold's first move: the
//!   activity's `object` — a bare id or an embedded document's — resolves
//!   through [`fetch_object::resolve_status`], so a local status answers
//!   without the network, an unknown remote one is fetched and ingested, a
//!   `Gone`/`NotFound` answer folds nothing, and a transient failure
//!   releases the activity for a retry. The resolved row is folded to its
//!   original when it names a reblog — nobody likes, reacts to or boosts a
//!   boost, the `action_target_id` rule of the endpoints — and it must
//!   stand visible to the actor: `private` and `direct` pass only for an
//!   accepted follower or a mentioned account, the shared
//!   [`status_visibility::can_view`] read.
//! * [`handle_like`] records a [`favourite::favourite_like`] row under the
//!   delivered activity id — idempotent per actor and status, the counter
//!   and the author's telling moving only with the row — and runs the
//!   favourite endpoint's `after_favourite_changed` hook, so a local author
//!   is told and streaming reacts exactly as of a local like. A `Like`
//!   carrying a `content` that names one emoji is a reaction in disguise —
//!   the Misskey and Fedibird favourite-with-emoji shape, Pleroma's
//!   `Transmogrifier` rewrite — and folds through the reaction arm; a
//!   `content` that names nothing usable is ignored rather than defaulted
//!   into a plain like, Pleroma's posture. Types that merely resemble a
//!   like — `Dislike`, `Zap` — reach no handler at all: the dispatcher's
//!   own unknown-type arm ignores them.
//! * [`handle_emoji_react`] validates the activity's `content` by the same
//!   rules the local reaction endpoint asks of its path segment
//!   ([`Emoji::classify`]: one unicode grapheme, or a `:shortcode:`), and —
//!   for a shortcode — the image URL of a matching `Emoji` entry in the
//!   activity's `tag` list: the remote's own emoji is what the reaction
//!   names, so this instance's `custom_emojis` table is not consulted and
//!   the stored URL comes from the tag. The row lands through
//!   [`emoji_reaction::react_with_ap_id`] under the delivered id, and the
//!   reaction endpoint's `after_reaction_changed` hook tells the author.
//! * [`handle_announce`] folds the boost: the target must be `public` or
//!   `unlisted` (a boost of anything else is ignored), and the row is a
//!   reblog status whose `uri` is the delivered `Announce` id (decision 6:
//!   a reblog row *is* the activity), whose visibility the `Announce`'s own
//!   addressing states exactly as a note's — the `parse_visibility` rule:
//!   the public address in `to`,
//!   public; in `cc`, unlisted; the actor's followers collection, private;
//!   anything else, direct), and whose `inserted_at` is the activity's
//!   `published`. A pair that holds a live boost — or an id a row already
//!   carries — restates a fact and is ignored. No follow relation is asked
//!   of the actor, which is what lets a relay's re-broadcasts through
//!   (the relay ticket owns the other side of that pairing); a peer whose
//!   domain the instance rejects never reaches this code — the dispatcher's
//!   policy stage applies that, and a blocked author's domain with it.
//! * The `Undo` routes ([`handle_undo_like`], [`handle_undo_emoji_react`],
//!   [`handle_undo_announce`]) take back exactly the row the undone
//!   activity's id names — for its maker only, the same-actor scoping of
//!   the id reads and the reblog row's own authorship — through the models'
//!   idempotent withdrawal writes, and run the same hooks with their
//!   withdrawal arm: the counter steps back and the author's telling goes.
//!   An `Undo` signed by anybody other than the actor of what it undoes
//!   dies at the dispatcher's own gate before any of this runs.
//!
//! Every route runs behind the dispatcher's gates: containment has already
//! proved the activity's `actor` to be the signer's `ap_id`
//! ([`crate::federation::dispatch`]), so the [`User`] a handler is handed
//! *is* the remote account the activity speaks for; the local-signer guard
//! each one keeps is the belt to that bracer, as in
//! [`crate::federation::handlers::misc`].

use chrono::{DateTime, TimeDelta, Utc};
use futures::future::BoxFuture;
use serde_json::Value;
use sqlx::PgPool;
use tracing::debug;

use crate::{
    api::{
        emoji_reactions::after_reaction_changed,
        statuses::{action_target_id, after_favourite_changed, after_reblog_changed},
    },
    error::AppError,
    federation::{
        dispatch::{ActivityView, FederationError, Handlers, Outcome},
        fetch_object, json, note_parser,
    },
    models::{
        emoji_reaction::{self, NewReaction},
        favourite,
        status::{self, Status, Visibility},
        status_visibility,
        user::User,
    },
    state::AppState,
    util::{emoji::Emoji, sanitize::strip_html, time::parse_time},
};

/// How far an Announce's `published` may lead the local clock before the
/// row is stamped `now` instead — the same skew the note and chat folds
/// keep, restated here for the activity's own timestamp.
const CLOCK_SKEW: TimeDelta = TimeDelta::minutes(5);
/// The shared step's static reason: an activity whose object names nothing
/// to fold — no id, nothing live behind the id, a gone-on-fetch object, or
/// a target its actor may not see.
const NO_TARGET: &str = "an activity whose object names no status visible to its actor";
/// Why an `Undo` whose object carries no activity id at all is refused:
/// the withdrawal is keyed on the id of what it undoes.
const NO_INNER_ID: &str = "an Undo whose object names no activity id";
/// The local-signer guard of the three withdrawal routes, shared wording:
/// containment has already proved the signer the activity's actor, so a
/// local one here is an account interacting with itself through the
/// federation door that only remote peers walk.
const UNDO_LOCAL_SIGNER: &str = "an Undo signed by an account this instance owns";

/// Folds this module's interaction routes into the dispatcher's registry.
///
/// The three activity types by their own names and their three withdrawals
/// in the `Undo` table. `crate::federation::handlers::registry` calls it,
/// which is the whole composition of the live registry.
pub fn register(handlers: &mut Handlers) {
    handlers.register("Like", handle_like);
    handlers.register("EmojiReact", handle_emoji_react);
    handlers.register("Announce", handle_announce);
    handlers.register_undo("Like", handle_undo_like);
    handlers.register_undo("EmojiReact", handle_undo_emoji_react);
    handlers.register_undo("Announce", handle_undo_announce);
}

/// The `Like` route: a favourite, or — `content` naming one emoji — the
/// reaction in disguise Pleroma's `Transmogrifier` rewrite names.
///
/// # Errors
///
/// [`FederationError::Transient`] for a read or write the retry may
/// outlive, and the resolver's own for an object that cannot be fetched
/// yet; everything the shape or the standing refuses arrives as
/// [`Outcome::Ignored`].
pub fn handle_like(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_like(state, view, signer).await })
}

/// The `EmojiReact` route: a reaction stored under the delivered id, with
/// the custom emoji's image read off the activity's own `Emoji` tag.
///
/// # Errors
///
/// Those of [`handle_like`].
pub fn handle_emoji_react(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_emoji_react(state, view, signer).await })
}

/// The `Announce` route: the boost's reblog row, `uri`ed under the
/// delivered activity id and addressed as the `Announce` itself states.
///
/// # Errors
///
/// Those of [`handle_like`].
pub fn handle_announce(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_announce(state, view, signer).await })
}

/// The `undo_by_type["Like"]` route: withdraw the like whose activity id
/// the `Undo` names, for its maker only.
///
/// # Errors
///
/// Those of [`handle_like`].
pub fn handle_undo_like(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_undo_like(state, view, signer).await })
}

/// The `undo_by_type["EmojiReact"]` route: take back the reaction whose
/// activity id the `Undo` names, for its maker only.
///
/// # Errors
///
/// Those of [`handle_like`].
pub fn handle_undo_emoji_react(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_undo_emoji_react(state, view, signer).await })
}

/// The `undo_by_type["Announce"]` route: tombstone the boost whose
/// activity id the `Undo` names, for its maker only.
///
/// # Errors
///
/// Those of [`handle_like`].
pub fn handle_undo_announce(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_undo_announce(state, view, signer).await })
}

/// One delivered reaction, in the parts the fold stores.
#[derive(Debug)]
struct Reaction {
    /// The emoji in its stored spelling: the document's own text, outer
    /// whitespace gone.
    emoji: String,
    /// The `Emoji` tag's icon URL; `None` for a unicode reaction.
    url: Option<String>,
}

/// What a delivered `Like`'s `content` field states, read the way Pleroma
/// reads the Misskey/Fedibird favourite-with-emoji shape.
enum LikeContent {
    /// No content, or a blank one: an ordinary favourite.
    Plain,
    /// The content names one emoji: the activity is a reaction.
    Reaction(Reaction),
    /// The content says something no reaction can name: Pleroma's rewrite
    /// hands it to the reaction validator and the validator refuses, so
    /// this does too — nothing is folded.
    Unusable,
}

/// The body of [`handle_like`]: the content rewrite first — a
/// favourite-with-emoji is a reaction, not a like — then the favourite
/// fold behind the shared target step.
async fn apply_like(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "a Like signed by an account this instance owns",
        ));
    }
    match like_content(&view) {
        LikeContent::Reaction(reaction) => fold_reaction(&state, &view, &signer, &reaction).await,
        LikeContent::Unusable => Ok(Outcome::Ignored(
            "a Like whose content names neither one emoji nor a tagged shortcode",
        )),
        LikeContent::Plain => fold_favourite(&state, &view, &signer).await,
    }
}

/// The body of [`handle_emoji_react`]: the document's own `content` as a
/// reaction, then the shared reaction fold.
async fn apply_emoji_react(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "an EmojiReact signed by an account this instance owns",
        ));
    }
    let Some(content) = view.raw.get("content").and_then(Value::as_str) else {
        return Ok(Outcome::Ignored(
            "an EmojiReact whose content is not a string",
        ));
    };
    match reaction_of(&view.raw, content) {
        None => Ok(Outcome::Ignored(
            "the EmojiReact content is neither one emoji nor a tagged shortcode",
        )),
        Some(reaction) => fold_reaction(&state, &view, &signer, &reaction).await,
    }
}

/// The body of [`handle_announce`]: the shared target step, the
/// boostability rule, the duplicate read, and the reblog row the activity
/// itself describes.
async fn apply_announce(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "an Announce signed by an account this instance owns",
        ));
    }
    let Some(target) = resolve_target(&state, &view, &signer).await? else {
        return Ok(Outcome::Ignored(NO_TARGET));
    };
    if !boostable(target.visibility) {
        debug!(status = %target.id, visibility = target.visibility.as_str(),
               "a boost of a status that is not public or unlisted");
        return Ok(Outcome::Ignored(
            "a boost of a status that is not public or unlisted",
        ));
    }
    if boost_stands(&state.db, &signer, &target, &view.id).await? {
        return Ok(Outcome::Ignored(
            "this actor's boost of that status stands already",
        ));
    }
    let new = status::NewStatus {
        uri: Some(view.id.clone()),
        user_id: signer.id,
        visibility: note_parser::parse_visibility(&view.raw, &signer),
        conversation_id: Some(target.conversation_id),
        reblog_of_id: Some(target.id),
        inserted_at: Some(announced_at(&view.raw, Utc::now())),
        ..Default::default()
    };
    let reblog = status::insert(&state.db, &state.config.base_url, &new)
        .await
        .map_err(|err| transient("boost", &err))?;
    after_reblog_changed(&state, &reblog, true).await;
    debug!(
        status = %target.id,
        actor = %signer.nickname,
        visibility = reblog.visibility.as_str(),
        "an inbound Announce boosted a status"
    );
    Ok(Outcome::Handled)
}

/// The body of [`handle_undo_like`]: the signer's own like row under the
/// undone activity id, withdrawn through the endpoint's own write.
async fn apply_undo_like(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(UNDO_LOCAL_SIGNER));
    }
    let Some(like_id) = view.object.id() else {
        return Ok(Outcome::Ignored(NO_INNER_ID));
    };
    let row = favourite::get_by_ap_id(&state.db, signer.id, like_id)
        .await
        .map_err(|err| transient("like lookup", &err))?;
    let Some(row) = row else {
        debug!(like_id, "an Undo naming no Like of its actor");
        return Ok(Outcome::Ignored("an Undo of a Like its actor never made"));
    };
    let removed = favourite::unfavourite(&state.db, signer.id, row.status_id)
        .await
        .map_err(|err| transient("like withdrawal", &err))?;
    let Some(removed) = removed else {
        return Ok(Outcome::Ignored("that Like is already withdrawn"));
    };
    after_favourite_changed(&state, &removed, false).await;
    debug!(
        status = %removed.status_id,
        actor = %signer.nickname,
        "an inbound Undo withdrew a Like"
    );
    Ok(Outcome::Handled)
}

/// The body of [`handle_undo_emoji_react`]: the signer's own reaction under
/// the undone activity id, withdrawn through the endpoint's own write.
async fn apply_undo_emoji_react(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(UNDO_LOCAL_SIGNER));
    }
    let Some(react_id) = view.object.id() else {
        return Ok(Outcome::Ignored(NO_INNER_ID));
    };
    let row = emoji_reaction::get_by_ap_id(&state.db, signer.id, react_id)
        .await
        .map_err(|err| transient("reaction lookup", &err))?;
    let Some(row) = row else {
        debug!(react_id, "an Undo naming no EmojiReact of its actor");
        return Ok(Outcome::Ignored(
            "an Undo of an EmojiReact its actor never made",
        ));
    };
    let removed = emoji_reaction::unreact(&state.db, signer.id, row.status_id, &row.emoji)
        .await
        .map_err(|err| transient("reaction withdrawal", &err))?;
    let Some(removed) = removed else {
        return Ok(Outcome::Ignored("that reaction is already withdrawn"));
    };
    after_reaction_changed(&state, &removed, false).await;
    debug!(
        status = %removed.status_id,
        actor = %signer.nickname,
        emoji = %removed.emoji,
        "an inbound Undo withdrew a reaction"
    );
    Ok(Outcome::Handled)
}

/// The body of [`handle_undo_announce`]: the live boost row the undone
/// `Announce` id names, the signer's own, tombstoned through the
/// endpoint's withdrawal write.
async fn apply_undo_announce(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(UNDO_LOCAL_SIGNER));
    }
    let Some(announce_id) = view.object.id() else {
        return Ok(Outcome::Ignored(NO_INNER_ID));
    };
    let stored = status::get_by_uri(&state.db, announce_id)
        .await
        .map_err(|err| transient("boost lookup", &err))?;
    let claim = stored.filter(|row| row.user_id == signer.id && row.deleted_at.is_none());
    let Some(reblog) = claim else {
        debug!(announce_id, "an Undo naming no live boost of its actor");
        return Ok(Outcome::Ignored(
            "an Undo of an Announce its actor never made",
        ));
    };
    let Some(original_id) = reblog.reblog_of_id else {
        return Ok(Outcome::Ignored("an Undo naming a status that is no boost"));
    };
    let removed = status::remove_reblog(&state.db, signer.id, original_id)
        .await
        .map_err(|err| transient("boost withdrawal", &err))?;
    let Some(tombstone) = removed else {
        return Ok(Outcome::Ignored("that boost is already withdrawn"));
    };
    after_reblog_changed(&state, &tombstone, false).await;
    debug!(
        status = %original_id,
        actor = %signer.nickname,
        "an inbound Undo withdrew a boost"
    );
    Ok(Outcome::Handled)
}

/// The favourite arm of [`apply_like`]: the shared target step, the
/// pair-keyed write under the delivered id, and — only for the insert that
/// happened — the endpoint's post-write hook.
async fn fold_favourite(
    state: &AppState,
    view: &ActivityView,
    signer: &User,
) -> Result<Outcome, FederationError> {
    let Some(target) = resolve_target(state, view, signer).await? else {
        return Ok(Outcome::Ignored(NO_TARGET));
    };
    let created = favourite::favourite_like(&state.db, signer.id, target.id, &view.id)
        .await
        .map_err(|err| transient("like", &err))?;
    let Some(row) = created else {
        debug!(status = %target.id, "this actor's like of that status stands already");
        return Ok(Outcome::Ignored(
            "this actor's Like of that status stands already",
        ));
    };
    after_favourite_changed(state, &row, true).await;
    debug!(
        status = %target.id,
        actor = %signer.nickname,
        "an inbound Like favourited a status"
    );
    Ok(Outcome::Handled)
}

/// The reaction fold, shared by [`apply_like`]'s rewritten arm and
/// [`apply_emoji_react`]: the target step, the triple-keyed write under
/// the delivered id, and the reaction endpoint's post-write hook — whose
/// telling deduplicates a restated reaction exactly as the `PUT` route's
/// repeat does.
async fn fold_reaction(
    state: &AppState,
    view: &ActivityView,
    signer: &User,
    reaction: &Reaction,
) -> Result<Outcome, FederationError> {
    let Some(target) = resolve_target(state, view, signer).await? else {
        return Ok(Outcome::Ignored(NO_TARGET));
    };
    let new = NewReaction {
        user_id: signer.id,
        status_id: target.id,
        emoji: &reaction.emoji,
        url: reaction.url.as_deref(),
    };
    let row = emoji_reaction::react_with_ap_id(&state.db, &new, &view.id)
        .await
        .map_err(|err| transient("reaction", &err))?;
    after_reaction_changed(state, &row, true).await;
    debug!(
        status = %target.id,
        actor = %signer.nickname,
        emoji = %row.emoji,
        "an inbound reaction landed on a status"
    );
    Ok(Outcome::Handled)
}

/// The shared step of every interaction fold: the live status the
/// activity's `object` names — resolved through
/// [`fetch_object::resolve_status`] (a local status answers without the
/// network; an unknown remote one is fetched and ingested; a gone or
/// missing object answers nothing) — folded to its original when the row
/// names a reblog, and admitted only while it stands visible to the
/// actor.
///
/// # Errors
///
/// [`FederationError::Transient`] for a network or database failure the
/// retry may outlive; `Ok(None)` is the [`Outcome::Ignored`] answer, after
/// the debug line that says which arm refused.
async fn resolve_target(
    state: &AppState,
    view: &ActivityView,
    actor: &User,
) -> Result<Option<Status>, FederationError> {
    let Some(id) = view.object.id() else {
        debug!("an interaction whose embedded object names no id");
        return Ok(None);
    };
    let Some(found) = fetch_object::resolve_status(state, id).await? else {
        debug!(
            object = id,
            "the object of an inbound interaction resolves to no status"
        );
        return Ok(None);
    };
    let Some(target) = interaction_target(state, &found).await? else {
        return Ok(None);
    };
    if !status_visibility::can_view(&state.db, &target, Some(actor))
        .await
        .map_err(|err| transient("the visibility read", &err))?
    {
        debug!(
            status = %target.id,
            actor = %actor.nickname,
            "the target status is not visible to the interaction's actor"
        );
        return Ok(None);
    }
    Ok(Some(target))
}

/// The row the interaction lands on: `found` itself for a post, its live
/// original for a reblog row — the endpoints' `action_target_id` rule, so
/// likes, reactions and boosts of a boost all land on the one post. `None`
/// (after the debug line) is a reblog whose original is gone.
async fn interaction_target(
    state: &AppState,
    found: &Status,
) -> Result<Option<Status>, FederationError> {
    let target_id = action_target_id(found);
    if target_id == found.id {
        return Ok(Some(found.clone()));
    }
    let original = status::get_by_id(&state.db, target_id)
        .await
        .map_err(|err| transient("the original read", &err))?;
    if original.is_none() {
        debug!(
            reblog = %found.id,
            "a boost whose original is no longer stored is nobody's interaction target"
        );
    }
    Ok(original)
}

/// Whether a status may be boosted: `public` and `unlisted` answers, the
/// ticket's rule restated — and, with it, the relay's re-broadcast of a
/// public post the instance's users never followed anybody for.
const fn boostable(visibility: Visibility) -> bool {
    matches!(visibility, Visibility::Public | Visibility::Unlisted)
}

/// Whether this `Announce` is already folded: the pair holds a live reblog
/// row (one boost per account and status, the table's own rule), or a row
/// already stands under the activity's own `uri` — tombstone included, the
/// id stays reserved (decision 6), so a redelivery of a withdrawn boost
/// restates nothing.
///
/// # Errors
///
/// [`FederationError::Transient`] for either read's own database failure.
async fn boost_stands(
    db: &PgPool,
    signer: &User,
    target: &Status,
    announce_id: &str,
) -> Result<bool, FederationError> {
    let pair = status::reblogged_status_ids(db, signer.id, std::slice::from_ref(&target.id))
        .await
        .map_err(|err| transient("the boost read", &err))?;
    if pair.contains(&target.id) {
        return Ok(true);
    }
    let same_id = status::get_by_uri(db, announce_id)
        .await
        .map_err(|err| transient("the boost uri read", &err))?;
    Ok(same_id.is_some())
}

/// What a `Like`'s `content` states: absent or blank (of any markup) is the
/// plain favourite; text that names one emoji (or a tagged shortcode) is
/// the reaction Pleroma's rewrite names; anything else is nothing this
/// instance folds.
fn like_content(view: &ActivityView) -> LikeContent {
    let Some(content) = view.raw.get("content").and_then(Value::as_str) else {
        return LikeContent::Plain;
    };
    if strip_html(content).trim().is_empty() {
        return LikeContent::Plain;
    }
    reaction_of(&view.raw, content).map_or(LikeContent::Unusable, LikeContent::Reaction)
}

/// The stored reaction one delivered document states: its emoji text, HTML
/// gone and outer whitespace trimmed, classified by the local reaction
/// rules — one unicode grapheme, or a `:shortcode:` whose image the
/// activity's `tag` list carries. `None` is the fold's refusal: text that
/// names no single emoji, or a shortcode no `Emoji` tag vouches for
/// (Pleroma's `maybe_validate_tag_presence`, restated).
fn reaction_of(document: &Value, content: &str) -> Option<Reaction> {
    let text = strip_html(content).trim().to_owned();
    let code = match Emoji::classify(&text) {
        Some(Emoji::Unicode(_)) => None,
        Some(Emoji::Custom(code)) => Some(code.to_owned()),
        None => return None,
    };
    let url = match code.as_deref() {
        None => None,
        Some(name) => Some(emoji_tag_url(document, name)?),
    };
    Some(Reaction { emoji: text, url })
}

/// The icon URL of the `Emoji` tag naming `shortcode`: a tag entry of type
/// `Emoji` whose `name` — outer colons dropped, the spellings both Misskey
/// and Pleroma send — equals the shortcode, carrying an `http(s)` URL under
/// `icon`. The first match wins; `None` refuses the shortcode.
fn emoji_tag_url(document: &Value, shortcode: &str) -> Option<String> {
    document
        .get("tag")
        .map_or_default(json::as_array)
        .iter()
        .find(|entry| {
            entry.get("type").and_then(Value::as_str) == Some("Emoji")
                && entry
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.trim_matches(':') == shortcode)
        })?
        .get("icon")
        .and_then(json::first_string)
        .filter(|url| json::http_host(url).is_some())
}

/// The reblog row's creation instant: the `Announce` activity's own
/// `published` through [`parse_time`], clamped to `now` when it leads the
/// local clock past [`CLOCK_SKEW`] — a peer's runaway clock moves nobody's
/// timeline — and answered `now` when the field is absent or unreadable.
fn announced_at(activity: &Value, now: DateTime<Utc>) -> DateTime<Utc> {
    let Some(published) = activity
        .get("published")
        .and_then(Value::as_str)
        .and_then(parse_time)
    else {
        return now;
    };
    let ceiling = now.checked_add_signed(CLOCK_SKEW).unwrap_or(now);
    if published > ceiling { now } else { published }
}

/// Shapes a database failure into the dispatcher's retryable arm, naming
/// the read or write that failed.
fn transient(what: &str, err: &AppError) -> FederationError {
    FederationError::Transient(format!(
        "could not fold the remote interaction's {what}: {err}"
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{Flag, NewUser, insert, local_ap_id},
    };

    /// The remote signer every direct-call fixture here delivers as.
    const BOB: &str = "https://remote.example/users/bob";

    /// Test state: `pool` and the fixed test config, collaborators inert.
    fn app_state(pool: &PgPool) -> AppState {
        let config = std::sync::Arc::new(crate::config::Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: std::sync::Arc::clone(&config),
            http_client: crate::http_client::HttpClient::from(config.as_ref()),
            uploader: crate::uploads::UploaderBackend::from(config.as_ref()),
            instance_stats_cache: std::sync::Arc::default(),
            mrf_config_cache: std::sync::Arc::default(),
            translation_languages_cache: std::sync::Arc::default(),
            rate_limiter: std::sync::Arc::new(crate::rate_limit::RateLimiter::from_config(
                config.as_ref(),
            )),
            mailer: std::sync::Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// The stored mirror of the fixture signer.
    async fn signer(pool: &PgPool) -> User {
        insert(
            pool,
            &NewUser {
                nickname: "bob@remote.example".to_owned(),
                ap_id: BOB.to_owned(),
                local: Flag::new(false),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the signer fixture inserts")
    }

    /// A local author named `nickname` and one public post of theirs.
    async fn local_post(pool: &PgPool, nickname: &str) -> Status {
        let author = insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the author fixture inserts");
        status::insert(
            pool,
            TEST_BASE_URL,
            &status::NewStatus::local(&author, "interaction target"),
        )
        .await
        .expect("the status fixture inserts")
    }

    /// The dispatcher's normalized view of `kind`, named `n` on the
    /// signer's host, delivered by `actor`, carrying `object` and the
    /// fields of `extra`.
    fn view(kind: &str, n: &str, actor: &str, object: Value, extra: Value) -> ActivityView {
        let mut activity = json!({
            "id": format!("https://remote.example/activities/{n}"),
            "type": kind,
            "actor": actor,
        });
        activity["object"] = object;
        if let Value::Object(fields) = extra {
            let map = activity
                .as_object_mut()
                .expect("the fixture activity is a map");
            for (key, value) in fields {
                let _previous = map.insert(key, value);
            }
        }
        ActivityView::parse(&activity).expect("the fixture is a dispatchable activity")
    }

    #[test]
    fn register_binds_the_three_interactions_and_their_withdrawals() {
        let mut handlers = Handlers::new();
        register(&mut handlers);
        for kind in ["Like", "EmojiReact", "Announce"] {
            assert!(handlers.handles(kind), "a {kind} routes here");
            assert!(handlers.handles_undo(kind), "its Undo routes here");
        }
        assert!(
            !handlers.handles("Dislike") && !handlers.handles("Zap"),
            "types that merely resemble a like stay the dispatcher's unknown-type ignore"
        );
    }

    #[test]
    fn a_like_content_states_a_reaction_or_nothing() {
        let plain = view(
            "Like",
            "plain",
            BOB,
            json!("https://test.example/objects/x"),
            json!({}),
        );
        assert!(
            matches!(like_content(&plain), LikeContent::Plain),
            "no content is a plain favourite"
        );
        let blank = view(
            "Like",
            "blank",
            BOB,
            json!("x"),
            json!({"content": "  <p> </p>"}),
        );
        assert!(
            matches!(like_content(&blank), LikeContent::Plain),
            "a blank content is a plain favourite too"
        );
        let emoji = view(
            "Like",
            "emoji",
            BOB,
            json!("x"),
            json!({"content": "<p>🍺</p>"}),
        );
        let LikeContent::Reaction(reaction) = like_content(&emoji) else {
            panic!("an HTML-wrapped emoji is a reaction");
        };
        assert_eq!(reaction.emoji, "🍺", "the tags and whitespace go");
        assert_eq!(reaction.url, None, "a unicode reaction carries no image");
        let text = view(
            "Like",
            "text",
            BOB,
            json!("x"),
            json!({"content": "<p>thanks!</p>"}),
        );
        assert!(
            matches!(like_content(&text), LikeContent::Unusable),
            "text that names no emoji folds nothing"
        );
    }

    #[test]
    fn a_shortcode_reaction_needs_its_emoji_tag() {
        let tagged = json!({
            "content": ":railroad_track:",
            "tag": [
                {"type": "Mention", "href": "https://remote.example/users/carol", "name": "@carol"},
                {"type": "Emoji", "name": ":railroad_track:",
                 "icon": {"type": "Image", "mediaType": "image/png",
                          "url": "https://remote.example/emoji/rail.png"}},
            ],
        });
        let reaction =
            reaction_of(&tagged, ":railroad_track:").expect("the tag vouches for the code");
        assert_eq!(
            reaction.emoji, ":railroad_track:",
            "the stored spelling keeps its colons"
        );
        assert_eq!(
            reaction.url.as_deref(),
            Some("https://remote.example/emoji/rail.png"),
            "the image is the tag's own"
        );

        let untagged = json!({"content": ":railroad_track:"});
        assert!(
            reaction_of(&untagged, ":railroad_track:").is_none(),
            "a shortcode no Emoji tag carries is refused"
        );
        let mismatched = json!({
            "tag": [{"type": "Emoji", "name": "other",
                     "icon": {"url": "https://remote.example/emoji/other.png"}}],
        });
        assert!(
            reaction_of(&mismatched, ":railroad_track:").is_none(),
            "and a tag naming another shortcode is no voucher"
        );
        for bad in ["", "word", "🙂🙂", "::", ":a b:"] {
            assert!(
                reaction_of(&json!({}), bad).is_none(),
                "{bad:?} is no reaction, tagged or not"
            );
        }
    }

    #[test]
    fn a_boost_takes_the_activities_published_time_on_skew_clamp_and_default() {
        let exact = announced_at(
            &json!({"published": "2024-06-01T10:00:00.000Z"}),
            DateTime::parse_from_rfc3339("2024-06-01T12:00:00Z")
                .expect("the fixture instant parses")
                .with_timezone(&Utc),
        );
        assert_eq!(
            exact.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "2024-06-01T10:00:00.000Z",
            "a past published lands verbatim"
        );
        let now = Utc::now();
        let future = announced_at(&json!({"published": "2999-01-01T00:00:00.000Z"}), now);
        assert_eq!(future, now, "a runaway clock is clamped to now");
        for unreadable in [
            json!({}),
            json!({"published": "yesterday"}),
            json!({"published": 7}),
        ] {
            assert_eq!(
                announced_at(&unreadable, now),
                now,
                "{unreadable} defaults to now"
            );
        }
    }

    #[test]
    fn only_a_public_or_unlisted_status_is_boostable() {
        assert!(boostable(Visibility::Public) && boostable(Visibility::Unlisted));
        for private in [Visibility::Private, Visibility::Direct, Visibility::Local] {
            assert!(
                !boostable(private),
                "a boost of a {private} status is ignored"
            );
        }
    }

    #[tokio::test]
    async fn an_undo_naming_nothing_and_a_locally_signed_activity_fold_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let bob = signer(&pool).await;
        let idless = view(
            "Undo",
            "undo-idless",
            BOB,
            json!({"type": "Like"}),
            json!({}),
        );
        let outcome = handle_undo_like(state.clone(), idless, bob)
            .await
            .expect("an ignore is not a failure");
        assert_eq!(outcome, Outcome::Ignored(NO_INNER_ID));

        let local = insert(
            &pool,
            &NewUser::local("self-bob", &local_ap_id(TEST_BASE_URL, "self-bob")),
        )
        .await
        .expect("the local fixture inserts");
        let posted = local_post(&pool, "guard-author").await;
        let guarded = view(
            "Like",
            "guard-1",
            &local.ap_id,
            json!(posted.uri),
            json!({}),
        );
        let refused = handle_like(state, guarded, local)
            .await
            .expect("an ignore is not a failure");
        assert!(
            matches!(refused, Outcome::Ignored(_)),
            "a local signer is refused"
        );
        assert!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM favourites")
                .fetch_one(&pool)
                .await
                .expect("the table reads")
                == 0,
            "and nothing was recorded"
        );
    }
}
