//! The Mastodon-compatible client API under `/api/v2`
//! (`docs/ARCHITECTURE.md` decision 1).

use axum::Router;

use crate::{
    api::{chats, instance, media},
    config::Config,
    state::AppState,
};

/// The `/api/v2` router.
///
/// Area routers live as `src/api/<area>.rs` and are merged here; the first
/// one is [`media`], whose two upload endpoints share the `/api/v1` handler,
/// and [`chats::v2_router`] is nested at `/pleroma/chats`, the paged
/// counterpart of that area's legacy `/api/v1` list. [`instance::v2_router`]
/// contributes `GET /api/v2/instance`, the reshaped instance document; its
/// `/instance` sub-resources stay v1-only. Do not call
/// [`Router::fallback`] here (or on anything merged here): unmatched paths
/// must reach the app-wide JSON fallback in [`crate::app`], which answers
/// them with Pleroma's `api_not_implemented`, and merging two routers that
/// each set a fallback panics.
///
/// `config` reaches [`media::router`] for the same reason it reaches
/// [`crate::api::v1::router`]: the upload route's body limit comes from the
/// configured upload limits.
pub fn router(config: &Config) -> Router<AppState> {
    Router::new()
        .merge(instance::v2_router())
        .merge(media::router(config))
        .nest("/pleroma/chats", chats::v2_router())
}
