//! Fixtures shared by the `oauth` children's unit tests: the parameter
//! builder and message reader the grant-gate and refusal tests use, the
//! stored-looking token row the response shapes render, and the
//! config-only [`AppState`] the endpoint tests build on — its lazy pool
//! is never consulted, because every case fails at a gate before any
//! database access.

use std::sync::Arc;

use chrono::{Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::params::TokenParams;
use crate::{
    config::Config, error::AppError, http_client::HttpClient, models::oauth_token::OauthToken,
    state::AppState, uploads::UploaderBackend, util::id::new_id,
};

/// A [`TokenParams`] with everything absent but `grant_type`, which
/// `grant` sets.
pub(super) fn params_with_grant(grant: Option<&str>) -> TokenParams {
    TokenParams {
        grant_type: grant.map(str::to_owned),
        username: None,
        password: None,
        scope: None,
        client_id: None,
        client_secret: None,
        refresh_token: None,
        code: None,
        redirect_uri: None,
    }
}

/// The message an [`AppError`] answers with.
pub(super) fn message_of(err: &AppError) -> String {
    err.to_string()
}

/// A stored-looking token row for `app_id`, valid for an hour.
pub(super) fn token_row(app_id: Uuid, scopes: &str) -> OauthToken {
    let now = Utc::now();
    OauthToken {
        id: new_id(),
        token: "access-secret".to_owned(),
        refresh_token: "refresh-secret".to_owned(),
        user_id: Some(new_id()),
        app_id,
        scopes: scopes.to_owned(),
        valid_until: now + Duration::hours(1),
        inserted_at: now,
        updated_at: now,
    }
}

/// The state the router-shaped tests below need: config only, the
/// lazy pool is never consulted (the grant-type and token gates
/// fail first).
pub(super) fn test_state() -> AppState {
    let config = Arc::new(Config::for_tests());
    let db = PgPool::connect_lazy("postgres://localhost/unused").unwrap();
    let hub = crate::streaming::hub::Hub::new(&db);
    AppState {
        db,
        hub,
        config: Arc::clone(&config),
        http_client: HttpClient::from(config.as_ref()),
        uploader: UploaderBackend::from(config.as_ref()),
        instance_stats_cache: Arc::default(),
        mrf_config_cache: Arc::default(),
        translation_languages_cache: Arc::default(),
        rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
        mailer: Arc::new(crate::mailer::MemoryTransport::new()),
    }
}
