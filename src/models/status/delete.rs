//! Soft deletion: turning live rows into tombstones ([`soft_delete`],
//! [`soft_delete_with_reblogs`], [`soft_delete_all_for_user`]), the
//! born-tombstone that reserves a `uri` before a post ever arrives
//! ([`insert_tombstone`]), the expiry batch read ([`fetch_expired`]) and the
//! counter and pin steps every delete owes around the tombstone.

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{error::AppResult, models::featured_tag, util::id::new_id};

use super::{Status, counters::uncount_deleted};

/// The tombstone assignment, as a literal so [`concat!`] can splice it into
/// every statement that turns a status row into a tombstone. A column a
/// tombstone must clear is added here, once.
macro_rules! tombstone_set {
    () => {
        "SET content = '', raw_text = '', spoiler_text = '', deleted_at = now(), \
         updated_at = now()"
    };
}

pub(super) use tombstone_set;

/// [`soft_delete`]: a live row becomes a tombstone once, which is what makes
/// the counter steps of the same transaction run exactly once per status.
const SOFT_DELETE_SQL: &str = concat!(
    "UPDATE statuses ",
    tombstone_set!(),
    " WHERE id = $1 AND deleted_at IS NULL RETURNING *"
);

/// [`soft_delete_with_reblogs`]: the same tombstone [`soft_delete`] writes,
/// for every live reblog of `id` at once. `RETURNING *` brings those rows back
/// so their own counters can be stepped down in the same transaction.
const SOFT_DELETE_REBLOGS_SQL: &str = concat!(
    "UPDATE statuses ",
    tombstone_set!(),
    " WHERE reblog_of_id = $1 AND deleted_at IS NULL RETURNING *"
);

/// [`soft_delete`] and [`soft_delete_with_reblogs`]: every account's pin of a
/// status that has just become a tombstone goes with it. A pin is a promise
/// that a profile can show the post, and a tombstone shows nowhere, so the
/// row would name something no listing answers; this is the soft delete's
/// share of what the foreign key's cascade does for a hard one (see
/// [`crate::models::pinned_status`]).
const CLEAR_PINS_SQL: &str = "DELETE FROM pinned_statuses WHERE status_id = $1";

/// Soft-deletes status `id`: the row stays, as a tombstone.
///
/// Its text is cleared and its `uri` reserved, the counters it fed step
/// back down in the same transaction — its author's `statuses_count`, its
/// parent's `replies_count` and, for a reblog or a quote, the original's
/// `reblogs_count` or `quotes_count` — and every account's pin of it is
/// removed there too,
/// the soft twin of the cascade a hard delete rides (see
/// [`crate::models::pinned_status`]).
///
/// Idempotent: a row that is already a tombstone, and an unknown id, are both
/// answered `None` and move nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn soft_delete(pool: &PgPool, id: Uuid) -> AppResult<Option<Status>> {
    let mut tx = pool.begin().await?;
    let Some(tombstone) = mark_deleted(&mut tx, id).await? else {
        return Ok(None);
    };
    uncount_deleted(&mut tx, &tombstone).await?;
    clear_pins(&mut tx, id).await?;
    featured_tag::refresh_conn(&mut tx, tombstone.user_id, Some(id)).await?;
    tx.commit().await?;
    Ok(Some(tombstone))
}

/// Soft-deletes status `id` **and every reblog of it** in one transaction.
///
/// The answer is `id`'s own tombstone beside the tombstoned reblog rows (empty
/// when nobody boosted it), which the caller needs to tell the boosters'
/// followers.
///
/// A post that is gone takes its boosts with it, so no live row is left
/// pointing at a tombstone: every reblog of `id` is tombstoned exactly as
/// [`soft_delete`] would tombstone it, and each one's counters step down in
/// the same transaction — its author's `statuses_count`, and above all
/// [`reblogs_count`](Status::reblogs_count) of the status being deleted, which
/// lands at zero.
///
/// Idempotent in the same shape as [`soft_delete`]: an id that is already a
/// tombstone, and an unknown id, are answered `None` and move nothing —
/// reblogs included, since an already deleted original has nothing left to
/// take down.
///
/// The media attachments of the deleted rows are left alone: they keep their
/// `status_id` and stay in the table, referenced by no live status, until the
/// cleanup job that removes them exists.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError), and the rollback takes the
/// half-written delete with it: a status and its reblogs are tombstoned
/// together or not at all.
pub async fn soft_delete_with_reblogs(
    pool: &PgPool,
    id: Uuid,
) -> AppResult<Option<(Status, Vec<Status>)>> {
    let mut tx = pool.begin().await?;
    let Some(tombstone) = mark_deleted(&mut tx, id).await? else {
        return Ok(None);
    };
    uncount_deleted(&mut tx, &tombstone).await?;
    clear_pins(&mut tx, id).await?;
    featured_tag::refresh_conn(&mut tx, tombstone.user_id, Some(id)).await?;
    let reblogs = tombstone_reblogs(&mut tx, id).await?;
    for reblog in &reblogs {
        uncount_deleted(&mut tx, reblog).await?;
        clear_pins(&mut tx, reblog.id).await?;
    }
    tx.commit().await?;
    Ok(Some((tombstone, reblogs)))
}

/// How many rows one pass of [`soft_delete_all_for_user`] tombstones: the
/// account-deletion job's batch size, small enough to keep every statement
/// short and large enough that most accounts tombstone in one pass.
const SOFT_DELETE_BATCH: i64 = 100;

/// The statement behind [`soft_delete_all_for_user`]: one pass, in one
/// statement, over at most [`SOFT_DELETE_BATCH`] of the account's live rows,
/// oldest id first.
///
/// `gone` writes the tombstone [`soft_delete`] writes and hands back the links
/// of the rows it turned. `pins` removes every pin of those rows. `targets`
/// counts, per linked status, how many of the tombstoned rows answered,
/// reblogged or quoted it, and `stepped` takes those counts off the linked
/// statuses in **one** `UPDATE` (a row written twice by one statement keeps
/// only one of the writes, so the three counters cannot be three updates).
/// Rows that are themselves in this batch are left out of `stepped`: they
/// are tombstoned by `gone`. The answer is the number of rows tombstoned.
const SOFT_DELETE_BATCH_FOR_USER_SQL: &str = concat!(
    "WITH gone AS (
         UPDATE statuses ",
    tombstone_set!(),
    " WHERE id IN (
             SELECT id FROM statuses
              WHERE user_id = $1 AND deleted_at IS NULL
              ORDER BY id
              LIMIT $2
         )
         RETURNING id, in_reply_to_id, reblog_of_id, quote_of_id
     ), pins AS (
         DELETE FROM pinned_statuses WHERE status_id IN (SELECT id FROM gone)
     ), targets AS (
         SELECT id, sum(replies)::int AS replies, sum(reblogs)::int AS reblogs,
                sum(quotes)::int AS quotes
           FROM (
                SELECT in_reply_to_id AS id, 1 AS replies, 0 AS reblogs, 0 AS quotes
                  FROM gone WHERE in_reply_to_id IS NOT NULL
                UNION ALL
                SELECT reblog_of_id, 0, 1, 0 FROM gone WHERE reblog_of_id IS NOT NULL
                UNION ALL
                SELECT quote_of_id, 0, 0, 1 FROM gone WHERE quote_of_id IS NOT NULL
           ) links
          GROUP BY id
     ), stepped AS (
         UPDATE statuses s
            SET replies_count = GREATEST(0, s.replies_count - t.replies),
                reblogs_count = GREATEST(0, s.reblogs_count - t.reblogs),
                quotes_count = GREATEST(0, s.quotes_count - t.quotes),
                updated_at = now()
           FROM targets t
          WHERE s.id = t.id AND s.id NOT IN (SELECT id FROM gone)
     )
     SELECT count(*) FROM gone"
);

/// Soft-deletes every status authored by user `user_id`: the status step of
/// the [`users.delete`](crate::jobs::deletion::KIND_DELETE) job.
///
/// `SOFT_DELETE_BATCH` rows per statement, repeated until a pass
/// tombstones nothing — the loop ends however many posts the account left,
/// and a rerun over an already tombstoned set is a no-op, so the job can
/// retry the step safely. Returns the total number of rows the passes
/// tombstoned.
///
/// What the per-status [`soft_delete`] does around the tombstone is done in
/// the same statement as the tombstone, per batch: every tombstoned reply,
/// reblog and quote steps `replies_count`, `reblogs_count` or `quotes_count`
/// of the live status it fed back down, and every pin of a tombstoned row is
/// removed. Only the author's own `statuses_count` is left alone: the
/// author's row is purged and deactivated by the same job, so its
/// denormalised counters describe nothing anyone will read.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn soft_delete_all_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<u64> {
    let mut total = 0_u64;
    loop {
        let tombstoned = sqlx::query_scalar::<_, i64>(SOFT_DELETE_BATCH_FOR_USER_SQL)
            .bind(user_id)
            .bind(SOFT_DELETE_BATCH)
            .fetch_one(pool)
            .await?;
        let tombstoned = u64::try_from(tombstoned).unwrap_or(0);
        if tombstoned == 0 {
            return Ok(total);
        }
        total += tombstoned;
    }
}

/// The [`insert_tombstone`] write: a status row that is born a tombstone.
/// Everything the column defaults can answer — the emptied text, the plain
/// content type, the zeroed counters — is left to the table; the statement
/// names only what has no default. `ON CONFLICT (uri) DO NOTHING` keeps the
/// write idempotent, and the missing `RETURNING` row is how the caller
/// learns somebody else already reserved that `uri`.
const INSERT_TOMBSTONE_SQL: &str = "INSERT INTO statuses (
     id, uri, user_id, visibility, conversation_id, local, deleted_at
) VALUES (
     $1, $2, $3, 'public', $1, false, now()
) ON CONFLICT (uri) DO NOTHING
  RETURNING *";

/// Reserves object uri `uri` as a tombstone of author `user_id`: the row a
/// [`Delete`](crate::federation::handlers::note) for a status this instance
/// never stored leaves behind.
///
/// The point is the unique `uri`: a later `Create` of the same object meets
/// [`insert_tombstone`]'s row in the ingest's tombstone check and is
/// ignored, so a post deleted before it arrived never arrives at all. The
/// row carries nothing else — no text, no counters, no author-count bump,
/// since it was never a post — and shares a soft delete's reads: hidden
/// from every timeline and visibility check that filters `deleted_at`.
///
/// `conversation_id` is the new row's own id, the root-status convention
/// [`insert`](super::insert) keeps; a tombstone is its own (empty) conversation.
///
/// Returns the stored row, or `None` when a row — tombstone or live —
/// already carried `uri`.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn insert_tombstone(
    pool: &PgPool,
    uri: &str,
    user_id: Uuid,
) -> AppResult<Option<Status>> {
    Ok(sqlx::query_as::<_, Status>(INSERT_TOMBSTONE_SQL)
        .bind(new_id())
        .bind(uri)
        .bind(user_id)
        .fetch_optional(pool)
        .await?)
}

/// The `SELECT` behind [`fetch_expired`]: live local statuses whose
/// expiration has run out, soonest-expiring first, at most `limit` of them.
/// A remote status is never expired here, whatever its `expires_at` says:
/// only a received `Delete` removes it.
const EXPIRED_SQL: &str = "SELECT * FROM statuses
     WHERE deleted_at IS NULL
       AND local
       AND expires_at IS NOT NULL
       AND expires_at <= now()
     ORDER BY expires_at, id
     LIMIT $1";

/// Fetches at most `limit` live statuses whose
/// [`expires_at`](Status::expires_at) has run out, soonest expiry first.
///
/// This is the batch read of the
/// [`statuses.expire_sweep`](crate::jobs::statuses::KIND_SWEEP) safety
/// job. A tombstoned row no longer matches the predicate and a
/// scheduled delete clears the row's own expiry from the queue's reach, so
/// repeated batches of one call drain the backlog in order without ever
/// handing the same row out twice.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn fetch_expired(pool: &PgPool, limit: i64) -> AppResult<Vec<Status>> {
    let rows = sqlx::query_as::<_, Status>(EXPIRED_SQL)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Turns the live row `id` into a tombstone on `conn`, returning it, or
/// `None` when there was no live row to tombstone.
async fn mark_deleted(conn: &mut PgConnection, id: Uuid) -> AppResult<Option<Status>> {
    let tombstone = sqlx::query_as::<_, Status>(SOFT_DELETE_SQL)
        .bind(id)
        .fetch_optional(conn)
        .await?;
    Ok(tombstone)
}

/// Removes every pin of the deleted status `id` on `conn`: the soft delete's
/// counterpart of the cascade a hard delete rides, and what keeps
/// [`count`](crate::models::pinned_status::count) counting only pins a
/// profile can still show.
pub(super) async fn clear_pins(conn: &mut PgConnection, id: Uuid) -> AppResult<()> {
    sqlx::query(CLEAR_PINS_SQL)
        .bind(id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

/// Turns every live reblog of `id` into a tombstone on `conn`, answering the
/// rows it turned so their counters can step down with them.
async fn tombstone_reblogs(conn: &mut PgConnection, id: Uuid) -> AppResult<Vec<Status>> {
    let reblogs = sqlx::query_as::<_, Status>(SOFT_DELETE_REBLOGS_SQL)
        .bind(id)
        .fetch_all(conn)
        .await?;
    Ok(reblogs)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tombstone_sql_tests {
    //! The four tombstone statements share one `SET` clause.

    use super::super::reblogs::TOMBSTONE_REBLOG_SQL;
    use super::*;

    /// Every statement that tombstones a status clears the text and stamps
    /// `deleted_at`, because they all splice in [`tombstone_set!`].
    #[test]
    fn every_tombstone_statement_carries_the_shared_set_clause() {
        let statements = [
            ("SOFT_DELETE_SQL", SOFT_DELETE_SQL),
            ("SOFT_DELETE_REBLOGS_SQL", SOFT_DELETE_REBLOGS_SQL),
            (
                "SOFT_DELETE_BATCH_FOR_USER_SQL",
                SOFT_DELETE_BATCH_FOR_USER_SQL,
            ),
            ("TOMBSTONE_REBLOG_SQL", TOMBSTONE_REBLOG_SQL),
        ];
        for (name, sql) in statements {
            assert!(sql.contains(tombstone_set!()), "{name} splices the clause");
            assert!(
                sql.contains("deleted_at = now()"),
                "{name} stamps deleted_at"
            );
            assert!(
                sql.contains("spoiler_text = ''"),
                "{name} clears spoiler_text"
            );
        }
    }
}
