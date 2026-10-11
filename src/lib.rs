//! wired-be: an API-only fediverse backend built on Axum.

pub mod api;
pub mod auth;
pub mod config;
pub mod entities;
pub mod error;
pub mod extract;
pub mod models;
pub mod oauth;
pub mod state;
pub mod util;

use axum::{Router, http::StatusCode, routing::get};
use sqlx::PgPool;

use crate::{api::apps, state::AppState};

/// The migrations embedded from `migrations/`, applied by
/// [`run_migrations`] and used by `#[sqlx::test(migrator =
/// "wired_be::MIGRATOR")]`.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Applies all pending migrations to the database behind `pool`.
///
/// # Errors
///
/// Returns a [`sqlx::migrate::MigrateError`] if any migration fails to run
/// or the migration bookkeeping table cannot be read or written.
pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    MIGRATOR.run(pool).await
}

/// Builds the application router.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .nest("/api/v1", apps::router())
        .with_state(state)
}

/// Liveness probe.
async fn health() -> StatusCode {
    StatusCode::NO_CONTENT
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt as _;
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt as _;

    /// Router backed by a pool that never connects, for routes that don't touch the DB.
    fn test_app() -> Router {
        let db = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap();
        app(AppState { db })
    }

    #[tokio::test]
    async fn health_returns_no_content() {
        let response = test_app()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "health status");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(body.is_empty(), "health body should be empty");
    }

    #[tokio::test]
    async fn unknown_route_returns_not_found() {
        let response = test_app()
            .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "unknown route status"
        );
    }

    #[tokio::test]
    async fn the_apps_routes_are_wired_under_api_v1() {
        // Routing happens before the extractors run, so with no token the
        // auth extractor rejects on the router itself: the 401 proves
        // `verify_credentials` is mounted, and the missing required body
        // proves registration is. Neither test touches the database, so
        // the lazy pool is enough.
        let app = test_app();
        let response = app
            .clone()
            .oneshot(
                Request::get("/api/v1/apps/verify_credentials")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "verify_credentials is routed and demands a token"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            r#"{"error":"The access token is invalid"}"#,
            "the 401 body is the Pleroma wording"
        );

        let registration = app
            .oneshot(
                Request::post("/api/v1/apps")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            registration.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "registration is routed and demands its parameters"
        );
    }

    #[test]
    fn migrations_are_embedded() {
        // The `migrations/` directory is embedded at compile time; a
        // build without the schema this project needs would be silent
        // otherwise.
        assert!(
            MIGRATOR
                .migrations
                .iter()
                .any(|m| m.version == 1_791_640_900),
            "the oauth_apps migration is embedded"
        );
    }
}
