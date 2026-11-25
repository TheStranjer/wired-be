//! `ActivityPub`'s adapter: the façade the protocol layer sees it through.
//!
//! This file holds no `ActivityPub` logic. The implementation stays in
//! `src/federation/` (actors, signatures, delivery, inbox processing) and in
//! `src/api/activitypub*` (the HTTP routes and handlers); every hook of
//! [`ProtocolAdapter`] implemented here only delegates to those modules.
//!
//! It is one of the two worked examples a new protocol copies: this one wraps
//! existing code, `protocols::fixture` is the minimal one.

use axum::Router;

use crate::{
    protocols::{ProtocolId, adapter::ProtocolAdapter},
    state::AppState,
};

/// The `ActivityPub` adapter.
#[derive(Debug, Clone, Copy)]
pub struct ActivityPub;

impl ProtocolAdapter for ActivityPub {
    /// `ActivityPub`'s id.
    fn id(&self) -> ProtocolId {
        ProtocolId::ActivityPub
    }

    /// The routes of [`crate::api::activitypub`]: actors, objects, inboxes, outboxes.
    fn router(&self, state: &AppState) -> Router<AppState> {
        crate::api::activitypub::router(&state.config)
    }
}
