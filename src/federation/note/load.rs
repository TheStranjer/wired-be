//! Reading a [`NoteContext`] from the database: [`load_note_context`] and
//! the satellite reads it runs.
//!
//! The parent uri, the mentions, the hashtags, the media, the custom
//! emojis, the poll and the quote — plus the shortcode scan the emoji read
//! feeds. The builder itself stays pure; the clock questions
//! ([`PollData`]'s `closed`) are answered here.

use std::collections::HashSet;

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    models::{
        activitypub_actor,
        custom_emoji::{self, CustomEmoji},
        hashtag,
        media_attachment::{self, MediaAttachment},
        mention, poll, quote_request,
        status::{self, Status, Visibility},
        user::{self, User},
    },
    pagination::PageParams,
    util::emoji::is_shortcode_char,
};

use super::{NoteContext, PollData, PollOptionData, SELF_REPLIES_LIMIT};

/// Reads everything [`note_json`](super::note_json) needs for status `status_id`.
///
/// The row and its author, the resolved parent uri, the mentions with the
/// replied-to author appended when the post does not already name them, the
/// hashtags, the media, the poll with its options and close state, the
/// quote, and the local custom-emoji rows for the shortcodes the text uses.
///
/// A tombstoned or unknown id is [`AppError::NotFound`]: this loads the
/// documents a live status federates as; the `Delete` side has its own
/// builder ([`tombstone_json`](super::tombstone_json)) and reads the row it already holds.
///
/// # Errors
///
/// [`AppError::NotFound`] when no live status (or no author row behind it)
/// carries `status_id`; any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn load_note_context(
    pool: &PgPool,
    config: &Config,
    status_id: Uuid,
) -> AppResult<NoteContext> {
    let status = status::get_by_id(pool, status_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let author = user::get_by_id(pool, status.user_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let author_followers_url = activitypub_actor::get(pool, author.id)
        .await?
        .and_then(|row| row.followers_url);
    let reply_parent_uri = parent_uri(pool, &status).await?;
    let mentions = load_mentions(pool, &status).await?;
    let hashtags = load_hashtags(pool, &status).await?;
    let attachments = load_attachments(pool, &status).await?;
    let poll_data = load_poll(pool, &status).await?;
    let emojis = load_emojis(pool, &status, poll_data.as_ref()).await?;
    let quote_uri = load_quote_uri(pool, &status).await?;
    let quote_authorization = quote_request::accepted_authorization(pool, status.id).await?;
    let self_replies = load_self_replies(pool, &status).await?;
    let thread_context = if status.local {
        status::remote_thread_context(pool, status.conversation_id).await?
    } else {
        None
    };
    Ok(NoteContext {
        status,
        author,
        author_followers_url,
        reply_parent_uri,
        mentions,
        hashtags,
        attachments,
        emojis,
        poll: poll_data,
        quote_uri,
        quote_authorization,
        self_replies,
        thread_context,
        config: config.clone(),
    })
}

/// The object ids of the author's own public answers to `status`.
///
/// Oldest first, at most [`SELF_REPLIES_LIMIT`]: the items of the
/// `replies` collection's inline first page. A `private`, `direct` or
/// `local` status advertises no `replies` and costs no read.
async fn load_self_replies(pool: &PgPool, status: &Status) -> AppResult<Vec<String>> {
    if !matches!(status.visibility, Visibility::Public | Visibility::Unlisted) {
        return Ok(Vec::new());
    }
    let window = PageParams {
        max_id: None,
        min_id: None,
        since_id: None,
        limit: SELF_REPLIES_LIMIT,
        offset: None,
    };
    let rows = status::public_replies_page_by(
        pool,
        status.id,
        status::ReplyAuthors::Only(status.user_id),
        &window,
    )
    .await?;
    Ok(rows.into_iter().map(|row| row.uri).collect())
}

/// The answered status's object id: the stored parent's `uri` (a parent
/// deleted after the answer still owns the address), else the
/// [`Status::in_reply_to_ap_id`] the dispatcher parked for a parent this
/// instance has not stored.
async fn parent_uri(pool: &PgPool, status: &Status) -> AppResult<Option<String>> {
    if let Some(parent_id) = status.in_reply_to_id {
        let parent = status::get_by_id_including_deleted(pool, parent_id).await?;
        if let Some(row) = parent {
            return Ok(Some(row.uri));
        }
    }
    Ok(status.in_reply_to_ap_id.clone())
}

/// The status's mentions in link order, with the replied-to author
/// appended when the post answers them without naming them — Pleroma
/// addresses a reply's parent author the same way, and the addressing
/// rules of [`note_json`](super::note_json) read it off this list.
///
/// The reader is `pub(crate)` for the one document that repeats a status'
/// audience without rendering the status: the `Delete` a withdrawal sends
/// over the mention links of a row the database no longer answers for
/// ([`crate::services::federation_out::after_status_deleted`]).
pub(crate) async fn load_mentions(pool: &PgPool, status: &Status) -> AppResult<Vec<User>> {
    let mut mentions = mention::for_statuses(pool, &[status.id])
        .await?
        .remove(&status.id)
        .unwrap_or_default();
    if let Some(parent_author_id) = status.in_reply_to_account_id
        && !mentions.iter().any(|person| person.id == parent_author_id)
        && let Some(parent_author) = user::get_by_id(pool, parent_author_id).await?
    {
        mentions.push(parent_author);
    }
    Ok(mentions)
}

/// The status's hashtag names, in the link read's order.
async fn load_hashtags(pool: &PgPool, status: &Status) -> AppResult<Vec<String>> {
    let tags = hashtag::for_statuses(pool, &[status.id])
        .await?
        .remove(&status.id)
        .unwrap_or_default();
    Ok(tags.into_iter().map(|tag| tag.name).collect())
}

/// The status's media, in position order.
async fn load_attachments(pool: &PgPool, status: &Status) -> AppResult<Vec<MediaAttachment>> {
    Ok(media_attachment::for_statuses(pool, &[status.id])
        .await?
        .remove(&status.id)
        .unwrap_or_default())
}

/// The enabled local emoji rows for every `:shortcode:` the status's text,
/// content warning or poll options carry — in shortcode order (the table
/// read's), which is all the tag list needs since the builder re-checks
/// each code against the same texts.
async fn load_emojis(
    pool: &PgPool,
    status: &Status,
    poll_data: Option<&PollData>,
) -> AppResult<Vec<CustomEmoji>> {
    let codes = used_shortcodes(status, poll_data);
    if codes.is_empty() {
        return Ok(Vec::new());
    }
    let rows = custom_emoji::get_by_shortcodes(pool, &codes, None).await?;
    Ok(rows.into_iter().filter(|emoji| !emoji.disabled).collect())
}

/// The poll of `status` as [`PollData`], `None` when it carries none. The
/// close state is read here, against the load-time clock: by the moment
/// the document federates, a poll whose deadline has run out is closed.
async fn load_poll(pool: &PgPool, status: &Status) -> AppResult<Option<PollData>> {
    let Some(row) = poll::get_for_status(pool, status.id).await? else {
        return Ok(None);
    };
    let options = poll::options(pool, row.id).await?;
    Ok(Some(PollData {
        multiple: row.multiple,
        expires_at: row.expires_at,
        closed: row.expires_at.is_some_and(|at| at <= Utc::now()),
        voters_count: row.voters_count,
        options: options
            .iter()
            .map(|option| PollOptionData {
                title: option.title.clone(),
                votes_count: option.votes_count,
            })
            .collect(),
    }))
}

/// The quoted status's object id, `None` for anything but a quote; a
/// quoted post that has since been deleted keeps its reserved uri.
async fn load_quote_uri(pool: &PgPool, status: &Status) -> AppResult<Option<String>> {
    match status.quote_of_id {
        Some(quote_id) => Ok(status::get_by_id_including_deleted(pool, quote_id)
            .await?
            .map(|row| row.uri)),
        None => Ok(None),
    }
}

/// The distinct `:shortcode:` texts the status's raw text, rendered HTML,
/// content warning and poll option titles carry, in order of first
/// appearance.
fn used_shortcodes(status: &Status, poll_data: Option<&PollData>) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut codes: Vec<String> = Vec::new();
    let options = poll_data.iter().flat_map(|poll| &poll.options);
    let texts = [&status.raw_text, &status.content, &status.spoiler_text]
        .into_iter()
        .chain(options.map(|option| &option.title));
    for text in texts {
        for code in shortcodes_in(text) {
            if seen.insert(code.clone()) {
                codes.push(code);
            }
        }
    }
    codes
}

/// Every `:shortcode:` reference in `text`: a colon, one or more
/// `[A-Za-z0-9_+-]`, a colon — the shape [`crate::util::emoji::shortcode`]
/// recognises, scanned out of running prose. A colon after an incomplete
/// candidate starts a new one (so `:a::b:` names both), and a candidate
/// that never closes is dropped.
fn shortcodes_in(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut candidate: Option<String> = None;
    for symbol in text.chars() {
        match symbol {
            ':' => {
                if let Some(code) = candidate.take()
                    && !code.is_empty()
                {
                    found.push(code);
                }
                candidate = Some(String::new());
            }
            other if is_shortcode_char(other) => {
                if let Some(code) = candidate.as_mut() {
                    code.push(other);
                }
            }
            _ => candidate = None,
        }
    }
    found
}

#[cfg(test)]
mod tests;
