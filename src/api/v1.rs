//! The Mastodon-compatible client API under `/api/v1`
//! (`docs/ARCHITECTURE.md` decision 1), including Pleroma's extensions at
//! `/api/v1/pleroma`.

use axum::Router;

use crate::{
    api::{
        accounts, announcements, apps, custom_emojis, emoji_packs, filters, lists, markers, media,
        misc, oauth_tokens, scrobbles,
    },
    config::Config,
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
///
/// `config` reaches [`media::router`], the one merged router that needs it:
/// the upload route carries its own body limit (see
/// [`crate::extract::multipart::upload_body_limit`]), which axum's multipart
/// extractor cannot read from the state on its own.
pub fn router(config: &Config) -> Router<AppState> {
    Router::new()
        .merge(accounts::router())
        .merge(announcements::router())
        .merge(apps::router())
        .merge(custom_emojis::router())
        .merge(filters::router())
        .merge(lists::router())
        .merge(markers::router())
        .merge(media::router(config))
        .merge(scrobbles::router())
        .nest("/pleroma", misc::util_router())
        .nest("/pleroma/apps", oauth_tokens::apps_router())
        .nest("/pleroma/emoji", emoji_packs::router())
}
