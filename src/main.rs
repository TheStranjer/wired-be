//! Server entry point: load config, connect to PostgreSQL, migrate, serve.

use std::{error::Error, sync::Arc};

use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use wired_be::{
    app, config::Config, http_client::HttpClient, run_migrations, state::AppState,
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
    let listener = TcpListener::bind(config.bind_addr).await?;
    info!(addr = %config.bind_addr, "listening");
    let state = AppState {
        db,
        config: Arc::clone(&config),
        http_client: HttpClient::from(config.as_ref()),
        uploader: UploaderBackend::from(config.as_ref()),
    };
    axum::serve(listener, app(state)).await?;
    Ok(())
}
