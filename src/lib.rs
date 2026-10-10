//! wired-be: an API-only fediverse backend built on Axum.

pub mod config;
pub mod error;
pub mod state;

use axum::{Router, http::StatusCode, routing::get};
use sqlx::PgPool;

use crate::state::AppState;

/// The migrations embedded from `migrations/`, applied by [`run_migrations`].
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Applies all pending migrations to the database behind `pool`.
///
/// # Errors
///
/// Returns a [`sqlx::migrate::MigrateError`] if any migration fails to run or
/// the migration bookkeeping table cannot be read or written.
pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    MIGRATOR.run(pool).await
}

/// Builds the application router.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
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

    /// Migration tests, run against the throwaway database created by `#[sqlx::test]`.
    mod migrations {
        use sqlx::PgPool;

        /// The base extensions migration installs `citext`, `pg_trgm` and `pgcrypto`.
        #[sqlx::test(migrator = "crate::MIGRATOR")]
        async fn extensions_migration_installs_all_three_extensions(pool: PgPool) {
            let row: (i64,) = sqlx::query_as(
                "SELECT count(*) FROM pg_extension WHERE extname IN ('citext','pg_trgm','pgcrypto')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(row.0, 3, "all three extensions should be installed");
        }
    }
}
