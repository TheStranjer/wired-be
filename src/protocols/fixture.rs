//! The fixture protocol: the minimal template for a new protocol.
//!
//! It exists only in test builds. Copy this file to start a protocol: it
//! implements every required method of [`ProtocolAdapter`] as simply as
//! possible. Every hook added to [`ProtocolAdapter`] from now on must also be
//! implemented here and tested through it, so this file always shows the full
//! set of things a protocol decides.
//!
//! `protocols::activitypub` is the other worked example, a façade over a
//! large existing implementation.

use axum::{Json, Router, http::StatusCode, routing::MethodRouter, routing::get};

use crate::{
    api::well_known::WebfingerContribution,
    config::Config,
    jobs::{AttemptInfo, HandlerFn, HandlerFuture, JobError},
    models::user::User,
    protocols::{ProtocolId, adapter::ProtocolAdapter},
    state::AppState,
};

/// The fixture adapter.
#[derive(Debug, Clone, Copy)]
pub struct Fixture;

/// The fixture's one job kind, which does nothing.
pub const KIND_NOOP: &str = "fixture.noop";

/// The fixture's own job queue.
///
/// It is deliberately not in [`crate::jobs::DEFAULT_JOB_QUEUES`]: the fixture
/// exists only in test builds, so the default worker never drains it.
pub const QUEUE_FIXTURE: &str = "fixture";

/// The handler of [`KIND_NOOP`]: succeeds without doing anything.
pub fn handle_noop(
    _state: AppState,
    _attempt: AttemptInfo,
    _args: serde_json::Value,
) -> HandlerFuture {
    Box::pin(async { Ok::<(), JobError>(()) })
}

/// The job kinds the fixture registers.
const FIXTURE_JOB_HANDLERS: &[(&str, HandlerFn)] = &[(KIND_NOOP, handle_noop)];

/// The queues the fixture declares.
const FIXTURE_JOB_QUEUES: &[&str] = &[QUEUE_FIXTURE];

/// The `/.well-known` path only the fixture serves.
pub const WELL_KNOWN_PATH: &str = "/.well-known/fixture";

/// `GET /fixture/ping`: answers 204 No Content.
async fn ping() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// `GET /.well-known/fixture`: answers a small JSON object.
async fn well_known_document() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "protocol": "fixture" }))
}

impl ProtocolAdapter for Fixture {
    /// The fixture's id.
    fn id(&self) -> ProtocolId {
        ProtocolId::Fixture
    }

    /// One route, `GET /fixture/ping`.
    fn router(&self, _state: &AppState) -> Router<AppState> {
        Router::new().route("/fixture/ping", get(ping))
    }

    /// One job kind, [`KIND_NOOP`].
    fn job_handlers(&self) -> &'static [(&'static str, HandlerFn)] {
        FIXTURE_JOB_HANDLERS
    }

    /// One queue, [`QUEUE_FIXTURE`].
    fn job_queues(&self) -> &'static [&'static str] {
        FIXTURE_JOB_QUEUES
    }

    /// One owned path, `GET /.well-known/fixture`.
    fn well_known_routes(&self) -> Vec<(&'static str, MethodRouter<AppState>)> {
        vec![(WELL_KNOWN_PATH, get(well_known_document))]
    }

    /// One alias, `fixture:<nickname>`, and no links.
    fn webfinger(&self, _config: &Config, user: &User) -> WebfingerContribution {
        WebfingerContribution {
            aliases: vec![format!("fixture:{}", user.nickname)],
            links: Vec::new(),
        }
    }
}
