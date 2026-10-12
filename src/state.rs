//! Shared application state, handed to every handler.

use std::sync::Arc;

use sqlx::PgPool;

use crate::{
    api::instance::InstanceStatsCache, config::Config, http_client::HttpClient,
    uploads::UploaderBackend,
};

/// State shared across all requests. Cheap to clone.
#[derive(Clone, Debug)]
pub struct AppState {
    /// PostgreSQL connection pool.
    pub db: PgPool,
    /// Runtime configuration.
    pub config: Arc<Config>,
    /// Outbound HTTP client for calls to other instances; cheap to clone,
    /// every handler shares one connection pool.
    pub http_client: HttpClient,
    /// Where uploaded files are stored and what URLs they get
    /// (see [`crate::uploads`]).
    pub uploader: UploaderBackend,
    /// In-memory cache behind `GET /api/v1/instance`'s `stats` block:
    /// the aggregate counters are re-read only after the window lapses
    /// (see [`crate::util::cache::TimedCache`] and
    /// [`STATS_TTL`](crate::api::instance::STATS_TTL)). Construct it with
    /// `Arc::default()`, which applies the endpoint's TTL.
    pub instance_stats_cache: Arc<InstanceStatsCache>,
}
