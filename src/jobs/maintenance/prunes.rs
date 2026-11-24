//! The prune pass: one daily run of batched `DELETE`s over the `jobs` rows
//! that reached a terminal state more than `JOBS_RETENTION_DAYS` days ago.
//!
//! The `received_activities` ledger is not pruned here: its one retention
//! lives in the dispatcher's `federation.cleanup_received` pass
//! ([`crate::federation::dispatch::KIND_CLEANUP_RECEIVED`]), so the replay
//! window has a single owner.
//!
//! [`handle_jobs`] is the [`JobRegistry`](crate::jobs::JobRegistry) handler of
//! job kind [`KIND_JOBS`]; `prune_in_batches` is the short-statement loop it
//! runs.

use serde_json::Value;
use sqlx::{PgPool, query};

use super::run_sweep;
use crate::{
    jobs::{AttemptInfo, HandlerFuture},
    state::AppState,
};

/// The job kind of the jobs-table prune, run from the
/// [`periodic_jobs`](crate::jobs::scheduler::periodic_jobs) registry through the
/// [`JobRegistry`](crate::jobs::JobRegistry) handler [`handle_jobs`], on
/// [`QUEUE_BACKGROUND`](crate::jobs::QUEUE_BACKGROUND).
pub const KIND_JOBS: &str = "maintenance.jobs";

/// How long a terminal (`completed` or `discarded`) job row stays in the
/// table after its last change before the prune reclaims it: seven days.
/// Nothing reads a terminal row after that; its `unique_key` (if it had one)
/// was freed the moment the row went terminal, so deleting it cannot wake a
/// deduplicated enqueue.
const JOBS_RETENTION_DAYS: i32 = 7;

/// Rows one round of the batched prune claims. Walking a backlog in
/// [`PRUNE_BATCH_SIZE`]-row batches keeps every `DELETE` short enough that
/// one pass never locks a table across the whole sweep.
const PRUNE_BATCH_SIZE: i64 = 1_000;

/// The batched `DELETE` behind [`prune_jobs`]: up to [`PRUNE_BATCH_SIZE`]
/// terminal rows whose `updated_at` is older than the retention `$1`, chosen
/// by `id` so the batches are ordered and no row is ever visited twice.
const PRUNE_JOBS_SQL: &str = "DELETE FROM jobs
         WHERE id IN (
             SELECT id FROM jobs
              WHERE state IN ('completed', 'discarded')
                AND updated_at < now() - make_interval(days => $1)
              ORDER BY id
              LIMIT $2
         )";

/// Runs one batched prune: repeats `sql` — a `DELETE` shaped like
/// [`PRUNE_JOBS_SQL`], taking the retention days as `$1` and the batch size
/// as `$2` — until one round removes fewer rows than `batch`, summing the
/// rounds into the total rows reclaimed.
///
/// A round that comes back short (including zero on an empty or fresh table)
/// ends the loop, so one pass drains the whole backlog.
async fn prune_in_batches(
    pool: &PgPool,
    sql: &'static str,
    retention_days: i32,
    batch: i64,
) -> Result<u64, sqlx::Error> {
    let batch_rows = u64::try_from(batch).unwrap_or(u64::MAX);
    let mut total = 0_u64;
    loop {
        let round = query(sql)
            .bind(retention_days)
            .bind(batch)
            .execute(pool)
            .await?;
        let deleted = round.rows_affected();
        total += deleted;
        if deleted < batch_rows {
            break;
        }
    }
    Ok(total)
}

/// Prunes the terminal `jobs` rows past [`JOBS_RETENTION_DAYS`], reporting
/// how many went. Queue-level deduplication — the `incoming:{activity id}`
/// replay guard of [`crate::api::activitypub`] included — dies with these
/// rows; the activity ledger is pruned by the dispatcher, not here.
async fn prune_jobs(pool: &PgPool) -> Result<u64, sqlx::Error> {
    prune_in_batches(pool, PRUNE_JOBS_SQL, JOBS_RETENTION_DAYS, PRUNE_BATCH_SIZE).await
}

/// The [`JobRegistry`](crate::jobs::JobRegistry) handler for [`KIND_JOBS`]: one
/// pass of the batched queue-row prune, logging the number of rows it deleted.
///
/// Every row it reclaims is bookkeeping nothing reads any more — a job that
/// ended a week ago — so the sweep only frees storage. A failed pass is a
/// [`JobError::Retry`](crate::jobs::JobError::Retry), and the scheduler enqueues the next day regardless,
/// so a pass lost to a database outage costs nothing but the storage.
pub fn handle_jobs(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move { run_sweep("pruned maintenance rows", prune_jobs(&state.db)).await })
}

#[cfg(test)]
mod tests;
