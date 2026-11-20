//! The dismissed-notification sweep: the daily pass that reclaims the
//! notification rows a soft dismissal hid more than `RETENTION_DAYS` days
//! ago. Reads stopped answering those rows the moment they were dismissed,
//! so the sweep only frees the storage they sit on.
//!
//! [`handle_notifications`] is the [`JobRegistry`](crate::jobs::JobRegistry)
//! handler of job kind [`KIND_NOTIFICATIONS`]; `delete_dismissed` runs the
//! one `DELETE` behind it.

use serde_json::Value;
use sqlx::{PgPool, query};
use tracing::info;

use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError},
    state::AppState,
};

/// The job kind of the dismissed-notification sweep, run from the
/// [`periodic_jobs`](crate::jobs::scheduler::periodic_jobs) registry through the
/// [`JobRegistry`](crate::jobs::JobRegistry) handler [`handle_notifications`], on
/// [`QUEUE_BACKGROUND`](crate::jobs::QUEUE_BACKGROUND).
pub const KIND_NOTIFICATIONS: &str = "maintenance.notifications";

/// How long a dismissed notification stays in the table before the sweep
/// reclaims it: thirty days. Reads stopped answering it the moment it was
/// dismissed; this is only how long the row itself lives on.
const RETENTION_DAYS: i32 = 30;

/// The `DELETE` behind [`handle_notifications`]: every notification row
/// whose soft dismissal (`dismissed_at IS NOT NULL`) is older than
/// [`RETENTION_DAYS`] days.
const SWEEP_SQL: &str = "DELETE FROM notifications
        WHERE dismissed_at IS NOT NULL
          AND dismissed_at < now() - make_interval(days => $1)";

/// Runs one sweep and returns the number of rows it deleted.
///
/// Split out from [`handle_notifications`] so the handler is only the
/// logging-and-error shape the [`JobRegistry`](crate::jobs::JobRegistry) expects.
async fn delete_dismissed(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let deleted = query(SWEEP_SQL).bind(RETENTION_DAYS).execute(pool).await?;
    Ok(deleted.rows_affected())
}

/// The [`JobRegistry`](crate::jobs::JobRegistry) handler for [`KIND_NOTIFICATIONS`]:
/// one sweep of the notifications dismissed longer than `RETENTION_DAYS`
/// days ago, logging how many rows it deleted.
///
/// A failed sweep is a [`JobError::Retry`], and the scheduler enqueues the
/// next period regardless, so the same sweep runs again shortly; the rows are
/// only invisible, never needed, so a pass lost to a database outage costs
/// nothing but the storage.
pub fn handle_notifications(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        match delete_dismissed(&state.db).await {
            Ok(deleted) => {
                info!(deleted, "dismissed notifications deleted");
                Ok(())
            }
            Err(err) => Err(JobError::Retry(format!(
                "dismissed notification sweep failed: {err}"
            ))),
        }
    })
}

#[cfg(test)]
mod tests;
