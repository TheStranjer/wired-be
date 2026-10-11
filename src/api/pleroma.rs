//! Pleroma extension endpoints under `/api/pleroma`
//! (`docs/ARCHITECTURE.md` decision 1): the API areas that are Pleroma's
//! own rather than Mastodon-compatible.
//!
//! Like the other area routers this is a composition point: each feature
//! module exposes a `router()` for its own subtree, and [`router`] nests
//! it at the path Pleroma serves it on. [`mfa`] is the first; the rest of
//! the `/api/pleroma` surface joins here the same way. Today [`misc`]
//! adds one route: `frontend_configurations`, Pleroma's
//! `UtilController.frontend_configurations/2`. Do not call
//! [`Router::fallback`] here: unmatched paths must reach the app-wide JSON
//! fallback in [`crate::app`].

use axum::Router;

use crate::{
    api::{mfa, misc},
    state::AppState,
};

/// The `/api/pleroma` router, merged into [`crate::api`].
pub fn router() -> Router<AppState> {
    Router::new()
        .nest("/accounts/mfa", mfa::router())
        .merge(misc::frontend_configurations_router())
}
