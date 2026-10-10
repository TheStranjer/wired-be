//! Shared application state, handed to every handler.

use std::sync::Arc;

use sqlx::PgPool;

use crate::config::Config;

/// State shared across all requests. Cheap to clone.
#[derive(Clone, Debug)]
pub struct AppState {
    /// PostgreSQL connection pool.
    pub db: PgPool,
    /// Runtime configuration.
    pub config: Arc<Config>,
}
