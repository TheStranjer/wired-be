//! Periodic maintenance jobs: the housekeeping sweeps that reclaim rows whose
//! useful life has ended.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres rows;
//! this module holds the recurring sweeps whose kinds live under the
//! `maintenance.` prefix. Each entry is one pass over one table, enqueued by
//! the [`periodic_jobs`](super::scheduler::periodic_jobs) schedule and run
//! through the [`JobRegistry`](super::JobRegistry) like every other kind.
//!
//! [`handle_notifications`] (job kind [`KIND_NOTIFICATIONS`]) is the first.
//! Dismissal is soft: the dismissal endpoints stamp
//! [`dismissed_at`](crate::models::notification::Notification::dismissed_at)
//! (`migrations/1794145737_notification_dismissed_at.sql`) and every read
//! filters those rows out, so they stay on the books forever once hidden.
//! The sweep deletes the ones dismissed longer than `RETENTION_DAYS` ago,
//! once per day, the window Pleroma-style retention gives them.
//!
//! [`handle_tokens`] (job kind [`KIND_TOKENS`]) is the second: one hourly
//! pass over the five short-lived credential tables, deleting the rows no
//! read accepts any more — `oauth_tokens` expired beyond the week their
//! refresh token buys them, spent or expired `oauth_authorizations` codes,
//! expired `mfa_tokens` challenges, spent or lapsed
//! `password_reset_tokens`, and expired `oauth_login_states` round-trip
//! states — summing the rows reclaimed into one count.
//! The reference is Pleroma's `ClearOauthTokenWorker`
//! (`lib/pleroma/workers/cron/clear_oauth_token_worker.ex`), which clears
//! expired tokens on a daily cron; here one sweep covers every credential
//! table, on the hour.
//!
//! [`handle_jobs`] (job kind [`KIND_JOBS`]) is the third: one daily pass of
//! batched prunes. It deletes the `jobs` rows that reached a terminal
//! state more than `JOBS_RETENTION_DAYS` days ago — which sweeps the
//! redeliverable-queue deduplication out too, since the queue keeps its
//! `incoming:{activity id}` unique key (see [`crate::api::activitypub`]).
//! It leaves `received_activities` alone: the dispatcher's own
//! `federation.cleanup_received` pass ([`crate::federation::dispatch`]) is
//! the one place that table's retention is set. The reference is the cron
//! purges under Pleroma's `lib/pleroma/workers/cron/`.
//!
//! [`handle_purge_deleted_statuses`] (job kind [`KIND_DELETED_STATUSES`]) is
//! the fourth: one daily pass hard-deleting the LOCAL statuses soft-deleted
//! more than
//! [`Config::purge_deleted_statuses_days`](crate::config::Config::purge_deleted_statuses_days)
//! days ago, `0` disabling the sweep. Remote tombstones (`local = false`)
//! are never touched: their rows and unique `uri`s stay so a deleted remote
//! post is never re-imported (see the `deleted_at` column comment in
//! `migrations/1791712088_statuses.sql`). Dependents — `media_attachments`
//! by `status_id`, favourites, notifications — follow each deleted status
//! through their `ON DELETE CASCADE` foreign keys, not by hand. The
//! reference is Pleroma's status-expiry `Purge` step, here driven by the
//! configured age instead of per-row `expires_at`.
//!
//! [`handle_sweep_media`] (job kind [`KIND_MEDIA`]) is the fifth: a
//! six-hourly sweep of locally stored media no status ever claimed. The
//! candidates are `status_id IS NULL` (unposted), `remote_url IS NULL`
//! (this instance's own file, not a mirror), at least `MEDIA_STALE_HOURS`
//! hours old, and referenced by neither a `chat_messages` row
//! (`attachment_media_id`, `migrations/1791714489_chats.sql`) nor a pending
//! `scheduled_statuses` row (its id inside `params -> 'media_ids'`, the
//! reservation that outlives the grace period until the post publishes). Each
//! reclaimed row loses its stored file first, through
//! [`UploaderBackend::delete`](crate::uploads::UploaderBackend::delete),
//! and then itself; a file whose removal fails keeps its row for the next
//! pass, so the sweep never deletes a pointer and strands its bytes. The
//! rows are walked in id-cursored batches of `MEDIA_BATCH_SIZE` so one
//! kept row cannot re-loop inside a run.
//!
//! This module is a tree, one child per sweep: `notifications` the
//! dismissed-notification delete, `tokens` the credential sweeps, `prunes`
//! the batched queue-row delete, `deleted_statuses` the
//! stale local tombstone purge and `media` the orphaned local-file sweep;
//! `test_support` holds the fixtures the children's unit tests share, and
//! each child's `tests` the sweep's own. Every kind and handler is
//! re-exported here, so an outside path stays `crate::jobs::maintenance::…`.

mod deleted_statuses;
mod media;
mod notifications;
mod prunes;
mod tokens;

#[cfg(test)]
mod test_support;

use std::fmt;

use tracing::info;

use super::JobError;

pub use self::{
    deleted_statuses::{KIND_DELETED_STATUSES, handle_purge_deleted_statuses},
    media::{KIND_MEDIA, handle_sweep_media},
    notifications::{KIND_NOTIFICATIONS, handle_notifications},
    prunes::{KIND_JOBS, handle_jobs},
    tokens::{KIND_TOKENS, handle_tokens},
};

/// Runs one sweep future and turns its outcome into the handler result every
/// single-statement sweep shares: a row count is logged at `info` as
/// `deleted` under `label`, and an error becomes a [`JobError::Retry`] whose
/// detail is `"{label} failed: {err}"`, so the queue keeps the pass.
///
/// `label` names what the sweep reclaimed (`"expired mutes deleted"`); it is
/// both the log message and the prefix of the retry detail.
pub(crate) async fn run_sweep<E: fmt::Display>(
    label: &'static str,
    sweep: impl Future<Output = Result<u64, E>>,
) -> Result<(), JobError> {
    match sweep.await {
        Ok(deleted) => {
            info!(deleted, "{label}");
            Ok(())
        }
        Err(err) => Err(JobError::Retry(format!("{label} failed: {err}"))),
    }
}

#[cfg(test)]
mod tests {
    use std::future::ready;

    use super::*;

    #[tokio::test]
    async fn a_count_is_a_clean_pass() {
        let outcome = run_sweep("rows deleted", ready(Ok::<u64, String>(3))).await;

        assert_eq!(outcome, Ok(()), "a swept count of three succeeds");
    }

    #[tokio::test]
    async fn a_zero_count_is_a_clean_pass() {
        let outcome = run_sweep("rows deleted", ready(Ok::<u64, String>(0))).await;

        assert_eq!(outcome, Ok(()), "nothing to delete succeeds too");
    }

    #[tokio::test]
    async fn an_error_retries_with_the_label_and_the_cause() {
        let outcome = run_sweep("rows deleted", ready(Err::<u64, &str>("boom"))).await;

        assert_eq!(
            outcome,
            Err(JobError::Retry("rows deleted failed: boom".to_owned())),
            "the retry names the sweep and carries the error"
        );
    }
}
