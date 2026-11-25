//! `ActivityPub`'s adapter: the façade the protocol layer sees it through.
//!
//! This file holds no `ActivityPub` logic. The implementation stays in
//! `src/federation/` (actors, signatures, delivery, inbox processing) and in
//! `src/api/activitypub*` (the HTTP routes and handlers); every hook of
//! [`ProtocolAdapter`] implemented here only delegates to those modules.
//!
//! # Job kinds
//!
//! A job kind belongs to `ActivityPub` when its handler lives under
//! `src/federation/` or `src/services/federation_out/`, or it is enqueued on
//! [`QUEUE_FEDERATOR_INCOMING`] or [`QUEUE_FEDERATOR_OUTGOING`]. These are the
//! kinds `ACTIVITYPUB_JOB_HANDLERS` registers:
//!
//! - `federator.deliver` ([`federation::delivery::KIND_DELIVER`])
//! - `federator.incoming` ([`federation::dispatch::KIND_INCOMING`])
//! - `federation.cleanup_received` ([`federation::dispatch::KIND_CLEANUP_RECEIVED`])
//! - `federator.publish` ([`federation::publisher::KIND_PUBLISH`])
//! - `polls.publish_update` ([`services::federation_out::KIND_PUBLISH_UPDATE`])
//! - `federator.backfill` ([`federation::fetch_object::KIND_BACKFILL`])
//! - `federator.quote_backfill` ([`federation::fetch_object::KIND_QUOTE_BACKFILL`])
//!
//! It is one of the two worked examples a new protocol copies: this one wraps
//! existing code, `protocols::fixture` is the minimal one.

use axum::Router;

use crate::{
    api::well_known::{self, FingerLink, WebfingerContribution},
    config::Config,
    federation,
    jobs::{HandlerFn, QUEUE_FEDERATOR_INCOMING, QUEUE_FEDERATOR_OUTGOING},
    models::user::User,
    protocols::{ProtocolId, adapter::ProtocolAdapter},
    services,
    state::AppState,
};

/// The `ActivityPub` adapter.
#[derive(Debug, Clone, Copy)]
pub struct ActivityPub;

/// Every job kind `ActivityPub` owns, bound to its handler in `src/federation/`
/// or `src/services/federation_out/`.
const ACTIVITYPUB_JOB_HANDLERS: &[(&str, HandlerFn)] = &[
    (
        federation::delivery::KIND_DELIVER,
        federation::delivery::handle,
    ),
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

/// The queues `ActivityPub` enqueues its inbound and outbound work onto.
const ACTIVITYPUB_JOB_QUEUES: &[&str] = &[QUEUE_FEDERATOR_INCOMING, QUEUE_FEDERATOR_OUTGOING];

impl ProtocolAdapter for ActivityPub {
    /// `ActivityPub`'s id.
    fn id(&self) -> ProtocolId {
        ProtocolId::ActivityPub
    }

    /// The routes of [`crate::api::activitypub`]: actors, objects, inboxes, outboxes.
    fn router(&self, state: &AppState) -> Router<AppState> {
        crate::api::activitypub::router(&state.config)
    }

    /// The federation job kinds listed in the module docs.
    fn job_handlers(&self) -> &'static [(&'static str, HandlerFn)] {
        ACTIVITYPUB_JOB_HANDLERS
    }

    /// The incoming and outgoing federator queues.
    fn job_queues(&self) -> &'static [&'static str] {
        ACTIVITYPUB_JOB_QUEUES
    }

    /// The actor id and profile aliases, and the profile-page, self and subscribe links.
    fn webfinger(&self, config: &Config, user: &User) -> WebfingerContribution {
        well_known::activitypub_contribution(user, config)
    }

    /// The `lrdd` link naming the `WebFinger` template.
    fn host_meta_links(&self, config: &Config) -> Vec<FingerLink> {
        well_known::activitypub_host_meta_links(config)
    }

    /// `NodeInfo` names it `activitypub`.
    fn nodeinfo_protocol(&self) -> Option<&'static str> {
        Some("activitypub")
    }
}
