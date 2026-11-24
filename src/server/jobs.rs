//! The [`JobRegistry`] [`serve`](super::serve)
//! builds.
//!
//! Every job kind this instance handles, bound to its handler. The
//! handlers themselves live in the areas they serve; this module is the
//! one table that ties kinds to handlers at boot.

use crate::{
    api, federation,
    jobs::{self, HandlerFn, JobRegistry},
    mailer, services, streaming,
};

/// Every job kind this instance handles, paired with its handler.
///
/// A table rather than a run of `register` calls so adding a kind is one
/// line and [`job_registry`] stays short.
const JOB_HANDLERS: &[(&str, HandlerFn)] = &[
    (
        federation::delivery::KIND_DELIVER,
        federation::delivery::handle,
    ),
    (streaming::hub::KIND_CLEANUP, streaming::hub::handle_cleanup),
    (jobs::mutes::KIND_EXPIRE, jobs::mutes::handle_expire),
    (
        jobs::idempotency::KIND_CLEANUP,
        jobs::idempotency::handle_cleanup,
    ),
    (
        jobs::maintenance::KIND_NOTIFICATIONS,
        jobs::maintenance::handle_notifications,
    ),
    (
        jobs::maintenance::KIND_TOKENS,
        jobs::maintenance::handle_tokens,
    ),
    (jobs::maintenance::KIND_JOBS, jobs::maintenance::handle_jobs),
    (
        jobs::maintenance::KIND_DELETED_STATUSES,
        jobs::maintenance::handle_purge_deleted_statuses,
    ),
    (
        jobs::maintenance::KIND_MEDIA,
        jobs::maintenance::handle_sweep_media,
    ),
    (jobs::instances::KIND_PROBE, jobs::instances::handle_probe),
    (jobs::imports::KIND_FOLLOWS, jobs::imports::handle_follows),
    (jobs::imports::KIND_BLOCKS, jobs::imports::handle_blocks),
    (jobs::imports::KIND_MUTES, jobs::imports::handle_mutes),
    (jobs::deletion::KIND_DELETE, jobs::deletion::handle_delete),
    (
        jobs::deletion::KIND_DELETE_REMOTE,
        jobs::deletion::handle_delete_remote,
    ),
    (api::backups::KIND_RUN, jobs::backups::handle_run),
    (jobs::backups::KIND_PURGE, jobs::backups::handle_purge),
    (jobs::statuses::KIND_EXPIRE, jobs::statuses::handle_expire),
    (jobs::statuses::KIND_SWEEP, jobs::statuses::handle_sweep),
    (
        jobs::scheduled_statuses::KIND_PUBLISH,
        jobs::scheduled_statuses::handle_publish,
    ),
    (
        jobs::scheduled_statuses::KIND_SWEEP,
        jobs::scheduled_statuses::handle_sweep,
    ),
    (jobs::web_push::KIND_DELIVER, jobs::web_push::handle_deliver),
    (
        services::webhooks::KIND_DELIVER,
        services::webhooks::handle_deliver,
    ),
    (jobs::rich_media::KIND_FETCH, jobs::rich_media::handle_fetch),
    (jobs::polls::KIND_END_NOTIFY, jobs::polls::handle_end_notify),
    (mailer::KIND_SEND, mailer::handle),
    (
        federation::dispatch::KIND_INCOMING,
        federation::dispatch::handle,
    ),
    (
        federation::publisher::KIND_PUBLISH,
        federation::publisher::handle,
    ),
    (
        services::federation_out::KIND_PUBLISH_UPDATE,
        services::federation_out::handle_publish_update,
    ),
    (
        federation::dispatch::KIND_CLEANUP_RECEIVED,
        federation::dispatch::handle_cleanup_received,
    ),
    (
        federation::fetch_object::KIND_BACKFILL,
        federation::fetch_object::handle,
    ),
    (
        federation::fetch_object::KIND_QUOTE_BACKFILL,
        federation::fetch_object::handle_quote,
    ),
];

/// The [`JobRegistry`] the [`serve`](super::serve) worker runs: every job
/// kind this instance handles, bound to its handler.
///
/// `pub` because the integration tests drain the queues with the very same
/// table the worker uses (`run_all_jobs` in `tests/api/common.rs`), so a
/// job kind registered here is one a test can run end to end.
#[must_use]
pub fn job_registry() -> JobRegistry {
    let mut registry = JobRegistry::new();
    for &(kind, handler) in JOB_HANDLERS {
        registry.register(kind, handler);
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_entry_is_registered() {
        let registry = job_registry();
        for &(kind, _) in JOB_HANDLERS {
            assert!(registry.get(kind).is_some(), "{kind} is not registered");
        }
    }

    #[test]
    fn job_kinds_are_unique() {
        let mut kinds: Vec<&str> = JOB_HANDLERS.iter().map(|&(kind, _)| kind).collect();
        kinds.sort_unstable();
        let total = kinds.len();
        kinds.dedup();
        assert_eq!(kinds.len(), total, "a job kind is listed twice");
    }

    #[test]
    fn unknown_kind_has_no_handler() {
        assert!(job_registry().get("no-such-job-kind").is_none());
    }
}
