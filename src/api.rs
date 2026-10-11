//! The HTTP API surface: the composition point for the area routers
//! (`docs/ARCHITECTURE.md` decision 3).
//!
//! Each API area lives in its own module and exposes a
//! `pub fn router() -> Router<AppState>`; [`router`] nests them under
//! their URL prefixes and [`crate::app`] merges the result into the
//! application router. Later tickets add the remaining `/api` areas
//! (`/api/v2`, `/api/pleroma`, OAuth, `ActivityPub`, discovery) here the
//! same way.

pub mod apps;

use axum::Router;

use crate::state::AppState;

/// Composes the API area routers under their URL prefixes.
pub fn router() -> Router<AppState> {
    Router::new().nest("/api/v1", apps::router())
}
