//! Program entry point: parse the command line, then either serve or run
//! the `create-admin` bootstrap. Logic lives in [`wired_be::cli`] and the
//! library so the binary stays a thin dispatch.

use std::{error::Error, io::Write as _, sync::Arc};

use sqlx::postgres::PgPoolOptions;
use tracing::info;
use tracing_subscriber::EnvFilter;
use wired_be::{
    cli::{self, Command},
    config::Config,
    run_migrations, serve,
    state::AppState,
};

/// Installs a `tracing-subscriber` formatter filtered by `RUST_LOG`, defaulting to `info`.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Reads the process arguments (without the program name).
fn arguments() -> Vec<String> {
    std::env::args().skip(1).collect()
}

/// Writes `text` to standard output; the help texts are the only
/// stdout this CLI produces, everything else rides on `tracing`.
///
/// # Errors
///
/// Any [`std::io::Error`] from the write or the flush.
fn print_stdout(text: &str) -> Result<(), std::io::Error> {
    let stdout = std::io::stdout();
    let mut sink = stdout.lock();
    sink.write_all(text.as_bytes()).map(drop)?;
    sink.flush().map(drop)
}

/// The `create-admin` command: connect with the environment's
/// configuration, apply pending migrations (the same way [`serve`] does,
/// so the command works on a fresh database), read the password from
/// standard input and run [`cli::create_admin`]. The created account is
/// written to the log; nothing else is printed.
///
/// # Errors
///
/// The configuration, connection, migration, stdin or creation failure
/// that stopped the bootstrap.
async fn run_create_admin(args: cli::CreateAdminArgs) -> Result<(), Box<dyn Error + Send + Sync>> {
    let config = Arc::new(Config::from_env()?);
    let db = PgPoolOptions::new().connect(&config.database_url).await?;
    run_migrations(&db).await?;
    let password = cli::read_password_from_stdin()?;
    let state = AppState::new(db, config);
    let user = cli::create_admin(&state, &args, &password).await?;
    info!(nickname = %user.nickname, %user.id, "admin account created");
    Ok(())
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
    match cli::parse_args(&arguments())? {
        Command::Serve => serve(Config::from_env()?).await?,
        Command::CreateAdmin(args) => run_create_admin(args).await?,
        Command::Help(text) => print_stdout(text)?,
    }
    Ok(())
}
