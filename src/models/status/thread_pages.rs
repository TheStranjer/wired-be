//! The oldest-first reads behind a thread's two collections: the replies
//! of one status ([`public_replies_page_by`]) and everything in one
//! conversation ([`public_context_page`], [`public_context_count`]).
//!
//! Both answer only what an anonymous viewer may see
//! ([`status_visibility`]'s clause), keyset-paged on the status id in the
//! direction the collections are served in: oldest first.

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, models::status_visibility, pagination::PageParams};

use super::Status;

/// The head of the replies statement, up to (and including) the placeholder
/// the object id is pushed into.
const REPLIES_SQL: &str = "SELECT s.* FROM statuses s WHERE s.in_reply_to_id = ";

/// The head of the context page statement, up to the conversation id.
///
/// The function appends the reblog exclusion and the anonymous visibility
/// clause after the placeholder it pushes the id into.
const CONTEXT_PAGE_SQL: &str = "SELECT s.* FROM statuses s WHERE s.conversation_id = ";

/// The head of the context count statement; see [`CONTEXT_PAGE_SQL`].
const CONTEXT_COUNT_SQL: &str = "SELECT count(*) FROM statuses s WHERE s.conversation_id = ";

/// The reblog exclusion of the context reads: an `Announce` row carries
/// its own conversation id and is no object of the thread.
const NOT_A_REBLOG: &str = " AND s.reblog_of_id IS NULL";

/// Whose answers a replies read lists, relative to the answered status'
/// author.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyAuthors {
    /// Everyone's answers.
    Anyone,
    /// Only answers written by this account (the author's self-replies).
    Only(Uuid),
    /// Every answer except those written by this account.
    Others(Uuid),
}

/// One page of `object_id`'s public replies by `authors`, oldest first.
///
/// `page.min_id` moves the window forward (the `limit` rows just newer than
/// the cursor), `page.max_id` backward (the `limit` rows just older,
/// answered in the same oldest-first order); with no cursor the page is
/// the `limit` oldest replies. The other [`PageParams`] fields are not
/// read: those pages are newest-first, these never are.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn public_replies_page_by(
    pool: &PgPool,
    object_id: Uuid,
    authors: ReplyAuthors,
    page: &PageParams,
) -> AppResult<Vec<Status>> {
    let mut builder = QueryBuilder::<Postgres>::new(REPLIES_SQL);
    let _bound: &mut _ = builder.push_bind(object_id);
    match authors {
        ReplyAuthors::Anyone => {}
        ReplyAuthors::Only(author) => {
            let _only: &mut _ = builder.push(" AND s.user_id = ").push_bind(author);
        }
        ReplyAuthors::Others(author) => {
            let _others: &mut _ = builder.push(" AND s.user_id <> ").push_bind(author);
        }
    }
    status_visibility::push_visibility_clause(&mut builder, None, "s");
    ascending_window(pool, builder, page).await
}

/// The public statuses of conversation `conversation_id`, counted.
///
/// The same non-reblog rows [`public_context_page`] lists.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn public_context_count(pool: &PgPool, conversation_id: Uuid) -> AppResult<i64> {
    let mut builder = QueryBuilder::<Postgres>::new(CONTEXT_COUNT_SQL);
    let _bound: &mut _ = builder.push_bind(conversation_id).push(NOT_A_REBLOG);
    status_visibility::push_visibility_clause(&mut builder, None, "s");
    let count = builder.build_query_scalar::<i64>().fetch_one(pool).await?;
    Ok(count)
}

/// One page of conversation `conversation_id`'s public statuses.
///
/// Non-reblog rows, oldest first, cursored like [`public_replies_page_by`].
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn public_context_page(
    pool: &PgPool,
    conversation_id: Uuid,
    page: &PageParams,
) -> AppResult<Vec<Status>> {
    let mut builder = QueryBuilder::<Postgres>::new(CONTEXT_PAGE_SQL);
    let _bound: &mut _ = builder.push_bind(conversation_id).push(NOT_A_REBLOG);
    status_visibility::push_visibility_clause(&mut builder, None, "s");
    ascending_window(pool, builder, page).await
}

/// Appends `page`'s cursors, the id ordering and the `LIMIT` to `builder`
/// and reads the rows, oldest first.
///
/// A `max_id` page is read from the cursor backwards — the newest rows
/// below it come off the index first — and flipped, so the answer keeps
/// the oldest-first order every page is served in.
async fn ascending_window(
    pool: &PgPool,
    mut builder: QueryBuilder<Postgres>,
    page: &PageParams,
) -> AppResult<Vec<Status>> {
    let backwards = page.max_id.is_some();
    if let Some(max_id) = page.max_id {
        let _older: &mut _ = builder.push(" AND s.id < ").push_bind(max_id);
    } else if let Some(min_id) = page.min_id {
        let _newer: &mut _ = builder.push(" AND s.id > ").push_bind(min_id);
    }
    let _paged: &mut _ = builder
        .push(if backwards {
            " ORDER BY s.id DESC"
        } else {
            " ORDER BY s.id ASC"
        })
        .push(" LIMIT ")
        .push_bind(page.limit);
    let mut rows = builder.build_query_as::<Status>().fetch_all(pool).await?;
    if backwards {
        rows.reverse();
    }
    Ok(rows)
}

#[cfg(test)]
mod tests;
