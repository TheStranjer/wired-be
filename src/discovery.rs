//! Discovery: the one module that owns every `/.well-known` path.
//!
//! A path under `/.well-known` is one of two kinds:
//!
//! - An **owned path** is served by exactly one protocol, such as a future
//!   `/.well-known/matrix/server`. The protocol declares it through
//!   [`ProtocolAdapter::well_known_routes`], and [`router`] mounts it only
//!   while that protocol is enabled.
//! - A **shared document** is served by core, and each protocol contributes
//!   parts of it: `WebFinger` (`/.well-known/webfinger`), host-meta
//!   (`/.well-known/host-meta`) and `NodeInfo` (`/.well-known/nodeinfo`).
//!   Core mounts them whatever `PROTOCOLS` says. The hooks through which
//!   protocols fill them are on [`ProtocolAdapter`]:
//!   [`webfinger`](ProtocolAdapter::webfinger) adds aliases and links to a
//!   user's `WebFinger` document, and
//!   [`host_meta_links`](ProtocolAdapter::host_meta_links) adds links to
//!   host-meta. A `NodeInfo` hook comes in a follow-up ticket.
//!
//! `/.well-known/oauth-authorization-server` is a future shared document:
//! more than one protocol may want to describe its authorization server
//! there, and the `ATProto` work must settle who serves it before it is
//! added.
//!
//! No path may be claimed twice. Axum panics at startup when two routes
//! overlap, and the tests in this file check every adapter in
//! [`ProtocolId::ALL`](crate::protocols::ProtocolId::ALL) against core, enabled or not, so a collision between
//! two protocols is caught even when no instance runs both.

use axum::Router;

use crate::{
    api::{nodeinfo, well_known},
    protocols::{adapter::ProtocolAdapter, registry},
    state::AppState,
};

/// The `/.well-known` paths core serves as shared documents.
pub const CORE_PATHS: [&str; 3] = [
    well_known::WEBFINGER_PATH,
    well_known::HOST_META_PATH,
    nodeinfo::DISCOVERY_PATH,
];

/// Every `/.well-known` route of the application, merged by [`crate::app`].
///
/// Core's shared documents come first, then the owned paths of each protocol
/// `state.config` enables. `/nodeinfo/{version}` is not under `/.well-known`
/// and stays in [`nodeinfo::router`].
pub fn router(state: &AppState) -> Router<AppState> {
    let shared_documents = well_known::router().merge(nodeinfo::well_known_router());
    registry::enabled(&state.config)
        .flat_map(ProtocolAdapter::well_known_routes)
        .fold(shared_documents, |routes, (path, handler)| {
            routes.route(path, handler)
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::protocols::ProtocolId;

    use super::*;

    /// Every owned path of every adapter, enabled or not, with its protocol.
    fn owned_paths() -> Vec<(ProtocolId, &'static str)> {
        ProtocolId::ALL
            .iter()
            .flat_map(|id| {
                registry::adapter(*id)
                    .well_known_routes()
                    .into_iter()
                    .map(|(path, _handler)| (*id, path))
            })
            .collect()
    }

    /// No path is claimed twice across core and all adapters.
    #[test]
    fn no_well_known_path_is_claimed_twice() {
        let mut claimed = BTreeSet::new();
        for path in CORE_PATHS {
            assert!(claimed.insert(path), "core claims {path} twice");
        }
        for (id, path) in owned_paths() {
            assert!(
                claimed.insert(path),
                "{id:?} claims {path}, which core or another protocol already serves"
            );
        }
    }

    /// Owned paths all live under `/.well-known/`.
    #[test]
    fn owned_paths_are_under_well_known() {
        for (id, path) in owned_paths() {
            assert!(
                path.starts_with("/.well-known/"),
                "{id:?} owns {path}, which is not under /.well-known/"
            );
        }
    }

    /// The fixture's hook reports its path, so the two tests above cover a real entry.
    #[test]
    fn the_fixture_owns_its_well_known_path() {
        assert!(
            owned_paths().contains(&(
                ProtocolId::Fixture,
                crate::protocols::fixture::WELL_KNOWN_PATH
            )),
            "the fixture declares /.well-known/fixture"
        );
    }
}
