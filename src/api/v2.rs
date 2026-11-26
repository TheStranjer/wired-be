//! The Mastodon-compatible client API under `/api/v2`
//! (`docs/ARCHITECTURE.md` decision 1).
//!
//! The area routers are split in two by the rule in [`crate::api`]:
//! [`core_router`] is always mounted, [`activitypub_router`] only while
//! `ActivityPub` is enabled. Each returns paths relative to `/api/v2`.

use axum::Router;

use crate::{
    api::{
        account_lists, chats, filters, instance, mastodon_admin, media, notification_policy,
        notifications_v2, search,
    },
    state::AppState,
};

/// The `/api/v2` routes that work for a local account with no `ActivityPub` data.
///
/// [`media`]'s two upload endpoints share the `/api/v1` handler.
/// [`instance::v2_router`] contributes `GET /api/v2/instance`, the reshaped
/// instance document; its `/instance` sub-resources stay v1-only.
/// [`notifications_v2::router`] contributes the grouped notification API:
/// `GET /api/v2/notifications` with its `unread_count`, `{group_key}`,
/// `{group_key}/accounts` and `{group_key}/dismiss` companions.
/// [`mastodon_admin::v2_router`] is nested at `/admin`, the prefix Mastodon's
/// admin API rides on: `GET /api/v2/admin/accounts` is the word-spelled twin
/// of that area's v1 listing, same entities, same page. Do not call
/// [`Router::fallback`] here (or on
/// anything merged here): unmatched paths must reach the app-wide JSON
/// fallback in [`crate::app`], which answers them with Pleroma's
/// `api_not_implemented`, and merging two routers that each set a fallback
/// panics.
///
/// `state` reaches [`media::router`] (as
/// [`state.config`](AppState::config)) for the same reason it reaches
/// [`crate::api::v1::core_router`]: the upload route's body limit comes from
/// the configured upload limits.
pub fn core_router(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(instance::v2_router())
        .merge(media::router(&state.config))
        .merge(notifications_v2::router())
        .merge(notification_policy::v2_router())
        .nest("/admin", mastodon_admin::v2_router())
}

/// The `/api/v2` routes backed by `ActivityPub` data.
///
/// They read or write statuses, relationships, remote accounts or federation state.
///
/// Mounted by the `ActivityPub` adapter only
/// ([`crate::protocols::adapter::ProtocolAdapter::client_api_router`]); the
/// same fallback rule as [`core_router`] applies.
/// [`chats::v2_router`] is nested at `/pleroma/chats`, the paged counterpart
/// of that area's legacy `/api/v1` list. [`account_lists::v2_router`]
/// contributes `GET /api/v2/suggestions`, the source-wrapped twin of that
/// area's v1 listing. [`filters::v2::router`] contributes the v2 filter API:
/// `/api/v2/filters` with its keyword and status sub-resources, over the
/// storage the v1 filter routes read too. [`search::v2_router`] contributes
/// `GET /api/v2/search`, the combined search whose hashtag arm renders tag
/// entities where v1 spells plain names.
///
/// `state` reaches [`search::v2_router`], whose route mounts the rate limiter
/// (see [`crate::rate_limit`]).
pub fn activitypub_router(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(account_lists::v2_router())
        .merge(filters::v2::router())
        .merge(search::v2_router(state))
        .nest("/pleroma/chats", chats::v2_router())
}
