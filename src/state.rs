//! Shared application state, handed to every handler.

use std::sync::Arc;

use sqlx::PgPool;

use crate::{
    api::instance::InstanceStatsCache,
    config::Config,
    http_client::HttpClient,
    mailer::{MailTransport, transport_from_config},
    rate_limit::RateLimiter,
    services::translate::TranslationLanguagesCache,
    streaming::hub::Hub,
    uploads::UploaderBackend,
};

/// State shared across all requests. Cheap to clone.
#[derive(Clone, Debug)]
pub struct AppState {
    /// PostgreSQL connection pool.
    pub db: PgPool,
    /// The streaming event hub (see [`crate::streaming::hub`]): every
    /// [`StreamEvent`](crate::streaming::hub::StreamEvent) published by
    /// this process fans out to the local subscribers here and is
    /// bridged to the other instances over Postgres `NOTIFY`.
    pub hub: Hub,
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
    /// The in-memory cache behind
    /// `GET /api/v1/instance/translation_languages`: the configured
    /// provider's language pairs are refetched only after the window lapses
    /// (see [`crate::util::cache::TimedCache`] and
    /// [`LANGUAGES_TTL`](crate::services::translate::LANGUAGES_TTL)).
    /// Construct it with `Arc::default()`, which applies that TTL.
    pub translation_languages_cache: Arc<TranslationLanguagesCache>,
    /// The in-memory cache behind the MRF pipeline's configuration read:
    /// the `mrf` instance setting is re-read only after the window lapses
    /// (see [`crate::util::cache::TimedCache`] and
    /// [`MRF_CONFIG_TTL`](crate::federation::mrf::MRF_CONFIG_TTL)).
    /// Construct it with `Arc::default()`, which applies that TTL.
    pub mrf_config_cache: Arc<crate::federation::mrf::MrfConfigCache>,
    /// The in-memory fixed-window rate limiter shared by every throttled
    /// route (see [`crate::rate_limit`]): one instance per process, so a
    /// bucket counts one budget across the routes that mount it. Build it
    /// from the same config with
    /// [`RateLimiter::from_config`](crate::rate_limit::RateLimiter::from_config),
    /// or over an injected clock with
    /// [`RateLimiter::with_clock`](crate::rate_limit::RateLimiter::with_clock)
    /// in tests that roll windows.
    pub rate_limiter: Arc<RateLimiter>,
    /// How emails leave this instance: the SMTP relay transport in
    /// production (see [`crate::mailer::transport_from_config`]), the
    /// recording [`MemoryTransport`](crate::mailer::MemoryTransport) in
    /// tests. The [`mailer`](crate::mailer) job handler sends through it.
    pub mailer: Arc<dyn MailTransport + Send + Sync>,
}

impl AppState {
    /// Builds the shared state over `db` and `config`: the streaming hub,
    /// the outbound HTTP client, the uploader backend, the endpoint caches
    /// at their default windows, the process-wide rate limiter and the
    /// mail transport [`transport_from_config`] picks for these settings
    /// [`transport_from_config`] picks for these settings (the SMTP relay
    /// when `SMTP_HOST` is set, the disabled stand-in while it is not).
    /// This is the construction [`serve`](crate::serve) runs with, shared
    /// with the `create-admin` command (see [`crate::cli`]).
    #[must_use]
    pub fn new(db: PgPool, config: Arc<Config>) -> Self {
        let hub = Hub::new(&db);
        Self {
            db,
            hub,
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            rate_limiter: Arc::new(RateLimiter::from_config(config.as_ref())),
            mailer: transport_from_config(&config),
            config,
        }
    }

    /// The [`MemoryTransport`](crate::mailer::MemoryTransport) behind
    /// [`mailer`](Self::mailer), when the state was built with one — the
    /// mailbox integration and unit tests assert deliveries on. `None` for
    /// any other transport.
    ///
    /// Test support: finding the recording mailbox behind [`mailer`](Self::mailer).
    #[cfg(any(test, feature = "integration-tests"))]
    #[must_use]
    pub fn mailbox(&self) -> Option<crate::mailer::MemoryTransport> {
        self.mailer
            .as_any()
            .downcast_ref::<crate::mailer::MemoryTransport>()
            .cloned()
    }
}
