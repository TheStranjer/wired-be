//! The references a received note points at, resolved against storage.
//!
//! `resolve_reply` finds the stored parent or, when the parent is unstored,
//! the conversation of a stored status sharing the object's `context`;
//! `resolve_quote` settles the quoted object locally; `resolve_mentions`
//! turns the mention `href`s into accounts — unknown actors fetched over
//! the network under caps, the same function the inbound edit handler
//! shares — and `upsert_emojis` lands the announced custom emojis under
//! the author's domain. These are steps 4 to 6 of the
//! [parent module](crate::services::ingest).

use std::collections::HashSet;

use futures::future::join_all;
use tracing::debug;
use uuid::Uuid;

use crate::{
    federation::{
        dispatch::FederationError,
        fetch_actor::get_or_fetch_by_ap_id,
        note_parser::{ParsedNote, RemoteEmoji},
    },
    models::{
        custom_emoji, status,
        user::{self, User},
    },
    state::AppState,
};

use super::{MAX_MENTION_LOOKUPS, MENTION_LOOKUP_TIMEOUT, ReplyLink};

/// Step 4, reply arm: resolve the object's `inReplyTo` against the stored
/// statuses, and only when that misses inherit the conversation of a
/// stored status sharing the object's `context` (`None` conversation makes
/// [`status::insert`] mint the new row as its own thread root).
pub(super) async fn resolve_reply(
    state: &AppState,
    parsed: &ParsedNote,
) -> Result<ReplyLink, FederationError> {
    let Some(parent_uri) = parsed.reply_to_uri.as_deref() else {
        return Ok(ReplyLink::default());
    };
    let parent = status::get_by_uri(&state.db, parent_uri)
        .await
        .map_err(|err| FederationError::transient("the reply parent read failed", &err))?;
    if parent.is_some() {
        return Ok(ReplyLink {
            parent,
            ..ReplyLink::default()
        });
    }
    let context_conversation = match parsed.ap_context.as_deref() {
        Some(context) => status::conversation_id_for_context(&state.db, context)
            .await
            .map_err(|err| {
                FederationError::transient("the context conversation read failed", &err)
            })?,
        None => None,
    };
    Ok(ReplyLink {
        unresolved_ap_id: Some(parent_uri.to_owned()),
        context_conversation,
        ..ReplyLink::default()
    })
}

/// Step 4, quote arm: `quote_uri` resolves locally only — a stored, live
/// status with that `uri` becomes the quote, an unknown or tombstoned one
/// leaves it unfilled.
///
/// This arm never fetches: inbox handling stays fast, and the
/// `federator.quote_backfill` job queued after the write fetches an unknown
/// quoted object (step 12 of the [parent module](crate::services::ingest)).
pub(super) async fn resolve_quote(
    state: &AppState,
    quote_uri: Option<&str>,
) -> Result<Option<Uuid>, FederationError> {
    let Some(uri) = quote_uri else {
        return Ok(None);
    };
    let found = status::get_by_uri(&state.db, uri)
        .await
        .map_err(|err| FederationError::transient("the quote uri read failed", &err))?;
    Ok(found
        .filter(|row| row.deleted_at.is_none())
        .map(|row| row.id))
}

/// Step 5: turn the object's mention `href`s into the accounts it really
/// mentions, in document order, each counted once.
///
/// A `href` naming a stored account (local or a remote mirror — decision 7
/// keeps both in `users`, keyed by `ap_id`) resolves straight from the
/// database. An unknown `href` is fetched through
/// [`get_or_fetch_by_ap_id`] — at most [`MAX_MENTION_LOOKUPS`] of them per
/// status, each under its own [`MENTION_LOOKUP_TIMEOUT`] — and whatever is
/// still unknown afterwards is dropped: the mention stays text, the post
/// stands.
///
/// The inbound edit handler resolves an edited post's mentions through this
/// same function ([`crate::federation::handlers::note`]), so a mention the
/// edit adds lands on exactly the account the create's path would find.
pub(crate) async fn resolve_mentions(
    state: &AppState,
    hrefs: &[String],
) -> Result<Vec<User>, FederationError> {
    let mut seen: HashSet<&str> = HashSet::new();
    let unique: Vec<&str> = hrefs
        .iter()
        .filter(|href| seen.insert(href.as_str()))
        .map(String::as_str)
        .collect();
    let mut found: Vec<User> = Vec::new();
    let mut unknown: Vec<&str> = Vec::new();
    for href in unique {
        let stored = user::get_by_ap_id(&state.db, href)
            .await
            .map_err(|err| FederationError::transient("the mention read failed", &err))?;
        match stored {
            Some(account) => found.push(account),
            None if unknown.len() < MAX_MENTION_LOOKUPS => unknown.push(href),
            None => {}
        }
    }
    let fetched = join_all(
        unknown
            .iter()
            .map(|href| fetch_mention(state, (*href).to_owned())),
    )
    .await;
    found.extend(fetched.into_iter().flatten());
    Ok(found)
}

/// The local accounts a note addresses in `to`/`cc` (its
/// [`ParsedNote::audience`]), in document order, each once.
///
/// A silent mention in Mastodon's sense: only stored rows count, and only
/// `local` ones — an unknown or remote id is skipped and never fetched, as
/// a remote recipient needs no access grant here.
///
/// # Errors
///
/// [`FederationError::Transient`] when a lookup fails.
pub(crate) async fn resolve_local_audience(
    state: &AppState,
    audience: &[String],
) -> Result<Vec<User>, FederationError> {
    let mut found: Vec<User> = Vec::new();
    for ap_id in audience {
        let stored = user::get_by_ap_id(&state.db, ap_id)
            .await
            .map_err(|err| FederationError::transient("the audience read failed", &err))?;
        if let Some(account) = stored
            && account.local.get()
            && found.iter().all(|seen| seen.id != account.id)
        {
            found.push(account);
        }
    }
    Ok(found)
}

/// Appends each of `extra` to `mentioned` unless an account with its id is
/// already there.
pub(crate) fn merge_accounts(mentioned: &mut Vec<User>, extra: Vec<User>) {
    for account in extra {
        if mentioned.iter().all(|seen| seen.id != account.id) {
            mentioned.push(account);
        }
    }
}

/// One unknown mention's capped fetch: the [`User`] when the actor answers
/// inside [`MENTION_LOOKUP_TIMEOUT`] and parses into a mirror, [`None`] —
/// logged at debug — when it fails, refuses, or takes too long. A mention
/// lookup's failure is never an ingestion failure.
async fn fetch_mention(state: &AppState, href: String) -> Option<User> {
    match tokio::time::timeout(MENTION_LOOKUP_TIMEOUT, get_or_fetch_by_ap_id(state, &href)).await {
        Ok(Ok(account)) => Some(account),
        Ok(Err(err)) => {
            debug!(%err, mention = %href, "a mention actor could not be resolved; dropping the mention");
            None
        }
        Err(_elapsed) => {
            debug!(
                mention = %href,
                timeout_secs = MENTION_LOOKUP_TIMEOUT.as_secs(),
                "a mention actor ran past its lookup timeout; dropping the mention"
            );
            None
        }
    }
}

/// Step 6: upsert the `Emoji` tags the post announces, each under the
/// announcing author's domain — the same row shape the emoji endpoints
/// serve through `GET /api/v1/custom_emojis`, and idempotent, so a
/// redelivered Create rewrites the same images it wrote the first time. The
/// edit fold of an `Update` calls it too, so an emoji first used in an edit
/// is stored.
pub(crate) async fn upsert_emojis(
    state: &AppState,
    emojis: &[RemoteEmoji],
) -> Result<(), FederationError> {
    for emoji in emojis {
        custom_emoji::upsert_remote(
            &state.db,
            &emoji.shortcode,
            emoji.domain.as_deref(),
            &emoji.url,
        )
        .await
        .map_err(|err| FederationError::transient("the custom emoji upsert failed", &err))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
