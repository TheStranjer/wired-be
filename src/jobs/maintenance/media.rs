//! The orphaned-media sweep: one six-hourly pass over the locally stored
//! media rows no status ever claimed — unposted, not a mirror of a remote
//! file, older than `MEDIA_STALE_HOURS` and carried by no chat message.
//!
//! [`handle_sweep_media`] is the [`JobRegistry`](crate::jobs::JobRegistry)
//! handler of job kind [`KIND_MEDIA`]; `sweep_media` walks the backlog behind
//! a forward-only id cursor through `fetch_orphaned_batch`, and
//! `reclaim_media` takes each candidate's stored file before its row, so a
//! pointer is never deleted around bytes that are still on disk.

use serde_json::Value;
use sqlx::{PgPool, query, query_as};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError},
    state::AppState,
};

/// The job kind of the orphaned local-media sweep, run from the
/// [`periodic_jobs`](crate::jobs::scheduler::periodic_jobs) registry through the
/// [`JobRegistry`](crate::jobs::JobRegistry) handler [`handle_sweep_media`], on
/// [`QUEUE_BACKGROUND`](crate::jobs::QUEUE_BACKGROUND).
pub const KIND_MEDIA: &str = "maintenance.media";

/// How long an unposted local media row must sit before the sweep may call
/// it abandoned: forty-eight hours. A client stores media first and names
/// it in the status only minutes later, so the grace covers that round
/// trip; past it nothing in the schema still names the file.
const MEDIA_STALE_HOURS: i32 = 48;

/// Rows one round of the orphaned-media walk claims. Walking the backlog
/// in [`MEDIA_BATCH_SIZE`]-row batches, ordered by `id` behind an id
/// cursor, keeps every statement short and — because the cursor only ever
/// moves past visited rows — guarantees a row kept after a failed file
/// delete is not revisited within the same run.
const MEDIA_BATCH_SIZE: i64 = 200;

/// The [`MediaCandidate`] fetch behind [`fetch_orphaned_batch`]: the next
/// [`MEDIA_BATCH_SIZE`] local, orphaned, stale rows past `id` cursor `$2`,
/// in id order.
///
/// "Local, orphaned, stale" is the ticket's conjunction: `status_id IS
/// NULL` (no status ever claimed the upload), `remote_url IS NULL` (a file
/// this instance stores, not a mirror of a remote one), and `inserted_at`
/// beyond [`MEDIA_STALE_HOURS`] hours ago. The `NOT EXISTS` arm skips the
/// media a live chat message still carries through
/// `attachment_media_id` — the one other stored reference to unposted
/// media in today's schema (`migrations/1791714489_chats.sql`).
///
/// The exclusion grows when the scheduled-statuses ticket lands: media
/// kept for a not-yet-due post will need the same guard against that
/// table. `scheduled_at` is only a deferred parameter in
/// `src/services/create_status.rs` today, so no table exists to name here
/// yet and this statement deliberately names none.
const SWEEP_MEDIA_CANDIDATES_SQL: &str = "SELECT id, storage_key FROM media_attachments
         WHERE status_id IS NULL
           AND remote_url IS NULL
           AND inserted_at < now() - make_interval(hours => $1)
           AND id > $2
           AND NOT EXISTS (
               SELECT 1 FROM chat_messages cm
                WHERE cm.attachment_media_id = media_attachments.id
           )
         ORDER BY id
         LIMIT $3";

/// The `DELETE` behind [`reclaim_media`]: one candidate row by id, issued
/// only once its stored file (if any) is gone.
const DELETE_MEDIA_ROW_SQL: &str = "DELETE FROM media_attachments WHERE id = $1";

/// One candidate row of the orphaned-media walk: enough to advance the
/// cursor, delete the file, and delete the row.
#[derive(Debug, sqlx::FromRow)]
struct MediaCandidate {
    /// The row's id: the walk's cursor and the delete's target.
    id: Uuid,
    /// The file's key inside the upload storage; `None` when the row has
    /// no stored file and needs no file step at all.
    storage_key: Option<String>,
}

/// Fetches the next batch of orphaned-media candidates past `after`, the
/// shape documented on [`SWEEP_MEDIA_CANDIDATES_SQL`].
async fn fetch_orphaned_batch(
    pool: &PgPool,
    after: Uuid,
) -> Result<Vec<MediaCandidate>, sqlx::Error> {
    query_as::<_, MediaCandidate>(SWEEP_MEDIA_CANDIDATES_SQL)
        .bind(MEDIA_STALE_HOURS)
        .bind(after)
        .bind(MEDIA_BATCH_SIZE)
        .fetch_all(pool)
        .await
}

/// Reclaims one candidate: the stored file goes first when there is one,
/// and only then the row; reports the rows it deleted (`1`) or kept (`0`).
///
/// A file the uploader refuses to remove — a key [`check_key`] rejects, or
/// bytes that are not on disk — keeps the row: the next pass tries again,
/// and storage never ends up with a file whose database pointer was
/// deleted around it.
///
/// [`check_key`]: crate::uploads::check_key
async fn reclaim_media(state: &AppState, row: MediaCandidate) -> Result<u64, sqlx::Error> {
    if let Some(key) = row.storage_key.as_deref()
        && let Err(err) = state.uploader.delete(key).await
    {
        warn!(
            %err,
            media_id = %row.id,
            "orphaned media file delete failed; keeping the row for the next sweep"
        );
        return Ok(0);
    }
    let removed = query(DELETE_MEDIA_ROW_SQL)
        .bind(row.id)
        .execute(&state.db)
        .await?;
    Ok(removed.rows_affected())
}

/// Runs one sweep: the whole stale-orphan backlog, walked in
/// [`MEDIA_BATCH_SIZE`]-row batches behind a forward-only id cursor, the
/// rows reclaimed summed into one count.
///
/// The cursor starts at the nil uuid — below every `UUIDv7` id (decision 4)
/// — and lands on the last row of each batch, kept or deleted alike, so a
/// row that survived its failed file delete waits for the next run instead
/// of re-appearing in this one. A short batch ends the walk.
///
/// Split out from [`handle_sweep_media`] so the handler is only the
/// logging-and-error shape the [`JobRegistry`](crate::jobs::JobRegistry) expects.
async fn sweep_media(state: &AppState) -> Result<u64, sqlx::Error> {
    let batch_limit = usize::try_from(MEDIA_BATCH_SIZE).unwrap_or(usize::MAX);
    let mut cursor = Uuid::nil();
    let mut deleted = 0_u64;
    loop {
        let batch = fetch_orphaned_batch(&state.db, cursor).await?;
        let visited = batch.len();
        if let Some(last) = batch.last() {
            cursor = last.id;
        }
        for candidate in batch {
            deleted += reclaim_media(state, candidate).await?;
        }
        if visited < batch_limit {
            break;
        }
    }
    Ok(deleted)
}

/// The [`JobRegistry`](crate::jobs::JobRegistry) handler for [`KIND_MEDIA`]: one
/// sweep of the stale, orphaned local media rows, logging the number of
/// rows it deleted.
///
/// A file that cannot be removed is not a failed pass: its row is kept,
/// warned about, and retried by the next run, so one unreadable file never
/// strands the sweep on the rest. Only a database failure is a
/// [`JobError::Retry`], and the scheduler enqueues the next six hours
/// regardless; a pass lost to an outage costs nothing but the storage.
pub fn handle_sweep_media(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        match sweep_media(&state).await {
            Ok(deleted) => {
                info!(deleted, "orphaned local media deleted");
                Ok(())
            }
            Err(err) => Err(JobError::Retry(format!(
                "orphaned media sweep failed: {err}"
            ))),
        }
    })
}

#[cfg(test)]
mod tests;
