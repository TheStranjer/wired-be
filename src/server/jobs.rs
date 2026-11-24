//! The [`JobRegistry`] [`serve`](super::serve)
//! builds.
//!
//! Every job kind this instance handles, bound to its handler. The
//! handlers themselves live in the areas they serve; this module is the
//! one table that ties kinds to handlers at boot.

use crate::{
    api, federation,
    jobs::{self, JobRegistry},
    mailer, services, streaming,
};

/// The [`JobRegistry`] the [`serve`](super::serve) worker runs: every job
/// kind this instance handles, bound to its handler.
///
/// `pub` because the integration tests drain the queues with the very same
/// table the worker uses (`run_all_jobs` in `tests/api/common.rs`), so a
/// job kind registered here is one a test can run end to end.
#[must_use]
pub fn job_registry() -> JobRegistry {
    let mut registry = JobRegistry::new();
    registry.register(
        federation::delivery::KIND_DELIVER,
        federation::delivery::handle,
    );
    registry.register(streaming::hub::KIND_CLEANUP, streaming::hub::handle_cleanup);
    registry.register(jobs::mutes::KIND_EXPIRE, jobs::mutes::handle_expire);
    registry.register(
        jobs::idempotency::KIND_CLEANUP,
        jobs::idempotency::handle_cleanup,
    );
    registry.register(
        jobs::maintenance::KIND_NOTIFICATIONS,
        jobs::maintenance::handle_notifications,
    );
    registry.register(
        jobs::maintenance::KIND_TOKENS,
        jobs::maintenance::handle_tokens,
    );
    registry.register(jobs::maintenance::KIND_JOBS, jobs::maintenance::handle_jobs);
    registry.register(
        jobs::maintenance::KIND_DELETED_STATUSES,
        jobs::maintenance::handle_purge_deleted_statuses,
    );
    registry.register(
        jobs::maintenance::KIND_MEDIA,
        jobs::maintenance::handle_sweep_media,
    );
    registry.register(jobs::instances::KIND_PROBE, jobs::instances::handle_probe);
    registry.register(jobs::imports::KIND_FOLLOWS, jobs::imports::handle_follows);
    registry.register(jobs::imports::KIND_BLOCKS, jobs::imports::handle_blocks);
    registry.register(jobs::imports::KIND_MUTES, jobs::imports::handle_mutes);
    registry.register(jobs::deletion::KIND_DELETE, jobs::deletion::handle_delete);
    registry.register(
        jobs::deletion::KIND_DELETE_REMOTE,
        jobs::deletion::handle_delete_remote,
    );
    registry.register(api::backups::KIND_RUN, jobs::backups::handle_run);
    registry.register(jobs::backups::KIND_PURGE, jobs::backups::handle_purge);
    registry.register(jobs::statuses::KIND_EXPIRE, jobs::statuses::handle_expire);
    registry.register(jobs::statuses::KIND_SWEEP, jobs::statuses::handle_sweep);
    registry.register(
        jobs::scheduled_statuses::KIND_PUBLISH,
        jobs::scheduled_statuses::handle_publish,
    );
    registry.register(
        jobs::scheduled_statuses::KIND_SWEEP,
        jobs::scheduled_statuses::handle_sweep,
    );
    registry.register(jobs::web_push::KIND_DELIVER, jobs::web_push::handle_deliver);
    registry.register(
        services::webhooks::KIND_DELIVER,
        services::webhooks::handle_deliver,
    );
    registry.register(jobs::rich_media::KIND_FETCH, jobs::rich_media::handle_fetch);
    registry.register(jobs::polls::KIND_END_NOTIFY, jobs::polls::handle_end_notify);
    registry.register(mailer::KIND_SEND, mailer::handle);
    registry.register(
        federation::dispatch::KIND_INCOMING,
        federation::dispatch::handle,
    );
    registry.register(
        federation::publisher::KIND_PUBLISH,
        federation::publisher::handle,
    );
    registry.register(
        services::federation_out::KIND_PUBLISH_UPDATE,
        services::federation_out::handle_publish_update,
    );
    registry.register(
        federation::dispatch::KIND_CLEANUP_RECEIVED,
        federation::dispatch::handle_cleanup_received,
    );
    registry.register(
        federation::fetch_object::KIND_BACKFILL,
        federation::fetch_object::handle,
    );
    registry
}
