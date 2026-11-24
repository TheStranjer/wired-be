//! The purge side of the account backups: [`handle_purge`], the daily
//! [`KIND_PURGE`](super::run::KIND_PURGE) sweep of the requests past their
//! validity window — [`sweep_archive_file`] takes each row's stored file
//! first, and the row follows it into deletion only once the bytes are
//! gone or were never there.

use chrono::Utc;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::{
    error::AppError,
    jobs::{AttemptInfo, HandlerFuture, retry},
    models::backup,
    state::AppState,
};

/// The [`JobRegistry`](crate::jobs::JobRegistry) handler for
/// [`KIND_PURGE`](super::run::KIND_PURGE):
/// one daily sweep of the backups past their validity window — the
/// stored file goes first, then the row.
///
/// With [`backup_purge`](crate::config::Config::backup_purge) off the
/// pass deletes nothing and reports so in the log; the window itself
/// comes from
/// [`backup_validity_days`](crate::config::Config::backup_validity_days),
/// read at the sweep's `Utc::now()`. A row with no `storage_key` never
/// had (or lost) its file, and an already-absent file answers the
/// uploader's `NotFound` — both are tolerated, not failures: the row is
/// all that is left to sweep. Any other refusal from the storage layer
/// keeps the row for the next pass, so the queue never ends up with a
/// database pointer to bytes it cannot name any more. A database failure
/// retries the whole pass; every step is idempotent.
///
/// [`backup_purge`]: crate::config::Config::backup_purge
pub fn handle_purge(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        if !state.config.backup_purge.get() {
            info!(purged = 0, "backup purge disabled");
            return Ok(());
        }
        let rows = backup::expired(&state.db, Utc::now(), state.config.backup_validity_days)
            .await
            .map_err(|err| retry("backup purge read", &err))?;
        let mut purged = 0_usize;
        for row in rows {
            if let Some(key) = row.storage_key.as_deref()
                && !sweep_archive_file(&state, &row, key).await
            {
                continue;
            }
            let _swept = backup::delete(&state.db, row.id)
                .await
                .map_err(|err| retry("backup row delete", &err))?;
            purged += 1;
        }
        info!(purged, "expired account backups purged");
        Ok(())
    })
}

/// Removes the stored file of `key` for `row`, answering whether the row
/// may follow it into deletion.
///
/// An already-absent file is tolerated: [`UploaderBackend::delete`] answers
/// the queue's own [`AppError::NotFound`] for bytes that are gone, and then
/// the row is all that is left to sweep. Any other refusal keeps the row
/// for the next pass, so storage never ends up with a file whose database
/// pointer was deleted around it — the rule of the orphaned-media sweep in
/// [`super::maintenance`](crate::jobs::maintenance).
///
/// [`UploaderBackend::delete`]: crate::uploads::UploaderBackend::delete
pub(in crate::jobs) async fn sweep_archive_file(
    state: &AppState,
    row: &backup::Backup,
    key: &str,
) -> bool {
    match state.uploader.delete(key).await {
        Ok(()) => true,
        Err(AppError::NotFound) => {
            debug!(backup = %row.id, %key, "backup file already absent; sweeping the row");
            true
        }
        Err(err) => {
            warn!(
                %err,
                backup = %row.id,
                %key,
                "backup file delete failed; keeping the row for the next purge"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests;
