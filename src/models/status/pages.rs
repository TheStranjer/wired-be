//! The paging reads over stored statuses: the author's feed page
//! ([`newest_public_posts`]) and outbox page ([`outbox_page`]), the backup
//! export walk ([`backup_page`]), the replies collection count and page
//! ([`public_replies_count`], [`public_replies_page`]) and the admin's
//! visibility counters ([`status_visibility_counts`]).

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{error::AppResult, pagination::PageParams};

use super::{Status, StatusVisibilityCounts};

/// [`newest_public_posts`]: one author's live `public` root posts (a reply
/// or a reblog is neither), newest id first (decision 4), optionally a
/// keyset page older than the cursor. `$2` is bound `NULL` on a first page,
/// which keeps every row.
const NEWEST_PUBLIC_POSTS_SQL: &str = "SELECT * FROM statuses
   WHERE user_id = $1
     AND visibility = 'public'
     AND deleted_at IS NULL
     AND in_reply_to_id IS NULL
     AND reblog_of_id IS NULL
     AND ($2::uuid IS NULL OR id < $2)
ORDER BY id DESC
    LIMIT $3";

/// [`outbox_page`]: the head of the author's outbox statement, up to (and
/// including) the placeholder [`outbox_page`] pushes the author's id into;
/// [`PageParams::apply_keyset`] then appends the cursors, the ordering and
/// the `LIMIT` on `s.id`.
const OUTBOX_PAGE_SQL: &str = "SELECT s.* FROM statuses s WHERE s.user_id = ";

/// The visibility arms of the two `ActivityPub` listing reads: a row the
/// fediverse may be shown is `public` or `unlisted` — `private`, `direct`
/// and Pleroma's never-federated `local` belong to nobody's outbox or
/// public replies collection — and a tombstone is no post at all.
const AP_VISIBLE: &str = " AND s.deleted_at IS NULL AND s.visibility IN ('public', 'unlisted')";

/// [`backup_page`]: the head of the backup archive statement, up to (and
/// including) the placeholder [`backup_page`] pushes the author's id into;
/// the function then appends [`BACKUP_VISIBLE`], the `after` cursor, the
/// ascending ordering and the `LIMIT` on `s.id`.
const BACKUP_PAGE_SQL: &str = "SELECT s.* FROM statuses s WHERE s.user_id = ";

/// The row filter of [`backup_page`]: every live status an account
/// authored that this instance federates. Unlike [`AP_VISIBLE`]'s two
/// public arms the backup is the account's own export, so `private` and
/// `direct` ride along too; only Pleroma's never-federated `local` stays
/// out (it has no `Create` to carry — [`note_json`] refuses it) and no
/// tombstone is anyone's post any more.
///
/// [`note_json`]: crate::federation::note::note_json
const BACKUP_VISIBLE: &str = " AND s.deleted_at IS NULL \
    AND s.visibility IN ('public', 'unlisted', 'private', 'direct')";

/// [`public_replies_page`]: the head of the replies statement, up to (and
/// including) the placeholder the object id is pushed into; the function
/// then appends [`AP_VISIBLE`], the cursor, the ordering and the `LIMIT`.
const REPLIES_PAGE_SQL: &str = "SELECT s.* FROM statuses s WHERE s.in_reply_to_id = ";

/// [`public_replies_count`]: the same live `public`/`unlisted` answers
/// [`public_replies_page`] pages through, counted rather than listed; the
/// `count(*)` `bigint` arrives as the `i64` the JSON number carries.
const PUBLIC_REPLIES_COUNT_SQL: &str = "SELECT count(*) FROM statuses s
      WHERE s.in_reply_to_id = $1
        AND s.deleted_at IS NULL
        AND s.visibility IN ('public', 'unlisted')";

/// The newest `limit` live, `public`, root posts of author `user_id`,
/// newest first (decision 4): the page the account's `Atom`/`RSS` feeds
/// ([`crate::api::feeds`]) enumerate.
///
/// A root post is neither a reply nor a reblog — the feed carries what the
/// account itself said, not its side of a conversation or its boosts — and
/// `public` is the only visibility that is (an `unlisted` post is public to
/// timelines but not advertised, `private`, `direct` and `local` are not
/// public at all). `max_id`, when given, is the keyset cursor: only posts
/// strictly older than it are returned.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn newest_public_posts(
    pool: &PgPool,
    user_id: Uuid,
    max_id: Option<Uuid>,
    limit: i64,
) -> AppResult<Vec<Status>> {
    let statuses = sqlx::query_as::<_, Status>(NEWEST_PUBLIC_POSTS_SQL)
        .bind(user_id)
        .bind(max_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(statuses)
}

/// The `user_id`'s outbox page: their publicly visible rows, newest first.
///
/// Live `public` and `unlisted` rows — posts, replies and reblogs alike —
/// with `page`'s cursors and `limit` applied on the status id
/// (decision 4). This is the read behind
/// `GET /users/{nickname}/outbox?page=true` (Pleroma's
/// `ActivityPubController.outbox`, fed by `fetch_user_activities`); who may
/// see it is the route's gate, not this query's.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn outbox_page(
    pool: &PgPool,
    user_id: Uuid,
    page: &PageParams,
) -> AppResult<Vec<Status>> {
    let mut builder = QueryBuilder::<Postgres>::new(OUTBOX_PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(user_id).push(AP_VISIBLE);
    page.apply_keyset(&mut builder, "s.id");
    let rows = builder.build_query_as::<Status>().fetch_all(pool).await?;
    Ok(page.finish(rows))
}

/// One ascending page of `user_id`'s statuses for the backup archive
/// ([`crate::jobs::backups`]).
///
/// Every live row the account authored that this instance federates — all
/// four visibilities but `local`, which has no document to export (see
/// `BACKUP_VISIBLE`) — oldest id first (decision 4), which is the order
/// the archive's `orderedItems` must take. `after`, when given, is the
/// keyset cursor: only rows with a strictly greater id come back, at
/// `limit` at a time, so a caller walking a whole account never holds more
/// than one page and never re-reads a row it has seen. This is the shape
/// [`outbox_page`] has with [`PageParams`], narrowed to the one walk the
/// export takes rather than a client-facing cursor set.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn backup_page(
    pool: &PgPool,
    user_id: Uuid,
    after: Option<Uuid>,
    limit: i64,
) -> AppResult<Vec<Status>> {
    let mut builder = QueryBuilder::<Postgres>::new(BACKUP_PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(user_id).push(BACKUP_VISIBLE);
    if let Some(after) = after {
        let _newer: &mut _ = builder.push(" AND s.id > ").push_bind(after);
    }
    let _paged: &mut _ = builder.push(" ORDER BY s.id ASC LIMIT ").push_bind(limit);
    let rows = builder.build_query_as::<Status>().fetch_all(pool).await?;
    Ok(rows)
}

/// The publicly visible answers to `object_id`, counted rather than listed.
///
/// The same live `public`/`unlisted` rows [`public_replies_page`] pages
/// through; the `count(*)` `bigint` arrives as the `i64` the JSON number
/// carries. This is the `totalItems` of `GET /objects/{uuid}/replies`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn public_replies_count(pool: &PgPool, object_id: Uuid) -> AppResult<i64> {
    let row = sqlx::query_scalar::<_, i64>(PUBLIC_REPLIES_COUNT_SQL)
        .bind(object_id)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// The start of [`status_visibility_counts`]' statement: the four
/// `count(*) FILTER` arms over the live, non-reblog rows, joined to the
/// author so the audience clause can narrow by origin. The clause
/// [`status_visibility_counts`] appends (local authors, or one domain's)
/// is Pleroma's `CounterCache` distinction between `get_sum` and
/// `get_by_instance` restated over this schema's `users.local` /
/// `users.domain` columns.
const STATUS_VISIBILITY_COUNTS_SQL: &str = "SELECT
     count(*) FILTER (WHERE s.visibility = 'public')  AS public,
     count(*) FILTER (WHERE s.visibility = 'unlisted') AS unlisted,
     count(*) FILTER (WHERE s.visibility = 'private')  AS private,
     count(*) FILTER (WHERE s.visibility = 'direct')   AS direct
     FROM statuses s
     JOIN users u ON u.id = s.user_id
     WHERE s.deleted_at IS NULL AND s.reblog_of_id IS NULL AND ";

/// The four visibility counters behind `GET /api/v1/pleroma/admin/stats`.
///
/// A status is counted once, under its own visibility, when it is neither
/// a tombstone nor a reblog row (the boost repeats somebody else's post;
/// Pleroma's `activity_visibility` never counts `Announce` activities).
/// `instance` selects the audience of the count: with `None` the
/// answer is this instance's own — the statuses of local accounts; with
/// a domain it is the statuses of the accounts mirrored from that domain
/// (Pleroma's `CounterCache.get_by_instance`, keyed on the actor's host).
/// A domain no account came from answers all zeros, like Pleroma's
/// missing `counter_cache` row does.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn status_visibility_counts(
    pool: &PgPool,
    instance: Option<&str>,
) -> AppResult<StatusVisibilityCounts> {
    let mut builder = QueryBuilder::<Postgres>::new(STATUS_VISIBILITY_COUNTS_SQL);
    match instance {
        None => {
            let _local: &mut _ = builder.push("u.local");
        }
        Some(domain) => {
            let _domain: &mut _ = builder.push("u.domain = ").push_bind(domain);
        }
    }
    let counts = builder
        .build_query_as::<StatusVisibilityCounts>()
        .fetch_one(pool)
        .await?;
    Ok(counts)
}

/// One page of `object_id`'s publicly visible replies, oldest first.
///
/// `min_id` moves the window forward (the `limit` rows just newer than the
/// cursor), `max_id` backward (the `limit` rows just older, answered in the
/// same oldest-first order); neither reads the rows the other side of the
/// cursor hides, and both together narrow to the window between. With no
/// cursor the page is the `limit` oldest replies of all.
///
/// The direction is the replies collection's own (Pleroma's
/// `object_replies` runs its keyset `:asc`), which is why this read takes
/// its cursors plainly rather than through [`PageParams`], whose pages are
/// all newest-first.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn public_replies_page(
    pool: &PgPool,
    object_id: Uuid,
    min_id: Option<Uuid>,
    max_id: Option<Uuid>,
    limit: i64,
) -> AppResult<Vec<Status>> {
    let mut builder = QueryBuilder::<Postgres>::new(REPLIES_PAGE_SQL);
    let _bound: &mut _ = builder.push_bind(object_id).push(AP_VISIBLE);
    // A `max_id` page is read from the cursor backwards — the newest rows
    // below it come off the index first — and flipped, so the answer keeps
    // the oldest-first order every replies page is served in.
    let backwards = max_id.is_some();
    if let Some(max_id) = max_id {
        let _older: &mut _ = builder.push(" AND s.id < ").push_bind(max_id);
    } else if let Some(min_id) = min_id {
        let _newer: &mut _ = builder.push(" AND s.id > ").push_bind(min_id);
    }
    let _paged: &mut _ = builder
        .push(if backwards {
            " ORDER BY s.id DESC"
        } else {
            " ORDER BY s.id ASC"
        })
        .push(" LIMIT ")
        .push_bind(limit);
    let mut rows = builder.build_query_as::<Status>().fetch_all(pool).await?;
    if backwards {
        rows.reverse();
    }
    Ok(rows)
}

#[cfg(test)]
mod tests;
