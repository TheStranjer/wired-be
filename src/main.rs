//! Server entry point: read config, connect to PostgreSQL, migrate, sync
//! the emoji packs, provision the instance actor, serve.

use std::{error::Error, sync::Arc};

use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use wired_be::{
    app,
    config::Config,
    http_client::HttpClient,
    run_migrations,
    services::{emoji_packs, instance_actor},
    state::AppState,
    uploads::UploaderBackend,
};

/// Installs a `tracing-subscriber` formatter filtered by `RUST_LOG`, defaulting to `info`.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // A `.env` file is optional; real environment variables take precedence.
    if let Err(err) = dotenvy::dotenv()
        && !err.not_found()
    {
        return Err(err.into());
    }
    init_tracing();
    let config = Arc::new(Config::from_env()?);
    let db = PgPoolOptions::new().connect(&config.database_url).await?;
    if let Err(err) = run_migrations(&db).await {
        error!(%err, "database migrations failed");
        return Err(err.into());
    }
    info!("database migrations applied");
    let emoji_count =
        emoji_packs::sync_custom_emojis(&db, &config.base_url, &config.emoji_dir).await?;
    info!(emoji_count, "emoji packs synced into custom_emojis");
    let fetch_actor = instance_actor::ensure_instance_actor(&db, &config).await?;
    info!(ap_id = %fetch_actor.ap_id, "instance actor ready");
    let listener = TcpListener::bind(config.bind_addr).await?;
    info!(addr = %config.bind_addr, "listening");
    let state = AppState {
        db,
        config: Arc::clone(&config),
        http_client: HttpClient::from(config.as_ref()),
        uploader: UploaderBackend::from(config.as_ref()),
        instance_stats_cache: Arc::default(),
    };
    axum::serve(listener, app(state)).await?;
    Ok(())
}
