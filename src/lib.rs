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
pub mod entities;
pub mod error;
pub mod extract;
pub mod fallback;
pub mod federation;
pub mod feeds;
pub mod http_client;
pub mod jobs;
pub mod mailer;
pub mod middleware;
pub mod models;
pub mod oauth;
pub mod pagination;
pub mod rate_limit;
pub mod server;
pub mod services;
pub mod state;
pub mod streaming;
#[cfg(any(test, feature = "integration-tests"))]
pub mod test_db;
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
/// ([`media_proxy`]), `/emoji` ([`api::emoji_packs`]), `/users/{nickname}`
/// and `/internal/fetch` ([`api::activitypub`]) and its feed variants plus
/// `/tags/{tag}` ([`api::feeds`]), `/.well-known` ([`api::well_known`]),
/// `/.well-known/nodeinfo` and `/nodeinfo/{version}` ([`api::nodeinfo`]),
/// `/oauth` ([`api::oauth`]), the `/oauth` provider-login paths
/// ([`api::oauth_external`]), `POST /auth/password` ([`api::password_reset`],
/// Pleroma routes the reset request outside `/api`), and the `MongooseIM`
/// auth endpoints plus `/manifest.json` ([`api::misc`]) — are merged at the
/// top level, together with the HTML helper pages of
/// [`api::html_pages`] (`/frontend_switcher`, `/embed/{id}`,
/// `/notice/{id}/embed_player` and `/mailer/unsubscribe/{token}`), the
/// remote-interaction pages of [`api::remote_interaction`]
/// (`/ostatus_subscribe`, `/authorize_interaction` and `/main/ostatus`)
/// and the invite registration link of
/// [`api::admin::invites::registration_router`] (`/registration/{token}`,
/// redirecting to the frontend's registration page), and the public
/// instance documents of [`api::admin::apps_docs::public_router`]
/// (`/instance/{name}.html`, the terms-of-service pages an admin
/// uploads).
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
    middleware::apply(
        Router::new()
            .route("/health", get(health))
            .merge(api::router(&state))
            .merge(api::activitypub::router(&state.config))
            .merge(api::feeds::router())
            .merge(api::oauth::router(&state))
            .merge(api::oauth_external::router())
            .merge(api::password_reset::auth_router(&state))
            .merge(api::well_known::router())
            .merge(api::nodeinfo::router())
            .merge(media_files::router())
            .merge(media_proxy::router())
            .merge(api::emoji_packs::files_router())
            .merge(api::misc::router())
            .merge(api::html_pages::router())
            .merge(api::remote_interaction::router())
            .merge(api::admin::invites::registration_router())
            .merge(api::admin::apps_docs::public_router())
            .fallback(not_found)
            .with_state(state),
    )
}
