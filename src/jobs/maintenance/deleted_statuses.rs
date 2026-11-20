//! The deleted-status purge: one daily pass hard-deleting the LOCAL statuses
//! soft-deleted more than the configured number of days ago, while every
//! remote tombstone stays so a deleted remote post is never re-imported.
//!
//! [`handle_purge_deleted_statuses`] is the
//! [`JobRegistry`](crate::jobs::JobRegistry) handler of job kind
//! [`KIND_DELETED_STATUSES`]; `purge_deleted` runs the one `DELETE` behind it,
//! and each purged status's dependents follow it through their cascade
//! constraints rather than by hand here.

use serde_json::Value;
use sqlx::{PgPool, query};
use tracing::info;

use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError},
    state::AppState,
};

/// The job kind of the deleted-local-status purge, run from the
/// [`periodic_jobs`](crate::jobs::scheduler::periodic_jobs) registry through the
/// [`JobRegistry`](crate::jobs::JobRegistry) handler [`handle_purge_deleted_statuses`],
/// on [`QUEUE_BACKGROUND`](crate::jobs::QUEUE_BACKGROUND).
pub const KIND_DELETED_STATUSES: &str = "maintenance.deleted_statuses";

/// The `DELETE` behind [`purge_deleted`]: every status row this instance
/// owns (`local = true`), that is soft-deleted (`deleted_at IS NOT NULL`),
/// and whose deletion is older than the configured `days` of age `$1`.
/// Remote tombstones (`local = false`) never match: the row and its unique
/// `uri` stay so a deleted remote post is never re-imported (see the
/// `deleted_at` column comment in `migrations/1791712088_statuses.sql`).
/// Dependents — `media_attachments` by `status_id`, favourites,
/// notifications — follow each deleted status through their
/// `ON DELETE CASCADE` foreign keys, never by hand here.
const PURGE_DELETED_STATUSES_SQL: &str = "DELETE FROM statuses
         WHERE deleted_at IS NOT NULL
           AND local = true
           AND deleted_at < now() - make_interval(days => $1)";

/// Runs one purge and returns the number of status rows it deleted.
///
/// Split out from [`handle_purge_deleted_statuses`] so the handler is only
/// the logging-and-error shape the [`JobRegistry`](crate::jobs::JobRegistry)
/// expects.
async fn purge_deleted(pool: &PgPool, age_days: i32) -> Result<u64, sqlx::Error> {
    let deleted = query(PURGE_DELETED_STATUSES_SQL)
        .bind(age_days)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

/// The [`JobRegistry`](crate::jobs::JobRegistry) handler for
/// [`KIND_DELETED_STATUSES`]: one daily hard purge of the stale deleted
/// local statuses, logging the number of rows it deleted.
///
/// The age comes from the configuration, not a literal: statuses
/// soft-deleted more than
/// [`Config::purge_deleted_statuses_days`](crate::config::Config::purge_deleted_statuses_days)
/// days ago go. A configured `0` disables the sweep: the pass deletes
/// nothing and reports a count of zero. Because the delete answers only to
/// `local = true` rows, remote tombstones survive every pass, and each
/// purged status's dependents go with it through their cascade
/// constraints.
///
/// A failed pass is a [`JobError::Retry`], and the scheduler enqueues the
/// next day regardless, so the same purge runs again shortly; a pass lost
/// to a database outage costs nothing but the storage.
pub fn handle_purge_deleted_statuses(
    state: AppState,
    _attempt: AttemptInfo,
    _args: Value,
) -> HandlerFuture {
    Box::pin(async move {
        let age_days = i32::try_from(state.config.purge_deleted_statuses_days).unwrap_or(i32::MAX);
        if age_days == 0 {
            info!(deleted = 0, "deleted-status purge disabled");
            return Ok(());
        }
        match purge_deleted(&state.db, age_days).await {
            Ok(deleted) => {
                info!(deleted, "deleted local statuses purged");
                Ok(())
            }
            Err(err) => Err(JobError::Retry(format!(
                "deleted-status purge failed: {err}"
            ))),
        }
    })
}

#[cfg(test)]
mod tests;
