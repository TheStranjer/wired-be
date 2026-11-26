//! Pinned posts of a remote actor, read back from its `featured` collection.
//!
//! The reference is Mastodon's `ActivityPub::SynchronizeFeaturedCollectionWorker`
//! (run from `ProcessAccountService` on every actor fetch or update): the
//! live `Add`/`Remove` activities (`handlers::misc::featured`) are only the
//! deltas, so an account discovered later, or a delivery that was missed or
//! arrived out of order, would never show the right pins without it.
//!
//! The sync reads the collection (inline, or by URL on the actor's own
//! origin, following `first` when it is paged), keeps the first 20 items
//! that are the actor's own statuses — fetching an unknown one on the actor's
//! origin through the object resolver — and replaces the account's pins with
//! them. A collection that cannot be read leaves the stored pins alone, and
//! so does a sync within [`FEATURED_SYNC_GRACE`](featured_live::FEATURED_SYNC_GRACE)
//! of a live `Add`/`Remove`: the peer may still serve its cached, older set.
//!
//! The actor `Update` handler runs the sync inline and never fails on it;
//! an actor fetch queues the `federator.featured_pins` job
//! ([`KIND_SYNC`], handled by [`handle`]) instead, so the object fetches stay
//! off the request path.

use serde_json::{Value, json};
use tracing::{debug, warn};
use uuid::Uuid;

use super::{
    dispatch::FederationError,
    featured_tags::load_collection,
    fetch_actor::{Signer, fetch_signer},
    fetch_object::resolve_status_as_actor,
    json::{as_array, value_or_id},
};
use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError, QUEUE_BACKGROUND, enqueue_keyed, load_user},
    models::{
        activitypub_actor,
        featured_live::{self, Collection},
        job::NewJob,
        pinned_status,
        user::User,
    },
    state::AppState,
    util::url::{parse_http_url, same_origin},
};

/// The [`JobRegistry`](crate::jobs::JobRegistry) key of the featured-pins
/// sync job: args `{"user_id": "<uuid>"}`, the remote account to refresh.
pub const KIND_SYNC: &str = "federator.featured_pins";

/// The failure budget of a `federator.featured_pins` job: three tries, then
/// the stored pins stay as they are until the next actor refresh.
pub const SYNC_MAX_ATTEMPTS: i32 = 3;

/// The unique-key prefix of a sync job, followed by the account's id: at most
/// one live sync per account.
const SYNC_KEY_PREFIX: &str = "featured_pins:";

/// Most items read from one featured collection. Mastodon lets an account
/// pin five; this is the generous bound that keeps a hostile collection from
/// growing the work.
const MAX_PINS: usize = 20;

/// The dedupe key of the sync job of the account `user_id`.
#[must_use]
pub fn sync_key(user_id: Uuid) -> String {
    format!("{SYNC_KEY_PREFIX}{user_id}")
}

/// Queues the sync of `user`'s pins when its actor `document` advertises a
/// `featured` collection.
///
/// Never fails the refresh that called it: a refused enqueue is logged, and
/// the next refresh queues the sync again.
pub(crate) async fn enqueue_sync(state: &AppState, user: &User, document: &Value) {
    if user.local.get() || featured_reference(document).is_none() {
        return;
    }
    let unique_key = sync_key(user.id);
    let new_job = NewJob {
        queue: QUEUE_BACKGROUND,
        kind: KIND_SYNC,
        args: json!({ "user_id": user.id.to_string() }),
        run_at: None,
        max_attempts: Some(SYNC_MAX_ATTEMPTS),
        unique_key: Some(&unique_key),
    };
    let _queued = enqueue_keyed(&state.db, new_job, "featured pins sync").await;
}

/// The `featured` property of an actor `document`, `None` when absent or null.
fn featured_reference(document: &Value) -> Option<&Value> {
    document.get("featured").filter(|field| !field.is_null())
}

/// The [`HandlerFn`](crate::jobs::HandlerFn) bound to [`KIND_SYNC`].
pub fn handle(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { handle_sync(state, args).await })
}

/// The `federator.featured_pins` handler: refreshes one remote account's pins
/// from the featured collection its stored actor row names.
///
/// # Errors
///
/// [`JobError::Discard`] for args no enqueue should have built or an account
/// that is gone; [`JobError::Retry`] for a database or network failure that
/// may pass on a later attempt. An account without a featured collection, or
/// a collection that cannot be read, is a quiet completion.
pub async fn handle_sync(state: AppState, args: Value) -> Result<(), JobError> {
    let Some(raw_id) = args.get("user_id").and_then(Value::as_str) else {
        return Err(JobError::Discard(
            "unusable featured pins args: no user id".to_owned(),
        ));
    };
    let Ok(user_id) = Uuid::parse_str(raw_id) else {
        return Err(JobError::Discard(format!(
            "unusable featured pins args: `{raw_id}` is not a user id"
        )));
    };
    let user = load_user(&state, user_id, "featured pins", false).await?;
    if user.local.get() {
        return Ok(());
    }
    let row = activitypub_actor::get(&state.db, user.id)
        .await
        .map_err(|err| JobError::Retry(format!("the featured url read failed: {err}")))?;
    let Some(featured) = row.and_then(|actor| actor.featured_url) else {
        debug!(account = %user.nickname, "no featured collection to sync");
        return Ok(());
    };
    let signer = fetch_signer(&state)
        .await
        .map_err(|err| JobError::Retry(format!("no fetch signer: {err}")))?;
    match sync(&state, &user, &Value::String(featured), signer.as_ref()).await {
        Ok(()) => Ok(()),
        Err(FederationError::Transient(detail)) => Err(JobError::Retry(detail)),
        Err(FederationError::Permanent(detail)) => Err(JobError::Discard(detail)),
    }
}

/// Syncs `user`'s pins when its actor `document` advertises a featured
/// collection, never failing the `Update` that called it.
///
/// An actor that advertises none keeps its pins. A failure is logged and the
/// stored pins stay as they were.
pub(crate) async fn sync_from_actor(
    state: &AppState,
    user: &User,
    document: &Value,
    signer: Option<&Signer>,
) {
    let Some(featured) = featured_reference(document) else {
        return;
    };
    if let Err(err) = sync(state, user, featured, signer).await {
        warn!(%err, account = %user.nickname, "could not sync the featured pins");
    }
}

/// Replaces the pins of the remote `user` with the statuses its `featured`
/// collection lists.
///
/// `featured` is the collection or the URL naming it. Only items on the
/// actor's origin that are its own statuses count; the rest are skipped. A
/// collection that cannot be read changes nothing.
///
/// # Errors
///
/// [`FederationError::Transient`] for a database or network failure while
/// resolving an item or storing the pins; the stored pins are then unchanged.
pub(crate) async fn sync(
    state: &AppState,
    user: &User,
    featured: &Value,
    signer: Option<&Signer>,
) -> Result<(), FederationError> {
    if featured_live::is_recent(&state.db, user.id, Collection::Pins).await {
        debug!(actor = %user.ap_id, "a live pin change is recent; the collection may be cached, pins kept");
        return Ok(());
    }
    let Some(collection) = load_collection(state, &user.ap_id, featured, signer).await else {
        return Ok(());
    };
    let Some(items) = collection_items(&collection) else {
        debug!(actor = %user.ap_id, "a featured collection that lists no items; pins kept");
        return Ok(());
    };
    let mut pinned: Vec<Uuid> = Vec::new();
    for item in items.iter().take(MAX_PINS) {
        if let Some(status_id) = own_status(state, user, item).await?
            && !pinned.contains(&status_id)
        {
            pinned.push(status_id);
        }
    }
    pinned_status::replace_all(&state.db, user.id, &pinned)
        .await
        .map_err(|err| FederationError::transient("could not store the featured pins", &err))?;
    debug!(actor = %user.ap_id, count = pinned.len(), "featured pins stored");
    Ok(())
}

/// The items of a featured `collection`, `None` when it says nothing about
/// them.
///
/// The `orderedItems` or `items` array, or an empty list for a collection
/// that declares `totalItems` zero. A document with neither is not a
/// readable collection, so the caller keeps what it has.
fn collection_items(collection: &Value) -> Option<&[Value]> {
    if let Some(items) = collection
        .get("orderedItems")
        .or_else(|| collection.get("items"))
    {
        return Some(as_array(items));
    }
    let empty: &[Value] = &[];
    (collection.get("totalItems").and_then(Value::as_u64) == Some(0)).then_some(empty)
}

/// The stored status `item` names when it is a post of `user` itself, `None`
/// for anything else.
///
/// An item on another origin than the actor is skipped without a request. A
/// status this instance does not store is fetched through the resolver, which
/// stores nothing `user` did not author.
///
/// # Errors
///
/// [`FederationError::Transient`] when the resolver hit a database or
/// network failure.
async fn own_status(
    state: &AppState,
    user: &User,
    item: &Value,
) -> Result<Option<Uuid>, FederationError> {
    let Some(uri) = value_or_id(item) else {
        return Ok(None);
    };
    let on_actors_origin = parse_http_url(&user.ap_id)
        .zip(parse_http_url(&uri))
        .is_some_and(|(actor, target)| same_origin(&actor, &target));
    if !on_actors_origin {
        debug!(%uri, actor = %user.ap_id, "a featured item on another origin; skipped");
        return Ok(None);
    }
    match resolve_status_as_actor(state, &uri, user).await {
        Ok(Some(status)) if status.user_id == user.id => Ok(Some(status.id)),
        Ok(_) => {
            debug!(%uri, actor = %user.ap_id, "a featured item that is not the actor's own status");
            Ok(None)
        }
        Err(FederationError::Permanent(detail)) => {
            debug!(%uri, %detail, "a featured item that cannot be folded; skipped");
            Ok(None)
        }
        Err(err @ FederationError::Transient(_)) => Err(err),
    }
}

#[cfg(test)]
mod tests;
