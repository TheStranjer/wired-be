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

use axum::{Router, http::StatusCode, routing::get};

use crate::{
    protocols::{ProtocolId, adapter::ProtocolAdapter},
    state::AppState,
};

/// The fixture adapter.
#[derive(Debug, Clone, Copy)]
pub struct Fixture;

/// `GET /fixture/ping`: answers 204 No Content.
async fn ping() -> StatusCode {
    StatusCode::NO_CONTENT
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
}
