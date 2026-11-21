//! The run side of the account backups: [`handle_run`], the
//! [`KIND_RUN`](crate::api::backups::KIND_RUN) handler that builds the
//! requested account's [`BackupArchive`](super::archive::BackupArchive),
//! stores it under [`storage_key`], and marks the row processed — with
//! [`RunArgs`]'s parsing, the [`retry`] the two queue handlers share, and
//! [`KIND_PURGE`], the kind [`handle_purge`](super::purge::handle_purge)
//! answers to.

use std::fmt;

use bytes::Bytes;
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use super::archive::build;
use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError},
    models::{
        backup,
        user::{self, User},
    },
    state::AppState,
};

/// The job kind of the periodic backup purge, handled by
/// [`handle_purge`](super::purge::handle_purge) on
/// [`QUEUE_BACKGROUND`](crate::jobs::QUEUE_BACKGROUND) from the
/// [`periodic_jobs`](crate::jobs::scheduler::periodic_jobs) registry.
pub const KIND_PURGE: &str = "backups.purge";

/// What a stored archive is put to the uploader as; the `content_type`
/// column of [`crate::api::backups`] says the same about every backup.
pub(super) const ARCHIVE_CONTENT_TYPE: &str = "application/zip";

/// The `{backup_id}` arguments of a [`crate::api::backups::KIND_RUN`] job,
/// exactly as `enqueue_backup_job` in [`crate::api::backups`] writes them.
#[derive(Debug, Deserialize)]
struct RunArgs {
    /// The `backups` row whose archive the job builds.
    backup_id: Uuid,
}

/// Reads [`RunArgs`] out of a claimed job's payload; arguments this shape
/// cannot deserialize are permanent — the job is discarded, never retried.
fn parse_args(args: &Value) -> Result<Uuid, JobError> {
    serde_json::from_value::<RunArgs>(args.clone())
        .map(|parsed| parsed.backup_id)
        .map_err(|err| JobError::Discard(format!("backup job arguments are unusable: {err}")))
}

/// Lifts a failure outside the queue's discard arms into a retry, naming
/// the step that hit it; every remaining step of a run converges on a
/// rerun, so another pass costs nothing but the wait.
pub(super) fn retry(step: &str, err: &dyn fmt::Display) -> JobError {
    JobError::Retry(format!("backup {step} failed: {err}"))
}

/// The storage key of the archive `file_name` belonging to `user_id`:
/// `backups/{user_id}/{file_name}` inside the uploads store. Both parts
/// are key-charset clean by construction — the id is a hyphenated UUID,
/// the name was minted by [`crate::models::backup::file_name`] — so
/// [`crate::uploads::check_key`] accepts the result.
pub(super) fn storage_key(user_id: Uuid, file_name: &str) -> String {
    format!("backups/{user_id}/{file_name}")
}

/// The [`JobRegistry`](crate::jobs::JobRegistry) handler for
/// [`KIND_RUN`](crate::api::backups::KIND_RUN): build the requested
/// account's archive, store it, and mark the row processed.
///
/// The row the id names and its owner are read first — a vanished either
/// one is permanent, the account's own delete having outrun the job — and
/// a payload the shape cannot parse never runs at all; those discard.
/// Everything after them is work a rerun converges on: [`build`] writes a
/// fresh temp file from the database as it stands, the uploader overwrites
/// the same key in place, and [`mark_processed`](backup::mark_processed)
/// rewrites the same row with the same key, so any IO or database failure
/// retries the job whole. The temp file is the archive's own to delete:
/// dropping the [`BackupArchive`](super::archive::BackupArchive) removes
/// it, at the latest when this handler returns.
pub fn handle_run(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move {
        let backup_id = parse_args(&args)?;
        let row = load_backup(&state, backup_id).await?;
        let owner = load_owner(&state, row.user_id).await?;
        let archive = build(&state, &owner, &row.file_name)
            .await
            .map_err(|err| retry("archive build", &err))?;
        let key = storage_key(owner.id, archive.file_name());
        let bytes = tokio::fs::read(archive.temp.path())
            .await
            .map_err(|err| retry("archive read", &err))?;
        state
            .uploader
            .put(&key, Bytes::from(bytes), ARCHIVE_CONTENT_TYPE)
            .await
            .map_err(|err| retry("archive upload", &err))?;
        let size = i64::try_from(archive.size()).unwrap_or(i64::MAX);
        let marked = backup::mark_processed(&state.db, row.id, size, &key)
            .await
            .map_err(|err| retry("row mark", &err))?;
        if let Some(marked) = marked {
            debug!(
                backup = %marked.id,
                user = %owner.nickname,
                size,
                %key,
                "backup archive stored"
            );
        } else {
            debug!(
                backup = %row.id,
                %key,
                "the backup row vanished before its mark; the stored file is its own orphan"
            );
        }
        Ok(())
    })
}

/// The row the job builds. A vanished row is permanent (discard): the
/// account's own delete took the request, and no retry brings it back.
///
/// # Errors
///
/// [`JobError::Discard`] when no row carries `id`, [`JobError::Retry`]
/// on a database failure.
async fn load_backup(state: &AppState, id: Uuid) -> Result<backup::Backup, JobError> {
    match backup::get_by_id(&state.db, id).await {
        Ok(Some(row)) => Ok(row),
        Ok(None) => Err(JobError::Discard(format!(
            "the backup job's backup {id} no longer exists"
        ))),
        Err(err) => Err(retry("row read", &err)),
    }
}

/// The account whose archive the row requests. A vanished owner is
/// permanent (discard), exactly as in [`load_backup`]; the id comes from
/// the row, so this is the race the delete won between the two reads.
///
/// # Errors
///
/// [`JobError::Discard`] when the user row is gone, [`JobError::Retry`]
/// on a database failure.
async fn load_owner(state: &AppState, user_id: Uuid) -> Result<User, JobError> {
    match user::get_by_id(&state.db, user_id).await {
        Ok(Some(owner)) => Ok(owner),
        Ok(None) => Err(JobError::Discard(format!(
            "the backup job's user {user_id} no longer exists"
        ))),
        Err(err) => Err(retry("owner read", &err)),
    }
}

#[cfg(test)]
mod tests;
