//! The HTTP API surface: the composition point for the area routers
//! (`docs/ARCHITECTURE.md` decision 3).
//!
//! Each API area lives in its own module and exposes a
//! `pub fn router() -> Router<AppState>`; [`router`] nests them under their
//! URL prefixes and [`crate::app`] merges the result into the application
//! router.
//!
//! Unmatched paths are not handled here. The app-wide JSON fallback in
//! [`crate::app`] answers every unrouted path under `/api` with 404
//! `{"error":"Not implemented"}` (Pleroma's `api_not_implemented`,
//! `lib/pleroma/web/fallback/redirect_controller.ex`), so the area routers
//! must never install a `Router::fallback` of their own: it would shadow the
//! app-wide one, and merging two routers that each set one panics.

pub mod apps;
pub mod v1;
pub mod v2;

use axum::Router;

use crate::state::AppState;

/// Composes every API area router under its URL prefix.
///
/// `/api/v1` ([`v1`]) and `/api/v2` ([`v2`]) hold the Mastodon-compatible
/// client API; later tickets add the remaining areas (Pleroma extensions,
/// `ActivityPub`, well-known) here the same way.
pub fn router() -> Router<AppState> {
    Router::new()
        .nest("/api/v1", v1::router())
        .nest("/api/v2", v2::router())
}
