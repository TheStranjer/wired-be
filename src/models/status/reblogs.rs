//! Reblogs — decision 6's `Announce` rows: the idempotent boost write
//! ([`create_reblog`]) and its withdrawal ([`remove_reblog`]), the batched
//! `reblogged` flag ([`reblogged_status_ids`]) and the reads that name the
//! boosters ([`reblogger_page`], [`local_reblogger_ids`]).

use std::collections::HashSet;

use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::user::User,
    pagination::PageParams,
    util::id::{activity_ap_id, new_id},
};

use super::{
    ReblogPageEntry, Status, Visibility,
    counters::{count_new_post, uncount_deleted},
    delete::clear_pins,
};

/// The `INSERT` behind [`create_reblog`]: a reblog row is decision 6's
/// `Announce` wearing status clothing — no text of its own (the `content`,
/// `raw_text`, `content_type` and `spoiler_text` column defaults empty it),
/// the original's conversation, `local` set, and the `{BASE_URL}/activities/
/// {id}` uri rather than a plain status's `/objects/{id}`. It rides the
/// partial unique index on `(user_id, reblog_of_id)`: a repeat reblog
/// inserts nothing, and `RETURNING *` answers exactly the insert that
/// happened — which is what tells the caller a counter step is owed.
const INSERT_REBLOG_SQL: &str = "INSERT INTO statuses (
     id, uri, url, user_id, visibility, conversation_id, reblog_of_id, local
) VALUES (
     $1, $2, $3, $4, $5, $6, $7, true
) ON CONFLICT (user_id, reblog_of_id) WHERE deleted_at IS NULL DO NOTHING
 RETURNING *";

/// [`create_reblog`]'s restatement arm: the caller's live reblog of one
/// original — at most one row, because the partial unique index keeps it
/// so, and a withdrawn (tombstoned) reblog is no row at all.
const LIVE_REBLOG_SQL: &str = "SELECT * FROM statuses
      WHERE user_id = $1 AND reblog_of_id = $2 AND deleted_at IS NULL";

/// [`remove_reblog`]: the same tombstone [`soft_delete`](super::soft_delete) writes, keyed on
/// the live `(user, original)` pair instead of the row id so the withdrawal
/// is one statement. `RETURNING *` hands the caller the row that went,
/// which is what the write hook reports and the counter steps run on; a
/// pair that never reblogged (or already withdrew) matches nothing, which
/// is what makes the write idempotent.
const TOMBSTONE_REBLOG_SQL: &str = "UPDATE statuses
      SET content = '',
          raw_text = '',
          spoiler_text = '',
          deleted_at = now(),
          updated_at = now()
     WHERE user_id = $1 AND reblog_of_id = $2 AND deleted_at IS NULL
   RETURNING *";

/// [`reblogged_status_ids`]: which of a batch of originals the viewer has
/// a live reblog of. Only the ids that *have* a row come back; the caller's
/// flag is membership in that set.
const REBLOGGED_IDS_SQL: &str = "SELECT reblog_of_id FROM statuses
      WHERE user_id = $1 AND reblog_of_id = ANY($2) AND deleted_at IS NULL";

/// [`reblogger_page`]: the accounts that reblogged one original, each
/// entry led by the id of the reblog row behind it — the id the page keys
/// on. The text stops where its one placeholder belongs, because that is
/// where [`reblogger_page`] pushes its bind; the live-row and distributable
/// visibility filters and the keyset then run on the reblog row `s`.
pub(super) const REBLOGGERS_PAGE_SQL: &str = "SELECT s.id AS reblog_id, u.* FROM statuses s
     JOIN users u ON u.id = s.user_id
      WHERE s.reblog_of_id = ";

/// Records `user`'s reblog of `original` at `visibility` and answers the
/// reblog row with a flag saying whether this call wrote it.
///
/// The row is what decision 6 stores an `Announce` as: empty content, the
/// requested `visibility`, `local` set, the original's
/// [`conversation_id`](Status::conversation_id), and the
/// `{base_url}/activities/{uuid}` uri (the row *is* the activity; a plain
/// local status owns `/objects/{uuid}` instead) with the usual
/// `{base_url}/notice/{uuid}` human url. `base_url` is the instance origin,
/// i.e. [`Config::base_url`](crate::config::Config::base_url), which the
/// model layer is handed rather than reading configuration itself.
///
/// A user reblogs a given status once: the write rides the partial unique
/// index on `(user_id, reblog_of_id)` (`WHERE deleted_at IS NULL`), so a
/// repeat after a live row exists — or after a withdrawal if the pair is
/// reblogged again in between — inserts nothing, steps nothing, and answers
/// the row that is already there with `false`. Only the insert that
/// happened moves the counters, in the same transaction as the row: the
/// original's `reblogs_count` and the reblogger's `statuses_count` (and
/// `last_status_at`, which the author-count step only ever moves forward),
/// exactly as [`insert`](super::insert) does for the reblog rows federation imports.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError). [`AppError::NotFound`]
/// is the one row this write cannot answer: a reblog some concurrent
/// withdrawal tombstoned between the conflict and the restatement read.
pub async fn create_reblog(
    pool: &PgPool,
    base_url: &str,
    user: &User,
    original: &Status,
    visibility: Visibility,
) -> AppResult<(Status, bool)> {
    let mut tx = pool.begin().await?;
    let id = new_id();
    let written = sqlx::query_as::<_, Status>(INSERT_REBLOG_SQL)
        .bind(id)
        .bind(activity_ap_id(base_url, id))
        .bind(format!("{base_url}/notice/{id}"))
        .bind(user.id)
        .bind(visibility)
        .bind(original.conversation_id)
        .bind(original.id)
        .fetch_optional(&mut *tx)
        .await?;
    let reblog = match written {
        Some(row) => {
            count_new_post(&mut tx, &row).await?;
            (row, true)
        }
        None => (live_reblog(&mut tx, user.id, original.id).await?, false),
    };
    tx.commit().await?;
    Ok(reblog)
}

/// Withdraws `user_id`'s reblog of `original_id`, answering the tombstone
/// of the reblog row, or `None` when the pair has no live reblog to take.
///
/// The withdrawal is [`soft_delete`](super::soft_delete)'s move — the row stays as a tombstone
/// with its text cleared and its `uri` reserved — keyed on the
/// `(user_id, reblog_of_id)` pair so the caller never names the row, and
/// idempotent for the same reason: a pair that never reblogged (or already
/// withdrew) matches no live row, steps nothing and is not an error. The
/// counters step back in the same transaction the tombstone goes into: the
/// reblogger's `statuses_count` and the original's `reblogs_count` (through
/// the same uncount step `soft_delete` runs, which reads both off the row),
/// and any pin of the reblog row goes with it as it would on a delete.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError), and the rollback leaves
/// the pair exactly as it was: a reblog and its counter steps go together
/// or not at all.
pub async fn remove_reblog(
    pool: &PgPool,
    user_id: Uuid,
    original_id: Uuid,
) -> AppResult<Option<Status>> {
    let mut tx = pool.begin().await?;
    let Some(tombstone) = sqlx::query_as::<_, Status>(TOMBSTONE_REBLOG_SQL)
        .bind(user_id)
        .bind(original_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(None);
    };
    uncount_deleted(&mut tx, &tombstone).await?;
    clear_pins(&mut tx, tombstone.id).await?;
    tx.commit().await?;
    Ok(Some(tombstone))
}

/// Which of `original_ids` `user_id` has a live reblog of — the batched
/// `reblogged` read the status renderer runs once per page
/// (`docs/ARCHITECTURE.md` decision 13).
///
/// The answer is the membership set of *original* ids, not of reblog row
/// ids: a caller keys a row's flag to the status the reblog lands on, which
/// for a reblog row on the page is its own original (the same rule the
/// `favourited` flag follows). An id with no live reblog row is simply
/// absent, and an empty `original_ids` asks the database nothing at all.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn reblogged_status_ids(
    pool: &PgPool,
    user_id: Uuid,
    original_ids: &[Uuid],
) -> AppResult<HashSet<Uuid>> {
    if original_ids.is_empty() {
        return Ok(HashSet::new());
    }
    let rows = sqlx::query_as::<_, (Uuid,)>(REBLOGGED_IDS_SQL)
        .bind(user_id)
        .bind(original_ids)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// The accounts that reblogged `original_id`, newest reblog first and
/// `page`'s `limit` at a time, each entry carrying the id of the reblog row
/// behind the account (see [`ReblogPageEntry`]).
///
/// This is the read behind `GET /api/v1/statuses/{id}/reblogged_by`
/// (Pleroma's `reblogged_by`, which walks the original object's
/// `announcements` list): the page keys on the *reblog row* id, so the
/// endpoint's `max_id`/`min_id`/`since_id` cursors and its `Link` header
/// carry reblog row ids, not account ids. A tombstoned reblog is off the
/// page — a withdrawn boost names no reblogger. Only distributable reblogs
/// (`public` or `unlisted`) are listed, as Mastodon's `distributable_visibility`
/// does: a followers-only or direct boost is its author's business, and no
/// per-viewer follower check is attempted. Who may see the *original* is the
/// endpoint's gate (the shared status visibility check), not this query's.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn reblogger_page(
    pool: &PgPool,
    original_id: Uuid,
    page: PageParams,
) -> AppResult<Vec<ReblogPageEntry>> {
    let mut builder = QueryBuilder::<Postgres>::new(REBLOGGERS_PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(original_id);
    let _live: &mut _ = builder.push(" AND s.deleted_at IS NULL");
    let _distributable: &mut _ = builder.push(" AND s.visibility IN ('public','unlisted')");
    page.apply_keyset(&mut builder, "s.id");
    let entries = builder
        .build_query_as::<ReblogPageEntry>()
        .fetch_all(pool)
        .await?;
    Ok(page.finish(entries))
}

/// [`local_reblogger_ids`]: the accounts behind the *live* reblog rows of
/// one original, restricted to users this instance owns — the boosters the
/// [`crate::services::notify`] edit telling can reach with a row. The
/// partial unique index on `(user_id, reblog_of_id) WHERE deleted_at IS
/// NULL` makes each account appear at most once without a `DISTINCT`.
const LOCAL_REBLOGGER_IDS_SQL: &str = "SELECT s.user_id FROM statuses s
      JOIN users u ON u.id = s.user_id
       WHERE s.reblog_of_id = $1 AND s.deleted_at IS NULL AND u.local
   ORDER BY s.id";

/// The local accounts that currently boost `original_id`, oldest reblog
/// first.
///
/// This is the audience of the `update` telling an edit sends (Pleroma's
/// `get_potential_receiver_ap_ids` of an `Update` activity: the object's
/// `Announce` actors, `where u.local`, the edit's actor subtracted —
/// `lib/pleroma/notification.ex`). A tombstoned boost names nobody, and a
/// remote booster is federation's to tell.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn local_reblogger_ids(pool: &PgPool, original_id: Uuid) -> AppResult<Vec<Uuid>> {
    let ids = sqlx::query_scalar::<_, Uuid>(LOCAL_REBLOGGER_IDS_SQL)
        .bind(original_id)
        .fetch_all(pool)
        .await?;
    Ok(ids)
}

/// Reads the restatement arm of [`create_reblog`]: the caller's live reblog
/// row of `original_id`, which the conflict says exists — unless a
/// concurrent withdrawal tombstoned it in the same window, which is this
/// read's only way to answer `None`.
async fn live_reblog(
    conn: &mut PgConnection,
    user_id: Uuid,
    original_id: Uuid,
) -> AppResult<Status> {
    let existing = sqlx::query_as::<_, Status>(LIVE_REBLOG_SQL)
        .bind(user_id)
        .bind(original_id)
        .fetch_optional(conn)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(existing)
}

#[cfg(test)]
mod tests;
