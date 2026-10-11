//! Server entry point: load config, connect to PostgreSQL, serve.

use std::error::Error;

use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use wired_be::{app, config::Config, state::AppState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // A `.env` file is optional; real environment variables take precedence.
    if let Err(err) = dotenvy::dotenv()
        && !err.not_found()
    {
        return Err(err.into());
    }
    let config = Config::from_env()?;
    let db = PgPoolOptions::new().connect(&config.database_url).await?;
    wired_be::run_migrations(&db).await?;
    let listener = TcpListener::bind(config.bind_addr).await?;
    axum::serve(listener, app(AppState { db })).await?;
    Ok(())
}
