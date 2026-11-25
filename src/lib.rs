//! wired-be: an API-only fediverse backend built on Axum.
//!
//! The crate root composes the router ([`app`]) and re-exports the boot path
//! ([`serve`], [`run_migrations`], [`MIGRATOR`], [`job_registry`]) from
//! [`server`]; the unmatched-path 404s and the liveness probe live in
//! [`fallback`].

pub mod api;
pub mod auth;
pub mod cli;
pub mod config;
pub mod default_assets;
pub mod discovery;
pub mod entities;
pub mod error;
pub mod extract;
pub mod fallback;
pub mod federation;
pub mod http_client;
pub mod jobs;
pub mod mailer;
pub mod middleware;
pub mod models;
pub mod oauth;
pub mod pagination;
pub mod protocols;
pub mod rate_limit;
pub mod server;
pub mod services;
pub mod state;
pub mod streaming;
#[cfg(any(test, feature = "integration-tests"))]
pub mod test_db;
#[cfg(any(test, feature = "integration-tests"))]
pub mod test_fixtures;
#[cfg(test)]
pub mod test_server;
#[cfg(test)]
pub mod test_support;
pub mod uploads;
pub mod util;

pub use crate::server::{MIGRATOR, job_registry, run_migrations, serve};

use axum::{Router, routing::get};

use crate::{
    api::media_files,
    fallback::{health, not_found},
    services::media_proxy,
    state::AppState,
};

/// Builds the application router, wrapped in the [`middleware`] stack.
///
/// The [`api`] routers are merged under their prefixes; the routers that
/// serve addresses outside `/api` fixed by their protocols, by stored URLs
/// or by the clients that call them — `/media` ([`media_files`]), `/proxy`
/// ([`media_proxy`]), `/emoji` ([`api::emoji_packs`]), `/.well-known`
/// `/.well-known` ([`discovery`], which serves core's documents and the paths
/// each enabled protocol owns), `/nodeinfo/{version}` ([`api::nodeinfo`]),
/// `/oauth` ([`api::oauth`]), the `/oauth` provider-login paths
/// ([`api::oauth_external`]), `POST /auth/password` ([`api::password_reset`],
/// Pleroma routes the reset request outside `/api`) — are merged at the
/// top level.
///
/// The routes of each protocol the operator enabled with `PROTOCOLS` come
/// from its adapter ([`protocols::registry::enabled`]) and are merged at the
/// top level too: `ActivityPub`'s `/users/{nickname}` and `/internal/fetch`
/// ([`api::activitypub`]) are there only while `activitypub` is enabled.
///
/// Paths no route matches get the standard JSON error response (decision 8),
/// marked with `x-wired-fallback: 1` so a test can tell the fallback's own
/// 404 from a handler's.
///
/// `state` is handed to [`api::router`] and [`api::oauth::router`] before
/// it is consumed: the registration routes those build mount the rate
/// limiter (see [`crate::rate_limit`]), whose middleware layer carries its
/// own cheap clone of the shared state.
///
/// [`api::emoji_packs`]: crate::api::emoji_packs::files_router
pub fn app(state: AppState) -> Router {
    let protocol_routes = protocols::registry::enabled(&state.config)
        .fold(Router::new(), |routes, adapter| {
            routes.merge(adapter.router(&state))
        });
    middleware::apply(
        Router::new()
            .route("/health", get(health))
            .merge(api::router(&state))
            .merge(protocol_routes)
            .merge(api::oauth::router(&state))
            .merge(api::oauth_external::router())
            .merge(api::password_reset::auth_router(&state))
            .merge(discovery::router(&state))
            .merge(api::nodeinfo::router())
            .merge(media_files::router())
            .merge(media_proxy::router())
            .merge(api::emoji_packs::files_router())
            .fallback(not_found)
            .with_state(state),
    )
}
