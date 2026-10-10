//! The Mastodon-compatible client API under `/api/v1`
//! (`docs/ARCHITECTURE.md` decision 1), including Pleroma's extensions at
//! `/api/v1/pleroma`.

use axum::Router;

use crate::state::AppState;

/// The `/api/v1` router.
///
/// An intentional empty skeleton: later endpoint tickets add their area
/// routers as `src/api/<area>.rs` and merge them into this one, e.g.
/// `Router::new().merge(timelines::router())`. Do not call
/// [`Router::fallback`] here (or on anything merged here): unmatched paths
/// must reach the app-wide JSON fallback in [`crate::app`], which answers
/// them with Pleroma's `api_not_implemented`, and merging two routers that
/// each set a fallback panics.
pub fn router() -> Router<AppState> {
    Router::new()
}
