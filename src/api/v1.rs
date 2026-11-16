//! The Mastodon-compatible client API under `/api/v1`
//! (`docs/ARCHITECTURE.md` decision 1), including Pleroma's extensions at
//! `/api/v1/pleroma`.

use axum::Router;

use crate::{
    api::{
        account_lists, accounts, admin, announcements, apps, backups, blocks, chats, conversations,
        custom_emojis, domain_blocks, emoji_packs, emoji_reactions, endorsements,
        federation_status, filters, follow_requests, instance, lists, markers, mastodon_admin,
        media, misc, mutes, notifications, oauth_tokens, password_reset, pleroma_accounts,
        pleroma_bookmark_folders, pleroma_settings, polls, push, remote_interaction, reports,
        scheduled_statuses, scrobbles, search, statuses, streaming, tags, timelines,
    },
    state::AppState,
};

/// The `/api/v1` router.
///
/// Area routers live as `src/api/<area>.rs` and are merged here; future
/// endpoints join them the same way. One of them is nested rather than
/// merged: [`mastodon_admin::v1_router`] rides on `/admin`, the prefix
/// Mastodon's admin API is routed under (its staff account reads are
/// `GET /api/v1/admin/accounts` and `…/accounts/{id}`). Do not call
/// [`Router::fallback`] here (or on anything merged here): unmatched paths
/// must reach the app-wide JSON fallback in [`crate::app`], which answers
/// them with Pleroma's `api_not_implemented`, and merging two routers that
/// each set a fallback panics.
///
/// `state` reaches [`accounts::router`], [`apps::router`],
/// [`polls::router`], [`reports::router`], [`search::router`] and
/// [`statuses::router`], the merged routers whose routes mount the rate
/// limiter, and — as
/// [`state.config`] — [`media::router`], [`pleroma_settings::router`],
/// [`admin::router`] and [`emoji_packs::router`], whose upload/install
/// routes carry their own body limit (see
/// [`crate::extract::multipart::upload_body_limit`]), which
/// axum's multipart extractor cannot read from the state on its own.
///
/// [`state.config`]: AppState::config
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(account_lists::router())
        .merge(accounts::router(state))
        .merge(announcements::router())
        .merge(apps::router(state))
        .merge(blocks::router())
        .merge(conversations::router())
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
        .merge(notifications::router())
        .merge(password_reset::pleroma_router())
        .merge(pleroma_accounts::router(state))
        .merge(pleroma_settings::router(&state.config))
        .merge(polls::router(state))
        .merge(push::router())
        .merge(remote_interaction::pleroma_router())
        .merge(reports::router(state))
        .merge(scheduled_statuses::router())
        .merge(scrobbles::router())
        .merge(search::router(state))
        .merge(statuses::router(state))
        .merge(streaming::router())
        .merge(tags::router())
        .merge(timelines::router())
        .nest("/pleroma", misc::util_router())
        .nest("/pleroma/admin", admin::router(&state.config))
        .nest("/pleroma/apps", oauth_tokens::apps_router())
        .nest("/pleroma/backups", backups::router())
        .nest(
            "/pleroma/bookmark_folders",
            pleroma_bookmark_folders::router(),
        )
        .nest("/pleroma/chats", chats::router())
        .nest("/pleroma/conversations", conversations::pleroma_router())
        .nest("/pleroma/emoji", emoji_packs::router(&state.config))
        .nest(
            "/pleroma/statuses",
            emoji_reactions::router().merge(statuses::pleroma_router()),
        )
        .merge(federation_status::router())
        .nest("/admin", mastodon_admin::v1_router())
}
