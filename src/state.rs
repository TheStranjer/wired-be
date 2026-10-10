//! Shared application state, handed to every handler.

use sqlx::PgPool;

/// State shared across all requests. Cheap to clone.
#[derive(Clone, Debug)]
pub struct AppState {
    /// PostgreSQL connection pool.
    pub db: PgPool,
}
