//! Server entry point: load config, connect to PostgreSQL, migrate, serve.

use std::{error::Error, sync::Arc};

use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use wired_be::{app, config::Config, http_client::HttpClient, run_migrations, state::AppState};

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
    let config = Config::from_env()?;
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
        http_client: HttpClient::from(&config),
        config: Arc::new(config),
    };
    axum::serve(listener, app(state)).await?;
    Ok(())
}
