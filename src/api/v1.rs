//! The Mastodon-compatible client API under `/api/v1`
//! (`docs/ARCHITECTURE.md` decision 1), including Pleroma's extensions at
//! `/api/v1/pleroma`.
//!
//! The area routers are split in two by the rule in [`crate::api`]:
//! [`core_router`] is always mounted, [`activitypub_router`] only while
//! `ActivityPub` is enabled. Each returns paths relative to `/api/v1`.

use axum::Router;

use crate::{
    api::{
        account_lists, accounts, admin, announcements, apps, backups, blocks, chats, conversations,
        custom_emojis, domain_blocks, emoji_packs, emoji_reactions, endorsements, featured_tags,
        federation_status, filters, follow_requests, instance, lists, markers, mastodon_admin,
        mastodon_admin_reports, media, misc, mutes, notifications, oauth_tokens, password_reset,
        pleroma_account_settings, pleroma_accounts, pleroma_bookmark_folders, pleroma_settings,
        polls, preferences, push, remote_interaction, reports, scheduled_statuses, scrobbles,
        search, statuses, streaming, tags, timelines, wired,
    },
    state::AppState,
};

/// The `/api/v1` routes that work for a local account with no `ActivityPub` data.
///
/// Area routers live as `src/api/<area>.rs` and are merged here; the table in
/// [`crate::api`] says which areas are core. Two admin prefixes are shared
/// with [`activitypub_router`]. `/admin` is Mastodon's admin API prefix:
/// [`mastodon_admin::v1_router`] is its core half
/// (`GET /api/v1/admin/accounts` and its `{id}` twin and moderation writes),
/// and [`mastodon_admin_reports::v1_router`] the `ActivityPub`-backed
/// `reports` queue. `/pleroma/admin` is the Pleroma admin API:
/// [`admin::core_router`] here, [`admin::activitypub_router`] there. Do not call
/// [`Router::fallback`] here (or on anything merged here): unmatched paths
/// must reach the app-wide JSON fallback in [`crate::app`], which answers
/// them with Pleroma's `api_not_implemented`, and merging two routers that
/// each set a fallback panics.
///
/// `state` reaches [`accounts::core_router`] and [`apps::router`], the merged
/// routers whose routes mount the rate limiter, and — as
/// [`state.config`] — [`media::router`], [`pleroma_settings::router`],
/// [`admin::core_router`] and [`emoji_packs::router`], whose upload/install
/// routes carry their own body limit (see
/// [`crate::extract::multipart::upload_body_limit`]), which
/// axum's multipart extractor cannot read from the state on its own.
///
/// [`state.config`]: AppState::config
pub fn core_router(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(accounts::core_router(state))
        .merge(announcements::router())
        .merge(apps::router(state))
        .merge(custom_emojis::router())
        .merge(instance::router())
        .merge(markers::router())
        .merge(media::router(&state.config))
        .merge(notifications::router())
        .merge(password_reset::pleroma_router())
        .merge(pleroma_settings::router(&state.config))
        .merge(preferences::router())
        .merge(push::router())
        .merge(streaming::router())
        .nest("/pleroma", misc::util_router())
        .nest("/pleroma/admin", admin::core_router(&state.config))
        .nest("/pleroma/apps", oauth_tokens::apps_router())
        .nest("/pleroma/backups", backups::router())
        .nest("/pleroma/emoji", emoji_packs::router(&state.config))
        .nest("/wired", wired::router())
        .nest("/admin", mastodon_admin::v1_router())
}

/// The `/api/v1` routes backed by `ActivityPub` data.
///
/// They read or write statuses, relationships, remote accounts or federation state.
///
/// Mounted by the `ActivityPub` adapter only
/// ([`crate::protocols::adapter::ProtocolAdapter::client_api_router`]); the
/// same fallback rule as [`core_router`] applies.
///
/// `state` reaches [`accounts::activitypub_router`], [`polls::router`], [`remote_interaction::pleroma_router`],
/// [`reports::router`], [`search::router`], [`statuses::router`] and
/// [`pleroma_accounts::router`], whose routes mount the rate limiter.
pub fn activitypub_router(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(account_lists::router())
        .merge(accounts::activitypub_router(state))
        .merge(blocks::router())
        .merge(conversations::router())
        .merge(domain_blocks::router())
        .merge(endorsements::router())
        .merge(endorsements::pleroma_router())
        .merge(featured_tags::router())
        .merge(filters::router())
        .merge(follow_requests::router())
        .merge(lists::router())
        .merge(mutes::router())
        .merge(pleroma_account_settings::mastodon_router())
        .merge(pleroma_accounts::router(state))
        .merge(polls::router(state))
        .merge(remote_interaction::pleroma_router(state))
        .merge(reports::router(state))
        .merge(scheduled_statuses::router())
        .merge(scrobbles::router())
        .merge(search::router(state))
        .merge(statuses::router(state))
        .merge(tags::router())
        .merge(timelines::router())
        .merge(federation_status::router())
        .nest(
            "/pleroma/bookmark_folders",
            pleroma_bookmark_folders::router(),
        )
        .nest("/pleroma/admin", admin::activitypub_router())
        .nest("/admin", mastodon_admin_reports::v1_router())
        .nest("/pleroma/chats", chats::router())
        .nest("/pleroma/conversations", conversations::pleroma_router())
        .nest(
            "/pleroma/statuses",
            emoji_reactions::router().merge(statuses::pleroma_router()),
        )
}
