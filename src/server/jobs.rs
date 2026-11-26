//! The [`JobRegistry`] [`serve`](super::serve) builds.
//!
//! Core job kinds, bound to their handlers in one table here, plus the kinds
//! each enabled protocol adapter registers. The handlers themselves live in
//! the areas they serve.

use crate::{
    api,
    config::Config,
    jobs::{self, HandlerFn, JobRegistry},
    mailer,
    protocols::registry as protocol_registry,
    services, streaming,
};

/// Every job kind this instance handles, paired with its handler.
///
/// A table rather than a run of `register` calls so adding a kind is one
/// line and [`job_registry`] stays short.
const JOB_HANDLERS: &[(&str, HandlerFn)] = &[
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
];

/// The [`JobRegistry`] the [`serve`](super::serve) worker runs: every job
/// kind this instance handles, bound to its handler.
///
/// That is the core `JOB_HANDLERS` table plus the
/// [`job_handlers`](crate::protocols::adapter::ProtocolAdapter::job_handlers)
/// of each protocol `config` enables, so a disabled protocol registers no
/// handlers.
///
/// `pub` because the integration tests drain the queues with the very same
/// table the worker uses (`run_all_jobs` in `tests/api/common.rs`), so a
/// job kind registered here is one a test can run end to end.
#[must_use]
pub fn job_registry(config: &Config) -> JobRegistry {
    let mut registry = JobRegistry::new();
    for &(kind, handler) in JOB_HANDLERS {
        registry.register(kind, handler);
    }
    for adapter in protocol_registry::enabled(config) {
        for &(kind, handler) in adapter.job_handlers() {
            registry.register(kind, handler);
        }
    }
    registry
}

#[cfg(test)]
mod tests {
    use crate::{
        config::{BASE_URL_VAR, DATABASE_URL_VAR, PROTOCOLS_VAR, TEST_BASE_URL, TEST_DATABASE_URL},
        jobs::{DEFAULT_JOB_QUEUES, QUEUE_FEDERATOR_INCOMING, QUEUE_FEDERATOR_OUTGOING},
        protocols::{ProtocolId, fixture},
    };

    use super::*;

    /// A test config whose `PROTOCOLS` is `protocols`.
    fn config_with(protocols: &str) -> Config {
        Config::from_lookup(|key| match key {
            DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
            BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
            PROTOCOLS_VAR => Some(protocols.to_owned()),
            _ => None,
        })
        .expect("the fixed job registry test config is valid")
    }

    /// The kinds `ActivityPub` owns, which only its adapter registers.
    const ACTIVITYPUB_KINDS: [&str; 8] = [
        crate::federation::delivery::KIND_DELIVER,
        crate::federation::dispatch::KIND_INCOMING,
        crate::federation::dispatch::KIND_CLEANUP_RECEIVED,
        crate::federation::publisher::KIND_PUBLISH,
        services::federation_out::KIND_PUBLISH_UPDATE,
        crate::federation::fetch_object::KIND_BACKFILL,
        crate::federation::fetch_object::KIND_QUOTE_BACKFILL,
        crate::federation::featured_pins::KIND_SYNC,
    ];

    #[test]
    fn every_table_entry_is_registered() {
        let registry = job_registry(&Config::for_tests());
        for &(kind, _) in JOB_HANDLERS {
            assert!(registry.get(kind).is_some(), "{kind} is not registered");
        }
    }

    #[test]
    fn job_kinds_are_unique() {
        let adapter_kinds = ProtocolId::ALL
            .iter()
            .flat_map(|&id| protocol_registry::adapter(id).job_handlers());
        let mut kinds: Vec<&str> = JOB_HANDLERS
            .iter()
            .chain(adapter_kinds)
            .map(|&(kind, _)| kind)
            .collect();
        kinds.sort_unstable();
        let total = kinds.len();
        kinds.dedup();
        assert_eq!(
            kinds.len(),
            total,
            "a job kind is listed twice across core and the protocol adapters"
        );
    }

    #[test]
    fn unknown_kind_has_no_handler() {
        assert!(
            job_registry(&Config::for_tests())
                .get("no-such-job-kind")
                .is_none()
        );
    }

    #[test]
    fn every_adapter_queue_is_drained_by_default() {
        let default_queues: Vec<&str> = DEFAULT_JOB_QUEUES.split(',').collect();
        for &id in ProtocolId::ALL {
            if id == ProtocolId::Fixture {
                continue;
            }
            for queue in protocol_registry::adapter(id).job_queues() {
                assert!(
                    default_queues.contains(queue),
                    "the {} protocol declares the job queue `{queue}`, which is not in \
                     DEFAULT_JOB_QUEUES: add it there so the default worker drains it",
                    id.as_str()
                );
            }
        }
    }

    #[test]
    fn activitypub_declares_the_federator_queues() {
        assert_eq!(
            protocol_registry::adapter(ProtocolId::ActivityPub).job_queues(),
            [QUEUE_FEDERATOR_INCOMING, QUEUE_FEDERATOR_OUTGOING]
        );
    }

    #[test]
    fn fixture_declares_its_own_queue() {
        assert_eq!(
            protocol_registry::adapter(ProtocolId::Fixture).job_queues(),
            [fixture::QUEUE_FIXTURE]
        );
        assert_eq!(fixture::QUEUE_FIXTURE, "fixture", "the ticket's queue name");
    }

    #[test]
    fn activitypub_only_registers_federation_kinds_and_not_the_fixture() {
        let registry = job_registry(&config_with("activitypub"));
        for kind in ACTIVITYPUB_KINDS {
            assert!(registry.get(kind).is_some(), "{kind} is not registered");
        }
        assert!(registry.get(fixture::KIND_NOOP).is_none());
        assert_eq!(fixture::KIND_NOOP, "fixture.noop", "the ticket's kind name");
    }

    #[test]
    fn fixture_only_registers_its_kind_and_no_federation_kinds() {
        let registry = job_registry(&config_with("fixture"));
        assert!(registry.get(fixture::KIND_NOOP).is_some());
        for kind in ACTIVITYPUB_KINDS {
            assert!(registry.get(kind).is_none(), "{kind} is registered");
        }
        for &(kind, _) in JOB_HANDLERS {
            assert!(registry.get(kind).is_some(), "core kind {kind} is missing");
        }
    }
}
