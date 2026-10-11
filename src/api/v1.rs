//! The Mastodon-compatible client API under `/api/v1`
//! (`docs/ARCHITECTURE.md` decision 1), including Pleroma's extensions at
//! `/api/v1/pleroma`.

use axum::Router;

use crate::{
    api::{accounts, announcements, apps, custom_emojis, emoji_packs, filters},
    state::AppState,
};

/// The `/api/v1` router.
///
/// Area routers live as `src/api/<area>.rs` and are merged here; future
/// endpoints join them the same way. Do not call [`Router::fallback`]
/// here (or on anything merged here): unmatched paths must reach the
/// app-wide JSON fallback in [`crate::app`], which answers them with
/// Pleroma's `api_not_implemented`, and merging two routers that each set
/// a fallback panics.
pub fn router() -> Router<AppState> {
    Router::new()
        .merge(accounts::router())
        .merge(announcements::router())
        .merge(apps::router())
        .merge(custom_emojis::router())
        .merge(filters::router())
        .nest("/pleroma/emoji", emoji_packs::router())
}
