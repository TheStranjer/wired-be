//! The Mastodon-compatible client API under `/api/v1`
//! (`docs/ARCHITECTURE.md` decision 1), including Pleroma's extensions at
//! `/api/v1/pleroma`.

use axum::Router;

use crate::{
    api::{
        account_lists, accounts, announcements, apps, blocks, chats, custom_emojis, domain_blocks,
        emoji_packs, endorsements, filters, follow_requests, instance, lists, markers, media, misc,
        mutes, oauth_tokens, password_reset, pleroma_accounts, pleroma_settings, reports,
        scrobbles, search, statuses, streaming, tags,
    },
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
/// `state` reaches [`accounts::router`], [`apps::router`],
/// [`reports::router`] and [`search::router`], the merged routers whose
/// routes mount the rate
/// limiter, and — as
/// [`state.config`] — [`media::router`] and [`pleroma_settings::router`],
/// whose upload routes carry their own body limit (see
/// [`crate::extract::multipart::upload_body_limit`]), which axum's
/// multipart extractor cannot read from the state on its own.
///
/// [`state.config`]: AppState::config
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(account_lists::router())
        .merge(accounts::router(state))
        .merge(announcements::router())
        .merge(apps::router(state))
        .merge(blocks::router())
        .merge(custom_emojis::router())
        .merge(domain_blocks::router())
        .merge(endorsements::router())
        .merge(endorsements::pleroma_router())
        .merge(filters::router())
        .merge(follow_requests::router())
        .merge(instance::router())
        .merge(lists::router())
        .merge(markers::router())
        .merge(media::router(&state.config))
        .merge(mutes::router())
        .merge(password_reset::pleroma_router())
        .merge(pleroma_accounts::router())
        .merge(pleroma_settings::router(&state.config))
        .merge(reports::router(state))
        .merge(scrobbles::router())
        .merge(search::router(state))
        .merge(statuses::router())
        .merge(streaming::router())
        .merge(tags::router())
        .nest("/pleroma", misc::util_router())
        .nest("/pleroma/apps", oauth_tokens::apps_router())
        .nest("/pleroma/chats", chats::router())
        .nest("/pleroma/emoji", emoji_packs::router())
}
