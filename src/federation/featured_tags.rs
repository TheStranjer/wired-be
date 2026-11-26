//! Featured hashtags on the wire: the `featuredTags` collection a local
//! actor serves and the one a remote actor's refresh reads back.
//!
//! The reference is Mastodon's `ActivityPub::CollectionsController` (the
//! `tags` collection of an actor, a `Collection` whose `items` are
//! `Hashtag` objects with an `href` and a `#name`) and its
//! `ActivityPub::FetchFeaturedTagsCollectionService`.
//!
//! [`featured_tags_document`] is the outbound half: the collection of a
//! local account, its `href`s the instance's own tag pages. `sync_remote`
//! is the inbound half: on every refresh of a remote actor it reads the
//! actor's `featuredTags` — a collection URL on the actor's own origin, or an
//! inline collection — and replaces the account's stored set with it. A
//! collection that cannot be fetched or read leaves the stored set as it
//! was; an actor that advertises no `featuredTags` has none.

use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::{debug, warn};

use crate::{
    config::Config,
    error::AppResult,
    federation::{
        fetch_actor::{Signer, fetch_actor_json},
        json::{as_array, value_or_id},
        note,
    },
    models::{
        featured_live::{self, Collection},
        featured_tag,
        user::{User, featured_tags},
    },
    state::AppState,
    util::url::{parse_http_url, same_origin},
};

/// The `type` of a featured-tags collection.
const COLLECTION: &str = "Collection";
/// The `type` of one item.
const HASHTAG: &str = "Hashtag";
/// Most items read from one remote collection: far above the ten tags an
/// account may feature, so a hostile collection cannot make the read grow.
const MAX_ITEMS: usize = 50;

/// The `featuredTags` collection of the local account `user`.
///
/// Every featured tag is a `Hashtag` object whose `href` is this instance's
/// page for the tag, inline in one `Collection` (at most ten items). An
/// account that is not local and active lists nothing.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn featured_tags_document(
    pool: &PgPool,
    config: &Config,
    user: &User,
) -> AppResult<Value> {
    let entries = if user.is_local_active() {
        featured_tag::list(pool, user.id).await?
    } else {
        Vec::new()
    };
    Ok(collection_json(
        &featured_tags(&user.ap_id),
        config,
        &entries,
    ))
}

/// The collection document for the featured `entries`.
///
/// Each item's `name` keeps the owner's casing; its `href` slug is the
/// lowercase tag.
fn collection_json(id: &str, config: &Config, entries: &[featured_tag::Entry]) -> Value {
    let items: Vec<Value> = entries
        .iter()
        .map(|entry| {
            json!({
                "type": HASHTAG,
                "href": format!("{}/tags/{}", config.base_url, entry.tag_name),
                "name": format!("#{}", entry.name),
            })
        })
        .collect();
    json!({
        "@context": note::json_ld_context(config),
        "id": id,
        "type": COLLECTION,
        "totalItems": items.len(),
        "items": items,
    })
}

/// Replaces the featured tags of the mirrored remote `user` with those its
/// actor document `actor` advertises as `featuredTags`.
///
/// Never fails the refresh that called it: a collection that cannot be read
/// keeps the stored set, and a database failure is logged.
pub(crate) async fn sync_remote(
    state: &AppState,
    user: &User,
    actor: &Value,
    signer: Option<&Signer>,
) {
    if featured_live::is_recent(&state.db, user.id, Collection::Tags).await {
        debug!(actor = %user.ap_id, "a live tag change is recent; the collection may be cached, tags kept");
        return;
    }
    let names = match actor.get("featuredTags").filter(|field| !field.is_null()) {
        None => Vec::new(),
        Some(reference) => match load_collection(state, &user.ap_id, reference, signer).await {
            Some(collection) => hashtag_names(&collection),
            None => return,
        },
    };
    match featured_tag::replace_all(&state.db, user.id, &names).await {
        Ok(()) => debug!(actor = %user.ap_id, count = names.len(), "featured tags stored"),
        Err(err) => warn!(%err, actor = %user.ap_id, "could not store featured tags"),
    }
}

/// The collection `reference` names, `None` when it cannot be read.
///
/// An inline collection is taken as it is; a URL on the actor's own origin
/// is fetched, and so is its `first` page when the collection is paged.
/// The actor's featured pins read their collection the same way.
pub(crate) async fn load_collection(
    state: &AppState,
    actor_id: &str,
    reference: &Value,
    signer: Option<&Signer>,
) -> Option<Value> {
    if has_items(reference) {
        return Some(reference.clone());
    }
    let document = fetch_contained(state, actor_id, reference, signer).await?;
    if has_items(&document) {
        return Some(document);
    }
    match document.get("first") {
        Some(first) if has_items(first) => Some(first.clone()),
        Some(first) => fetch_contained(state, actor_id, first, signer).await,
        None => Some(document),
    }
}

/// The document `reference` names, fetched when it is on the actor's origin.
///
/// A collection on another host is not the actor's to speak for.
async fn fetch_contained(
    state: &AppState,
    actor_id: &str,
    reference: &Value,
    signer: Option<&Signer>,
) -> Option<Value> {
    let url = value_or_id(reference)?;
    let actor = parse_http_url(actor_id)?;
    if !parse_http_url(&url).is_some_and(|target| same_origin(&actor, &target)) {
        debug!(%url, actor = %actor_id, "featured tags collection on another origin");
        return None;
    }
    match fetch_actor_json(&state.http_client, &url, signer).await {
        Ok(document) => Some(document),
        Err(err) => {
            warn!(%err, %url, "could not fetch the featured tags collection");
            None
        }
    }
}

/// Whether `value` carries its items inline (`items` or `orderedItems`).
fn has_items(value: &Value) -> bool {
    value.get("items").is_some() || value.get("orderedItems").is_some()
}

/// The tag names of the items of `collection`, as written.
///
/// Each is the item's `name`, or the last path segment of its `href` when it
/// names nothing; the store normalizes them. Items that are not objects are
/// skipped.
fn hashtag_names(collection: &Value) -> Vec<String> {
    let Some(items) = collection
        .get("orderedItems")
        .or_else(|| collection.get("items"))
    else {
        return Vec::new();
    };
    as_array(items)
        .iter()
        .take(MAX_ITEMS)
        .filter_map(hashtag_name)
        .collect()
}

/// The tag name of one `Hashtag` object, as written.
///
/// It is the object's `name`, or the last path segment of its `href` when it
/// names nothing; the store normalizes it. `None` for an object with neither.
pub(crate) fn hashtag_name(item: &Value) -> Option<String> {
    let name = item.get("name").and_then(Value::as_str);
    let from_href = || {
        let href = item.get("href").and_then(Value::as_str)?;
        href.trim_end_matches('/').rsplit('/').next()
    };
    name.or_else(from_href).map(str::to_owned)
}

#[cfg(test)]
mod tests;
