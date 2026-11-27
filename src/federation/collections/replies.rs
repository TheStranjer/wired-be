//! The thread collections of one local status: its replies
//! ([`replies_document`]) and the whole conversation it belongs to
//! ([`context_document`]).
//!
//! Both are served oldest first and keyset-paged on the status id, with
//! the `min_id` / `max_id` cursors spelled out in the `next` / `prev`
//! URLs. The replies collection follows Mastodon's `NoteSerializer`: the
//! note embeds a first page of the author's own answers and `next` leads
//! to `?only_other_accounts=true&page=true`, the page of everyone else's
//! (Mastodon's `FetchRepliesService` crawls exactly those links), while a
//! plain `?page=true` lists every public answer — the walk Pleroma and
//! Akkoma backfill follows. The context is the FEP-7888 shape: an
//! `OrderedCollection` of the thread's public object ids.

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    models::status::{self, ReplyAuthors, Status},
    util::id::context_ap_id,
};

use super::{
    COLLECTION, ORDERED_COLLECTION, PageLinks, collection_page, collection_root, flag_set,
    full_page, keyset_page, ordered_collection_page, page_requested, requested_url,
};

/// How many reply ids one replies page holds.
const REPLIES_PAGE_SIZE: i64 = 20;
/// How many object ids one context page holds.
const CONTEXT_PAGE_SIZE: i64 = 20;
/// The query flag that narrows the replies to other accounts' answers.
const ONLY_OTHER_ACCOUNTS: &str = "only_other_accounts";
/// The query prefix the narrowed walk keeps on its cursor links.
const OTHER_ACCOUNTS_PREFIX: &str = "only_other_accounts=true&";

/// The replies collection of `object`.
///
/// The root carries the count of publicly visible answers; when the query
/// asks for a page, the 20 (`REPLIES_PAGE_SIZE`) oldest ones as their
/// object ids.
///
/// With `only_other_accounts=true` a page lists only the answers written
/// by accounts other than `object`'s author, whose own answers are the
/// ones the note embeds.
///
/// # Errors
///
/// [`AppError::BadRequest`] when a cursor is no id; any database failure
/// arrives as [`AppError::Database`](crate::error::AppError).
pub async fn replies_document(
    pool: &PgPool,
    config: &Config,
    object: &Status,
    query: Option<&str>,
) -> AppResult<Value> {
    let id = format!("{}/replies", object.uri);
    if !page_requested(query) {
        let total = status::public_replies_count(pool, object.id).await?;
        return Ok(collection_root(&id, COLLECTION, Some(total), config));
    }
    let page = keyset_page(query, REPLIES_PAGE_SIZE)?;
    let (authors, prefix) = if flag_set(query, ONLY_OTHER_ACCOUNTS) {
        (ReplyAuthors::Others(object.user_id), OTHER_ACCOUNTS_PREFIX)
    } else {
        (ReplyAuthors::Anyone, "")
    };
    let rows = status::public_replies_page_by(pool, object.id, authors, &page).await?;
    let links = ascending_links(&id, query, prefix, &row_ids(&rows), page.limit);
    Ok(collection_page(&links, object_ids(&rows), config))
}

/// The context collection of conversation `conversation_id`.
///
/// The root carries the count of the thread's public objects; when the
/// query asks for a page, the 20 (`CONTEXT_PAGE_SIZE`) oldest as their
/// object ids.
///
/// A conversation with no publicly visible object is
/// [`AppError::RouteNotFound`], whether it never existed or only holds
/// private posts: the collection must not tell the two apart. So is a
/// conversation no local status was published naming this minted address
/// ([`status::names_context`]): a remote thread whose context was stored
/// before the first local reply keeps that context, and this instance does
/// not claim one of its own. Once a local status named the minted address
/// it stays served, whatever context a later remote reply brings.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] as above; [`AppError::BadRequest`] when a
/// cursor is no id; any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn context_document(
    pool: &PgPool,
    config: &Config,
    conversation_id: Uuid,
    query: Option<&str>,
) -> AppResult<Value> {
    let id = context_id(config, conversation_id);
    if !status::names_context(pool, conversation_id, &id).await? {
        return Err(AppError::RouteNotFound);
    }
    let total = status::public_context_count(pool, conversation_id).await?;
    if total == 0 {
        return Err(AppError::RouteNotFound);
    }
    if !page_requested(query) {
        return Ok(collection_root(
            &id,
            ORDERED_COLLECTION,
            Some(total),
            config,
        ));
    }
    let page = keyset_page(query, CONTEXT_PAGE_SIZE)?;
    let rows = status::public_context_page(pool, conversation_id, &page).await?;
    let links = ascending_links(&id, query, "", &row_ids(&rows), page.limit);
    Ok(ordered_collection_page(&links, object_ids(&rows), config))
}

/// The address of conversation `conversation_id`'s context collection:
/// what the notes of the thread name as their `context`.
#[must_use]
pub fn context_id(config: &Config, conversation_id: Uuid) -> String {
    context_ap_id(&config.base_url, conversation_id)
}

/// The object ids of `rows`, as page items.
fn object_ids(rows: &[Status]) -> Vec<Value> {
    rows.iter().map(|row| json!(&row.uri)).collect()
}

/// The ids of `rows`, oldest first as the page came back.
fn row_ids(rows: &[Status]) -> Vec<Uuid> {
    rows.iter().map(|row| row.id).collect()
}

/// The envelope of one oldest-first page whose rows have ids `ids`.
///
/// `next` walks newer from the page's newest id while the page came back
/// full (`limit` rows), `prev` walks back toward the oldest end from the
/// page's oldest id. `prefix` is the query the walk keeps in front of
/// `page=true` on its links (empty, or a flag such as
/// `only_other_accounts=true&`), and the page's `id` echoes the request.
fn ascending_links(
    collection: &str,
    query: Option<&str>,
    prefix: &str,
    ids: &[Uuid],
    limit: i64,
) -> PageLinks {
    let cursor = |name: &str, id: Uuid| format!("{collection}?{prefix}page=true&{name}={id}");
    PageLinks {
        id: requested_url(collection, query),
        part_of: collection.to_owned(),
        next: full_page(ids.len(), limit)
            .then_some(ids.last())
            .flatten()
            .map(|id| cursor("min_id", *id)),
        prev: ids.first().map(|id| cursor("max_id", *id)),
    }
}

#[cfg(test)]
mod tests;
