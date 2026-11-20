//! The boot path: the embedded [`MIGRATOR`], [`run_migrations`] and the
//! [`serve`] entry point with its shutdown signal.
//!
//! [`serve`] starts the HTTP server, the [`Worker`] job runtime, the
//! periodic [`scheduler`] and the streaming listener, and builds the
//! router with [`app`]; the [`jobs`] submodule is the
//! handler registry it runs, re-exported here and at the crate root so
//! every existing `wired_be::` path keeps working.

pub mod jobs;

use std::{error::Error, net::SocketAddr, sync::Arc};

use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::{
    app,
    config::{Config, SECRET_KEY_BASE_VAR},
    jobs::{Worker, scheduler},
    services::{emoji_packs, instance_actor, runtime_config, vapid},
    state::AppState,
    streaming,
    util::token::{CREDENTIAL_BYTES, random_token},
};

pub use self::jobs::job_registry;

/// The migrations embedded from `migrations/`, applied by [`run_migrations`].
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Applies all pending migrations to the database behind `pool`.
///
/// # Errors
///
/// Returns a [`sqlx::migrate::MigrateError`] if any migration fails to run or
/// the migration bookkeeping table cannot be read or written.
pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    MIGRATOR.run(pool).await
}

/// Starts the HTTP server, the job worker, the periodic scheduler and
/// the streaming listener, and shuts everything down on ctrl-c or on a
/// graceful restart requested through the admin API.
///
/// Connects to PostgreSQL, applies [`MIGRATOR`], syncs the emoji packs
/// (see [`crate::services::emoji_packs`]), provisions the instance actor
/// (see [`crate::services::instance_actor`]) and the VAPID key pair (see
/// [`crate::services::vapid`]), binds
/// [`config.bind_addr`](Config::bind_addr), and runs [`app`] together with a
/// [`Worker`] over [`config`](Config::job_queues)
/// ([`Config::job_concurrency`] slots at once) until the process is
/// interrupted. Job handlers are bound in the
/// [`JobRegistry`](crate::jobs::JobRegistry) this builds:
/// today the federation delivery job
/// ([`delivery::KIND_DELIVER`](crate::federation::delivery::KIND_DELIVER)), the
/// streaming reference-table sweep
/// ([`KIND_CLEANUP`](streaming::hub::KIND_CLEANUP)), the mailer job
/// ([`mailer::KIND_SEND`](crate::mailer::KIND_SEND)), the expired-mute sweep
/// ([`mutes::KIND_EXPIRE`](crate::jobs::mutes::KIND_EXPIRE)), the expired
/// idempotency-key sweep
/// ([`idempotency::KIND_CLEANUP`](crate::jobs::idempotency::KIND_CLEANUP)), the
/// user-import jobs ([`imports::KIND_FOLLOWS`](crate::jobs::imports::KIND_FOLLOWS),
/// [`imports::KIND_BLOCKS`](crate::jobs::imports::KIND_BLOCKS) and
/// [`imports::KIND_MUTES`](crate::jobs::imports::KIND_MUTES)), the account
/// deletion jobs ([`deletion::KIND_DELETE`](crate::jobs::deletion::KIND_DELETE)
/// and the remote mirror's
/// [`KIND_DELETE_REMOTE`](crate::jobs::deletion::KIND_DELETE_REMOTE)), the
/// account backup jobs — the archive run of a request
/// ([`backups.run`](crate::api::backups::KIND_RUN),
/// [`jobs::backups::handle_run`](crate::jobs::backups::handle_run))
/// and its daily expiry sweep
/// ([`backups.purge`](crate::jobs::backups::KIND_PURGE)) —,
/// the Web Push delivery job
/// ([`web_push::KIND_DELIVER`](crate::jobs::web_push::KIND_DELIVER)), the poll-end
/// notification sweep ([`KIND_END_NOTIFY`](crate::jobs::polls::KIND_END_NOTIFY))
/// and the
/// federation intake pair — the incoming-activity dispatcher
/// ([`dispatch::KIND_INCOMING`](crate::federation::dispatch::KIND_INCOMING)) and
/// its daily received-activity ledger prune
/// ([`KIND_CLEANUP_RECEIVED`](crate::federation::dispatch::KIND_CLEANUP_RECEIVED))
/// , the fetched-thread backfill
/// ([`KIND_BACKFILL`](crate::federation::fetch_object::KIND_BACKFILL))
/// — and the outgoing fan-out publisher
/// ([`publisher::KIND_PUBLISH`](crate::federation::publisher::KIND_PUBLISH)), so
/// the worker idles, sweeps, delivers, sends, pushes, processes received
/// activities and fans out outgoing ones. The state's mail transport comes from
/// [`mailer::transport_from_config`](crate::mailer::transport_from_config): the SMTP relay when `SMTP_HOST` is
/// set, the disabled stand-in while it is not. An unset or blank
/// `SECRET_KEY_BASE` is filled with a fresh random signing secret for this
/// boot (with one warning in the log), so mail-unsubscribe links minted by
/// an earlier boot stop validating after a restart. Alongside it, a spawned
/// [`scheduler::run`] task enqueues the [`scheduler::periodic_jobs`]
/// recurring jobs, and a spawned
/// [`run_pg_listener`](streaming::hub::run_pg_listener) task mirrors the
/// other instances' [`Hub`](streaming::hub::Hub) events onto this
/// process' streaming bus,
/// honoring the same shutdown signal as the other tasks.
///
/// # Errors
///
/// Returns the connection, migration, emoji-sync, instance-actor, VAPID
/// key or bind failure that stopped startup.
pub async fn serve(config: Config) -> Result<(), Box<dyn Error + Send + Sync>> {
    let db = PgPoolOptions::new().connect(&config.database_url).await?;
    run_migrations(&db).await?;
    info!("database migrations applied");
    let config = Arc::new(with_generated_secret_key_base(config));
    let emoji_count =
        emoji_packs::sync_custom_emojis(&db, &config.base_url, &config.emoji_dir).await?;
    info!(emoji_count, "emoji packs synced into custom_emojis");
    let fetch_actor = instance_actor::ensure_instance_actor(&db, &config).await?;
    info!(ap_id = %fetch_actor.ap_id, "instance actor ready");
    let vapid_public = vapid::ensure_instance_keypair(&db, &config).await?;
    info!(vapid_public_key = %vapid_public, "VAPID key pair ready");
    let state = AppState::new(db, Arc::clone(&config));
    let listener = TcpListener::bind(state.config.bind_addr).await?;
    info!(addr = %state.config.bind_addr, "listening");
    let worker = Worker::new(
        state.clone(),
        Arc::new(job_registry()),
        state.config.job_queues.clone(),
        state.config.job_concurrency,
    );
    info!(
        queues = %state.config.job_queues.join(","),
        concurrency = state.config.job_concurrency,
        "job worker running"
    );
    let scheduler_task = tokio::spawn(scheduler::run(
        state.db.clone(),
        scheduler::periodic_jobs(),
        wait_for_shutdown(),
    ));
    let listener_task = tokio::spawn(streaming::hub::run_pg_listener(
        state.db.clone(),
        state.hub.clone(),
        wait_for_shutdown(),
    ));
    // The socket peer rides into the services as `ConnectInfo`, which is
    // what the rate limiter counts anonymous requests against (see
    // [`crate::rate_limit`]).
    let server = axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(wait_for_shutdown());
    let outcome = tokio::join!(
        server,
        worker.run(wait_for_shutdown()),
        scheduler_task,
        listener_task
    )
    .0;
    Ok(outcome?)
}

/// Returns `config` with its [`Config::secret_key_base`] filled: the mail
/// signing secret this instance runs on.
///
/// A configured secret passes through untouched. When the field is `None`
/// — `SECRET_KEY_BASE` unset or blank — the one [`Config::from_lookup`]
/// read is a deterministic function of the environment (AGENTS.md), so
/// the generation lives here at the boot path: a fresh [`random_token`]
/// over [`CREDENTIAL_BYTES`] becomes this boot's secret and one `warn!`
/// (without its value) says so, meaning links minted by an earlier boot
/// stop validating after a restart.
///
/// [`random_token`]: crate::util::token::random_token
/// [`CREDENTIAL_BYTES`]: crate::util::token::CREDENTIAL_BYTES
fn with_generated_secret_key_base(mut config: Config) -> Config {
    if config.secret_key_base.is_none() {
        warn!(
            var = SECRET_KEY_BASE_VAR,
            "no signing secret configured; generated an ephemeral one for this boot, \
             mail-unsubscribe links minted by an earlier boot stop validating"
        );
        config.secret_key_base = Some(random_token(CREDENTIAL_BYTES));
    }
    config
}

/// Resolves on the process-wide shutdown signal: the user pressing
/// ctrl-c, or an admin's `GET /api/v1/pleroma/admin/restart` raising the
/// graceful-restart flag that [`serve`] waits on here (the ctrl-c path
/// reused, so both drain everything and the process then exits with code
/// `0` — the supervisor's cue to start the next boot).
async fn wait_for_shutdown() {
    tokio::select! {
        () = runtime_config::RuntimeConfig::wait_for_restart() => {
            info!("graceful restart requested; draining");
        }
        signal = tokio::signal::ctrl_c() => {
            if let Err(err) = signal {
                error!(%err, "cannot listen for ctrl-c; the process must be stopped by signal");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config with only the required variables set, for tests that don't
    /// exercise config.
    fn test_config() -> Config {
        Config::for_tests()
    }

    /// A configured `SECRET_KEY_BASE` passes through the boot helper
    /// untouched.
    #[test]
    fn with_generated_secret_key_base_keeps_a_configured_secret() {
        let mut config = test_config();
        config.secret_key_base = Some("a-signing-secret".to_owned());
        let config = with_generated_secret_key_base(config);
        assert_eq!(
            config.secret_key_base.as_deref(),
            Some("a-signing-secret"),
            "a set secret is kept verbatim"
        );
    }

    /// An unset secret becomes a generated one: `Some` and non-empty.
    #[test]
    fn with_generated_secret_key_base_fills_an_unset_secret() {
        let mut config = test_config();
        config.secret_key_base = None;
        let config = with_generated_secret_key_base(config);
        let secret = config.secret_key_base.expect("the unset secret is filled");
        assert!(!secret.is_empty(), "the generated secret is non-empty");
    }
}

#[cfg(test)]
mod migrations;
