//! The prune pass: one daily run of batched `DELETE`s over the `jobs` rows
//! that reached a terminal state more than `JOBS_RETENTION_DAYS` days ago and
//! the `received_activities` ledger rows past
//! `RECEIVED_ACTIVITIES_RETENTION_DAYS` — the second guarded on the table
//! existing, so a schema that never gained one is skipped, not an error.
//!
//! [`handle_jobs`] is the [`JobRegistry`](crate::jobs::JobRegistry) handler of
//! job kind [`KIND_JOBS`]; `prune_in_batches` is the short-statement loop both
//! steps share.

use serde_json::Value;
use sqlx::{PgPool, query, query_scalar};
use tracing::{debug, info};

use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError},
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

/// How long a `received_activities` replay-guard row may stay before the
/// prune reclaims it: fourteen days. A redelivery after that window is
/// treated as a new activity, the same trade Pleroma-style retention makes.
const RECEIVED_ACTIVITIES_RETENTION_DAYS: i32 = 14;

/// Rows one round of the batched prunes claims. Walking a backlog in
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

/// The probe behind [`prune_received_activities`]: the table's name when the
/// schema has it, `NULL` when it does not. `to_regclass` answers from the
/// catalog without touching the table, so the statement is safe against a
/// schema that never gained one.
const RECEIVED_ACTIVITIES_EXISTS_SQL: &str =
    "SELECT to_regclass('public.received_activities')::text";

/// The batched `DELETE` behind [`prune_received_activities`]: the same shape
/// as [`PRUNE_JOBS_SQL`], against the replay-guard table's `ap_id` and
/// `inserted_at` (the ledger's key, see
/// [`crate::federation::dispatch::KIND_INCOMING`]). It is only ever sent
/// after the existence probe found the table this pass.
const PRUNE_RECEIVED_ACTIVITIES_SQL: &str = "DELETE FROM received_activities
         WHERE ap_id IN (
             SELECT ap_id FROM received_activities
              WHERE inserted_at < now() - make_interval(days => $1)
              ORDER BY ap_id
              LIMIT $2
         )";

/// Runs one batched prune: repeats `sql` — a `DELETE` shaped like
/// [`PRUNE_JOBS_SQL`], taking the retention days as `$1` and the batch size
/// as `$2` — until one round removes fewer rows than `batch`, summing the
/// rounds into the total rows reclaimed.
///
/// Shared by [`prune_jobs`] and [`prune_received_activities`]; a round that
/// comes back short (including zero on an empty or fresh table) ends the
/// loop, so one pass drains the whole backlog.
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
/// rows; the activity ledger itself is the next step's table.
async fn prune_jobs(pool: &PgPool) -> Result<u64, sqlx::Error> {
    prune_in_batches(pool, PRUNE_JOBS_SQL, JOBS_RETENTION_DAYS, PRUNE_BATCH_SIZE).await
}

/// Prunes the `received_activities` replay-guard rows past
/// [`RECEIVED_ACTIVITIES_RETENTION_DAYS`], guarded on the table existing.
///
/// The [`RECEIVED_ACTIVITIES_EXISTS_SQL`] probe runs at job time on every
/// pass, so nothing here assumes the table; the dispatcher ticket added it,
/// so the step prunes its stale rows. The same horizon is kept by the
/// dedicated [`federation.cleanup_received`](crate::federation::dispatch::KIND_CLEANUP_RECEIVED)
/// pass — two daily deletes, the same fourteen days, one table: a row any
/// pass reclaims stays reclaimed.
async fn prune_received_activities(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let table = query_scalar::<_, Option<String>>(RECEIVED_ACTIVITIES_EXISTS_SQL)
        .fetch_one(pool)
        .await?;
    if table.is_none() {
        debug!("received_activities table absent; skipping the activity prune");
        return Ok(0);
    }
    prune_in_batches(
        pool,
        PRUNE_RECEIVED_ACTIVITIES_SQL,
        RECEIVED_ACTIVITIES_RETENTION_DAYS,
        PRUNE_BATCH_SIZE,
    )
    .await
}

/// Runs one prune pass: the terminal job rows, then the existence-guarded
/// `received_activities` rows, their counts summed into one total.
///
/// Split out from [`handle_jobs`] so the handler is only the
/// logging-and-error shape the [`JobRegistry`](crate::jobs::JobRegistry) expects.
async fn sweep_prunes(state: &AppState) -> Result<u64, sqlx::Error> {
    let jobs = prune_jobs(&state.db).await?;
    let activities = prune_received_activities(&state.db).await?;
    Ok(jobs + activities)
}

/// The [`JobRegistry`](crate::jobs::JobRegistry) handler for [`KIND_JOBS`]: one
/// pass of the batched queue-row and replay-guard prunes, logging the total
/// number of rows it deleted.
///
/// Every row it reclaims is bookkeeping nothing reads any more — a job that
/// ended a week ago, a replay guard past its window — so the sweep only
/// frees storage. A failed pass is a [`JobError::Retry`], and the scheduler
/// enqueues the next day regardless, so a pass lost to a database outage
/// costs nothing but the storage.
pub fn handle_jobs(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        match sweep_prunes(&state).await {
            Ok(deleted) => {
                info!(deleted, "pruned maintenance rows");
                Ok(())
            }
            Err(err) => Err(JobError::Retry(format!("prune sweep failed: {err}"))),
        }
    })
}

#[cfg(test)]
mod tests;
