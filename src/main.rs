//! Server entry point: load config, then hand everything to [`wired_be::serve`].

use std::error::Error;

use tracing_subscriber::EnvFilter;
use wired_be::{config::Config, serve};

/// Installs a `tracing-subscriber` formatter filtered by `RUST_LOG`, defaulting to `info`.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    // A `.env` file is optional; real environment variables take precedence.
    if let Err(err) = dotenvy::dotenv()
        && !err.not_found()
    {
        return Err(err.into());
    }
    init_tracing();
    let config = Config::from_env()?;
    serve(config).await?;
    Ok(())
}
