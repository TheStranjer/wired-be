//! wired-be: an API-only fediverse backend built on Axum.

pub mod api;
pub mod auth;
pub mod config;
pub mod entities;
pub mod error;
pub mod extract;
pub mod federation;
pub mod feeds;
pub mod http_client;
pub mod jobs;
pub mod mailer;
pub mod middleware;
pub mod models;
pub mod oauth;
pub mod pagination;
pub mod rate_limit;
pub mod services;
pub mod state;
pub mod streaming;
#[cfg(any(test, feature = "integration-tests"))]
pub mod test_db;
pub mod uploads;
pub mod util;

use std::{error::Error, net::SocketAddr, sync::Arc};

use axum::{
    Router,
    http::{StatusCode, Uri},
    routing::get,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::{
    api::media_files,
    config::{Config, SECRET_KEY_BASE_VAR},
    error::AppError,
    http_client::HttpClient,
    jobs::{JobRegistry, Worker, scheduler},
    services::{emoji_packs, instance_actor, media_proxy, vapid},
    state::AppState,
    streaming::hub::Hub,
    uploads::UploaderBackend,
    util::token::{CREDENTIAL_BYTES, random_token},
};

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

/// Starts the HTTP server, the job worker, the periodic scheduler and the
/// streaming listener, and shuts everything down on ctrl-c.
///
/// Connects to PostgreSQL, applies [`MIGRATOR`], syncs the emoji packs
/// (see [`crate::services::emoji_packs`]), provisions the instance actor
/// (see [`crate::services::instance_actor`]) and the VAPID key pair (see
/// [`crate::services::vapid`]), binds
/// [`config.bind_addr`](Config::bind_addr), and runs [`app`] together with a
/// [`Worker`] over [`config`](Config::job_queues)
/// ([`Config::job_concurrency`] slots at once) until the process is
/// interrupted. Job handlers are bound in the [`JobRegistry`] this builds:
/// today the federation delivery job
/// ([`delivery::KIND_DELIVER`](federation::delivery::KIND_DELIVER)), the
/// streaming reference-table sweep
/// ([`KIND_CLEANUP`](streaming::hub::KIND_CLEANUP)), the mailer job
/// ([`mailer::KIND_SEND`]), the expired-mute sweep
/// ([`mutes::KIND_EXPIRE`](jobs::mutes::KIND_EXPIRE)), the expired
/// idempotency-key sweep
/// ([`idempotency::KIND_CLEANUP`](jobs::idempotency::KIND_CLEANUP)), the
/// user-import jobs ([`imports::KIND_FOLLOWS`](jobs::imports::KIND_FOLLOWS),
/// [`imports::KIND_BLOCKS`](jobs::imports::KIND_BLOCKS) and
/// [`imports::KIND_MUTES`](jobs::imports::KIND_MUTES)), the account
/// deletion jobs ([`deletion::KIND_DELETE`](jobs::deletion::KIND_DELETE)
/// and the remote mirror's
/// [`KIND_DELETE_REMOTE`](jobs::deletion::KIND_DELETE_REMOTE)),
/// the Web Push delivery job
/// ([`web_push::KIND_DELIVER`](jobs::web_push::KIND_DELIVER)), the poll-end
/// notification sweep ([`KIND_END_NOTIFY`](jobs::polls::KIND_END_NOTIFY))
/// and the
/// federation intake pair — the incoming-activity dispatcher
/// ([`dispatch::KIND_INCOMING`](federation::dispatch::KIND_INCOMING)) and
/// its daily received-activity ledger prune
/// ([`KIND_CLEANUP_RECEIVED`](federation::dispatch::KIND_CLEANUP_RECEIVED))
/// — and the outgoing fan-out publisher
/// ([`publisher::KIND_PUBLISH`](federation::publisher::KIND_PUBLISH)), so
/// the worker idles, sweeps, delivers, sends, pushes, processes received
/// activities and fans out outgoing ones. The state's mail transport comes from
/// [`mailer::transport_from_config`]: the SMTP relay when `SMTP_HOST` is
/// set, the disabled stand-in while it is not. An unset or blank
/// `SECRET_KEY_BASE` is filled with a fresh random signing secret for this
/// boot (with one warning in the log), so mail-unsubscribe links minted by
/// an earlier boot stop validating after a restart. Alongside it, a spawned
/// [`scheduler::run`] task enqueues the [`scheduler::periodic_jobs`]
/// recurring jobs, and a spawned
/// [`run_pg_listener`](streaming::hub::run_pg_listener) task mirrors the
/// other instances' [`Hub`] events onto this process' streaming bus,
/// honoring the same ctrl-c shutdown as the other tasks.
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
    let hub = Hub::new(&db);
    let state = AppState {
        db,
        hub,
        config: Arc::clone(&config),
        http_client: HttpClient::from(config.as_ref()),
        uploader: UploaderBackend::from(config.as_ref()),
        instance_stats_cache: Arc::default(),
        mrf_config_cache: Arc::default(),
        translation_languages_cache: Arc::default(),
        rate_limiter: Arc::new(rate_limit::RateLimiter::from_config(config.as_ref())),
        mailer: mailer::transport_from_config(&config),
    };
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
        wait_for_ctrl_c(),
    ));
    let listener_task = tokio::spawn(streaming::hub::run_pg_listener(
        state.db.clone(),
        state.hub.clone(),
        wait_for_ctrl_c(),
    ));
    // The socket peer rides into the services as `ConnectInfo`, which is
    // what the rate limiter counts anonymous requests against (see
    // [`crate::rate_limit`]).
    let server = axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(wait_for_ctrl_c());
    let outcome = tokio::join!(
        server,
        worker.run(wait_for_ctrl_c()),
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

/// The [`JobRegistry`] the [`serve`] worker runs: every job kind this
/// instance handles, bound to its handler.
fn job_registry() -> JobRegistry {
    let mut registry = JobRegistry::new();
    registry.register(
        federation::delivery::KIND_DELIVER,
        federation::delivery::handle,
    );
    registry.register(streaming::hub::KIND_CLEANUP, streaming::hub::handle_cleanup);
    registry.register(jobs::mutes::KIND_EXPIRE, jobs::mutes::handle_expire);
    registry.register(
        jobs::idempotency::KIND_CLEANUP,
        jobs::idempotency::handle_cleanup,
    );
    registry.register(
        jobs::maintenance::KIND_NOTIFICATIONS,
        jobs::maintenance::handle_notifications,
    );
    registry.register(
        jobs::maintenance::KIND_TOKENS,
        jobs::maintenance::handle_tokens,
    );
    registry.register(jobs::maintenance::KIND_JOBS, jobs::maintenance::handle_jobs);
    registry.register(
        jobs::maintenance::KIND_DELETED_STATUSES,
        jobs::maintenance::handle_purge_deleted_statuses,
    );
    registry.register(
        jobs::maintenance::KIND_MEDIA,
        jobs::maintenance::handle_sweep_media,
    );
    registry.register(jobs::imports::KIND_FOLLOWS, jobs::imports::handle_follows);
    registry.register(jobs::imports::KIND_BLOCKS, jobs::imports::handle_blocks);
    registry.register(jobs::imports::KIND_MUTES, jobs::imports::handle_mutes);
    registry.register(jobs::deletion::KIND_DELETE, jobs::deletion::handle_delete);
    registry.register(
        jobs::deletion::KIND_DELETE_REMOTE,
        jobs::deletion::handle_delete_remote,
    );
    registry.register(jobs::statuses::KIND_EXPIRE, jobs::statuses::handle_expire);
    registry.register(jobs::statuses::KIND_SWEEP, jobs::statuses::handle_sweep);
    registry.register(
        jobs::scheduled_statuses::KIND_PUBLISH,
        jobs::scheduled_statuses::handle_publish,
    );
    registry.register(
        jobs::scheduled_statuses::KIND_SWEEP,
        jobs::scheduled_statuses::handle_sweep,
    );
    registry.register(jobs::web_push::KIND_DELIVER, jobs::web_push::handle_deliver);
    registry.register(jobs::rich_media::KIND_FETCH, jobs::rich_media::handle_fetch);
    registry.register(jobs::polls::KIND_END_NOTIFY, jobs::polls::handle_end_notify);
    registry.register(mailer::KIND_SEND, mailer::handle);
    registry.register(
        federation::dispatch::KIND_INCOMING,
        federation::dispatch::handle,
    );
    registry.register(
        federation::publisher::KIND_PUBLISH,
        federation::publisher::handle,
    );
    registry.register(
        federation::dispatch::KIND_CLEANUP_RECEIVED,
        federation::dispatch::handle_cleanup_received,
    );
    registry
}

/// Resolves when the user presses ctrl-c, the process-wide shutdown signal.
async fn wait_for_ctrl_c() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        error!(%err, "cannot listen for ctrl-c; the process must be stopped by signal");
    }
}

/// Builds the application router, wrapped in the [`middleware`] stack.
///
/// The [`api`] routers are merged under their prefixes; the routers that
/// serve addresses outside `/api` fixed by their protocols, by stored URLs
/// or by the clients that call them — `/media` ([`media_files`]), `/proxy`
/// ([`media_proxy`]), `/emoji` ([`api::emoji_packs`]), `/users/{nickname}`
/// and `/internal/fetch` ([`api::activitypub`]) and its feed variants plus
/// `/tags/{tag}` ([`api::feeds`]), `/.well-known` ([`api::well_known`]),
/// `/.well-known/nodeinfo` and `/nodeinfo/{version}` ([`api::nodeinfo`]),
/// `/oauth` ([`api::oauth`]), the `/oauth` provider-login paths
/// ([`api::oauth_external`]), `POST /auth/password` ([`api::password_reset`],
/// Pleroma routes the reset request outside `/api`), and the `MongooseIM`
/// auth endpoints plus `/manifest.json` ([`api::misc`]) — are merged at the
/// top level, together with the HTML helper pages of
/// [`api::html_pages`] (`/frontend_switcher`, `/embed/{id}`,
/// `/notice/{id}/embed_player` and `/mailer/unsubscribe/{token}`). Paths
/// no route matches get the standard JSON error response (decision 8).
///
/// `state` is handed to [`api::router`] and [`api::oauth::router`] before
/// it is consumed: the registration routes those build mount the rate
/// limiter (see [`crate::rate_limit`]), whose middleware layer carries its
/// own cheap clone of the shared state.
///
/// [`api::emoji_packs`]: crate::api::emoji_packs::files_router
pub fn app(state: AppState) -> Router {
    middleware::apply(
        Router::new()
            .route("/health", get(health))
            .merge(api::router(&state))
            .merge(api::activitypub::router())
            .merge(api::feeds::router())
            .merge(api::oauth::router(&state))
            .merge(api::oauth_external::router())
            .merge(api::password_reset::auth_router(&state))
            .merge(api::well_known::router())
            .merge(api::nodeinfo::router())
            .merge(media_files::router())
            .merge(media_proxy::router())
            .merge(api::emoji_packs::files_router())
            .merge(api::misc::router())
            .merge(api::html_pages::router())
            .fallback(not_found)
            .with_state(state),
    )
}

/// Liveness probe.
async fn health() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// Fallback handler for every unmatched path, answered as a JSON 404
/// (decision 8). Pleroma's router-level fallback distinguishes unknown
/// endpoints under `/api` — 404 "Not implemented"
/// (`lib/pleroma/web/fallback/redirect_controller.ex`, `api_not_implemented`)
/// — from every other unknown path, 404 "Not found"; both are produced here
/// as [`AppError`]s. Known paths called with the wrong method never reach
/// this handler, so they keep axum's default 405.
async fn not_found(uri: Uri) -> AppError {
    let path = uri.path();
    if path == "/api" || path.starts_with("/api/") {
        AppError::NotImplemented
    } else {
        AppError::RouteNotFound
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt as _;
    use sqlx::postgres::PgPoolOptions;
    use std::sync::Arc;
    use tower::ServiceExt as _;

    use crate::{
        config::Config, http_client::HttpClient, streaming::hub::Hub, uploads::UploaderBackend,
    };

    /// A config with only the required variables set, for tests that don't
    /// exercise config.
    fn test_config() -> Config {
        Config::for_tests()
    }

    /// Router backed by a pool that never connects, for routes that don't touch the DB.
    fn test_app() -> Router {
        let db = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap();
        let config = Arc::new(test_config());
        app(AppState {
            hub: Hub::new(&db),
            db,
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            mrf_config_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(mailer::MemoryTransport::new()),
        })
    }

    #[tokio::test]
    async fn health_returns_no_content() {
        let response = test_app()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "health status");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(body.is_empty(), "health body should be empty");
    }

    #[tokio::test]
    async fn unknown_route_returns_not_found() {
        let response = test_app()
            .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "unknown route status"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            r#"{"error":"Not found"}"#,
            "unknown non-API route body is the router-level 404"
        );
    }

    #[tokio::test]
    async fn unknown_api_route_returns_not_implemented() {
        let response = test_app()
            .oneshot(Request::get("/api/v1/nothing").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "unrouted API path status"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            r#"{"error":"Not implemented"}"#,
            "unrouted path under /api is Pleroma's api_not_implemented"
        );
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

    /// Migration tests, run against a slot of the pooled fixture in
    /// [`crate::test_db`].
    mod migrations {

        /// One foreign key of the three tables: its name, the table it
        /// was declared on, its delete action and what it points at.
        #[derive(Debug, sqlx::FromRow)]
        struct ForeignKey {
            /// The table carrying the constraint.
            relname: String,
            /// The constraint's name.
            conname: String,
            /// Its `ON DELETE` action, as PostgreSQL spells it.
            confdeltype: String,
            /// The table it references.
            target: String,
        }

        /// One column of the three tables, as `information_schema`
        /// describes it.
        #[derive(Debug, sqlx::FromRow)]
        struct SchemaColumn {
            /// The table it belongs to.
            table_name: String,
            /// Its own name.
            column_name: String,
            /// The default expression, `None` when it has none.
            column_default: Option<String>,
            /// `NO` for a `NOT NULL` column, `YES` otherwise.
            is_nullable: String,
        }

        /// One column of the table, as `information_schema` describes
        /// it.
        #[derive(Debug, sqlx::FromRow)]
        struct RuleColumn {
            /// Its name.
            column_name: String,
            /// The declared type.
            data_type: String,
            /// `NO` for a `NOT NULL` column, `YES` otherwise.
            is_nullable: String,
            /// The default expression, `None` when it has none.
            column_default: Option<String>,
        }

        /// One column of the table, as `information_schema` describes it.
        #[derive(Debug, sqlx::FromRow)]
        struct StateColumn {
            /// Its name.
            column_name: String,
            /// The declared type.
            data_type: String,
            /// `NO` for a `NOT NULL` column, `YES` otherwise.
            is_nullable: String,
            /// The default expression, `None` when it has none.
            column_default: Option<String>,
        }

        /// A column as a `(name, type, nullability, default)` tuple, the shape
        /// the `oauth_login_states` expectation is written in.
        type Described = (String, String, String, Option<String>);
        /// One expected foreign key: the table it was declared on, its name,
        /// its delete action and the table it points at.
        type ExpectedKey = (&'static str, &'static str, &'static str, &'static str);

        /// The indexes the statuses migrations create on `statuses`: the
        /// primary key, the uri lookup, the six the timeline and thread
        /// queries read, and the partial unique key that lets an account
        /// reblog a given status once.
        const STATUS_INDEXES: [&str; 9] = [
            "statuses_pkey",
            "statuses_uri_key",
            "statuses_user_id_idx",
            "statuses_in_reply_to_id_idx",
            "statuses_conversation_id_idx",
            "statuses_reblog_of_id_idx",
            "statuses_visibility_idx",
            "statuses_quote_of_id_idx",
            "statuses_user_id_reblog_of_id_key",
        ];
        /// The live-row-partial statuses indexes: the two newest-first
        /// listings, so tombstones drop out of them for free, and the
        /// unique reblog pair, so a withdrawn boost frees the pair again.
        const PARTIAL_STATUS_INDEXES: [&str; 3] = [
            "statuses_user_id_idx",
            "statuses_visibility_idx",
            "statuses_user_id_reblog_of_id_key",
        ];
        /// The foreign keys the chats migration declares: table, constraint,
        /// `ON DELETE` action and target.
        const CHAT_FOREIGN_KEYS: [ExpectedKey; 6] = [
            ("chats", "chats_user_id_fkey", "c", "users"),
            ("chats", "chats_recipient_id_fkey", "c", "users"),
            (
                "chat_messages",
                "chat_messages_sender_id_fkey",
                "c",
                "users",
            ),
            (
                "chat_messages",
                "chat_messages_attachment_media_id_fkey",
                "n",
                "media_attachments",
            ),
            (
                "chat_message_references",
                "chat_message_references_chat_id_fkey",
                "c",
                "chats",
            ),
            (
                "chat_message_references",
                "chat_message_references_message_id_fkey",
                "c",
                "chat_messages",
            ),
        ];
        /// The unique keys the chats migration declares.
        const CHAT_UNIQUE_KEYS: [(&str, &str); 3] = [
            ("chats", "chats_user_id_recipient_id_key"),
            (
                "chat_message_references",
                "chat_message_references_chat_id_message_id_key",
            ),
            ("chat_messages", "chat_messages_ap_id_key"),
        ];
        /// The indexes the chats migration declares for its list queries.
        const CHAT_INDEXES: [&str; 6] = [
            "chats_user_id_updated_at_idx",
            "chats_recipient_id_idx",
            "chat_messages_sender_id_idx",
            "chat_message_references_chat_id_idx",
            "chat_message_references_chat_id_unread_idx",
            "chat_message_references_message_id_idx",
        ];
        /// The `chat_messages` columns the migration defaults for the model,
        /// and the columns it makes `NOT NULL`.
        const CHAT_DEFAULTS: [(&str, &str, &str); 4] = [
            ("chats", "pinned", "false"),
            ("chat_messages", "content", "''::text"),
            ("chat_messages", "raw_text", "''::text"),
            ("chat_message_references", "unread", "true"),
        ];
        /// The columns of the three chat tables no row may leave unset.
        const CHAT_REQUIRED_COLUMNS: [(&str, &str); 5] = [
            ("chats", "user_id"),
            ("chats", "recipient_id"),
            ("chat_messages", "sender_id"),
            ("chat_message_references", "chat_id"),
            ("chat_message_references", "message_id"),
        ];
        /// The `instance_rules` columns and their types, in the order the
        /// migration declares them.
        const RULE_COLUMNS: [(&str, &str); 6] = [
            ("id", "uuid"),
            ("text", "text"),
            ("hint", "text"),
            ("priority", "integer"),
            ("inserted_at", "timestamp with time zone"),
            ("updated_at", "timestamp with time zone"),
        ];
        /// The timestamps of a table, defaulted by the database to `now()`.
        const NOW_COLUMNS: [&str; 2] = ["inserted_at", "updated_at"];

        /// The `indexname`s of `rows`, the `pg_indexes` rows of one table.
        fn index_names(rows: &[(String, String)]) -> Vec<&str> {
            rows.iter().map(|(name, _)| name.as_str()).collect()
        }

        /// The `indexdef` of the index named `name` in `rows`.
        ///
        /// # Panics
        ///
        /// If `rows` carries no such index.
        fn index_definition<'rows>(rows: &'rows [(String, String)], name: &str) -> &'rows str {
            rows.iter()
                .find(|(indexname, _)| indexname == name)
                .map_or_else(
                    || panic!("{name} exists: {rows:?}"),
                    |(_, definition)| definition.as_str(),
                )
        }

        /// The row of `columns` describing `table`'s column `name`.
        ///
        /// # Panics
        ///
        /// If `columns` does not carry that column.
        fn column_of<'columns>(
            columns: &'columns [SchemaColumn],
            table: &str,
            name: &str,
        ) -> &'columns SchemaColumn {
            columns
                .iter()
                .find(|row| row.table_name == table && row.column_name == name)
                .unwrap_or_else(|| panic!("{table}.{name} exists"))
        }

        /// The row of `columns` describing the `instance_rules` column `name`.
        ///
        /// # Panics
        ///
        /// If `columns` does not carry that column.
        fn rule_column<'columns>(
            columns: &'columns [RuleColumn],
            name: &str,
        ) -> &'columns RuleColumn {
            columns
                .iter()
                .find(|column| column.column_name == name)
                .unwrap_or_else(|| panic!("instance_rules.{name} exists"))
        }

        /// `columns` as `(name, type, nullability, default)` tuples.
        fn described_of(columns: Vec<StateColumn>) -> Vec<Described> {
            columns
                .into_iter()
                .map(|column| {
                    (
                        column.column_name,
                        column.data_type,
                        column.is_nullable,
                        column.column_default,
                    )
                })
                .collect()
        }

        /// Every index the statuses migrations create, the three partial on
        /// live rows only, and the one live reblog pair per account unique.
        fn assert_statuses_indexes(rows: &[(String, String)]) {
            let names = index_names(rows);
            for expected in STATUS_INDEXES {
                assert!(names.contains(&expected), "{expected} indexes statuses");
            }
            for partial in PARTIAL_STATUS_INDEXES {
                assert!(
                    index_definition(rows, partial).contains("deleted_at IS NULL"),
                    "{partial} is partial on live rows only"
                );
            }
            assert!(
                index_definition(rows, "statuses_user_id_reblog_of_id_key").contains("UNIQUE"),
                "the live reblog pair indexes unique, so an account reblogs a status once"
            );
        }

        /// Every foreign key the chats migration declares, with its delete
        /// action and target.
        fn assert_chat_foreign_keys(keys: &[ForeignKey]) {
            for (table, constraint, action, target) in CHAT_FOREIGN_KEYS {
                let found = keys
                    .iter()
                    .find(|key| key.relname == table && key.conname == constraint)
                    .unwrap_or_else(|| panic!("{constraint} links {table}"));
                assert_eq!(
                    found.confdeltype, action,
                    "{constraint} deletes as {action}"
                );
                assert_eq!(found.target, target, "{constraint} points at {target}");
            }
        }

        /// The unique keys that keep one chat per participant and account, one
        /// reference per chat and message, and one `ap_id` per message.
        fn assert_chat_unique_keys(uniques: &[(String, String)]) {
            for (table, constraint) in CHAT_UNIQUE_KEYS {
                assert!(
                    uniques
                        .iter()
                        .any(|(relname, conname)| relname == table && conname == constraint),
                    "{constraint} keeps {table} free of duplicates"
                );
            }
        }

        /// The indexes the chat list queries read.
        fn assert_chat_indexes(indexes: &[(String, String)]) {
            for name in CHAT_INDEXES {
                assert!(
                    indexes.iter().any(|(indexname, _)| indexname == name),
                    "{name} serves a chat query"
                );
            }
        }

        /// The defaults the chats migration leaves the model to rely on, and
        /// the `NOT NULL`s it fills in for them.
        fn assert_chat_column_defaults(columns: &[SchemaColumn]) {
            for (table, column, default) in CHAT_DEFAULTS {
                let row = column_of(columns, table, column);
                assert_eq!(
                    row.column_default.as_deref(),
                    Some(default),
                    "{table}.{column} defaults to {default}"
                );
                assert_eq!(row.is_nullable, "NO", "{table}.{column} is NOT NULL");
            }
        }

        /// The columns of the three chat tables no row may leave unset.
        fn assert_chat_required_columns(columns: &[SchemaColumn]) {
            for (table, column) in CHAT_REQUIRED_COLUMNS {
                assert_eq!(
                    column_of(columns, table, column).is_nullable,
                    "NO",
                    "{table}.{column} cannot be null"
                );
            }
        }

        /// The type and nullability of every `instance_rules` column.
        fn assert_rule_column_types(columns: &[RuleColumn]) {
            for (name, data_type) in RULE_COLUMNS {
                assert_eq!(
                    rule_column(columns, name).data_type,
                    data_type,
                    "instance_rules.{name} is {data_type}"
                );
                assert_eq!(
                    rule_column(columns, name).is_nullable,
                    "NO",
                    "instance_rules.{name} is NOT NULL"
                );
            }
        }

        /// The two timestamps the `instance_rules` migration stamps itself.
        fn assert_rule_timestamps(columns: &[RuleColumn]) {
            for name in NOW_COLUMNS {
                assert_eq!(
                    rule_column(columns, name).column_default.as_deref(),
                    Some("now()"),
                    "{name} defaults to now()"
                );
            }
        }

        /// The base extensions migration installs `citext`, `pg_trgm` and `pgcrypto`.
        #[tokio::test]
        async fn extensions_migration_installs_all_three_extensions() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let row: (i64,) = sqlx::query_as(
                "SELECT count(*) FROM pg_extension WHERE extname IN ('citext','pg_trgm','pgcrypto')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(row.0, 3, "all three extensions should be installed");
        }

        /// The statuses migrations create every index the timeline, thread
        /// and reblog queries need — three of them partial on
        /// `deleted_at IS NULL`, so tombstones drop out of them for free,
        /// and one of those keeps the live reblog pair unique — and gives
        /// `media_attachments.status_id` its foreign key into the
        /// new table.
        #[tokio::test]
        async fn statuses_migration_indexes_statuses_and_links_media() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let rows: Vec<(String, String)> = sqlx::query_as(
                "SELECT indexname, indexdef FROM pg_indexes
                  WHERE schemaname = 'public' AND tablename = 'statuses'",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_statuses_indexes(&rows);

            let keys: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT con.conname, con.confdeltype::text, con.confrelid::regclass::text
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                  WHERE cls.relname = 'media_attachments' AND con.contype = 'f'",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            let status_key = keys
                .iter()
                .find(|(name, _, _)| name == "media_attachments_status_id_fkey")
                .unwrap();
            assert_eq!(
                status_key.1, "c",
                "status_id's foreign key deletes its media"
            );
            assert_eq!(
                status_key.2, "statuses",
                "and points at the table this migration created"
            );
        }

        /// The chats migration creates the three tables `src/models/chat.rs`
        /// reads and writes — one chat per participant, the shared message,
        /// and one reference per participant's chat — and links each of them
        /// the way the model's queries assume: the accounts and the media a
        /// message may name, the cascades that take a conversation apart with
        /// its owners, and the keys its upserts conflict on.
        #[tokio::test]
        async fn chats_migration_links_both_sides_of_a_conversation() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let keys: Vec<ForeignKey> = sqlx::query_as(
                "SELECT cls.relname, con.conname, con.confdeltype::text, conf.relname AS target
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_class conf ON conf.oid = con.confrelid
                  WHERE con.contype = 'f'
                    AND cls.relname IN ('chats', 'chat_messages', 'chat_message_references')",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_chat_foreign_keys(&keys);
        }

        /// The unique keys and indexes the chats migration adds: one chat per
        /// participant and account, one reference per chat and message, a
        /// reserved `ap_id`, and the newest-first and unread-only indexes the
        /// list queries read.
        #[tokio::test]
        async fn chats_migration_keys_and_indexes_the_chat_tables() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let uniques: Vec<(String, String)> = sqlx::query_as(
                "SELECT cls.relname, con.conname
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                  WHERE con.contype = 'u'
                    AND cls.relname IN ('chats', 'chat_messages', 'chat_message_references')",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_chat_unique_keys(&uniques);

            let indexes: Vec<(String, String)> = sqlx::query_as(
                "SELECT indexname, indexdef FROM pg_indexes
                  WHERE schemaname = 'public'
                    AND tablename IN ('chats', 'chat_messages', 'chat_message_references')",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_chat_indexes(&indexes);
            let (unread_name, unread_def) = indexes
                .iter()
                .find(|(indexname, _)| indexname == "chat_message_references_chat_id_unread_idx")
                .unwrap();
            assert!(
                unread_def.contains("WHERE unread"),
                "the unread index {unread_name} holds only unread references: {unread_def}"
            );
        }

        /// The column defaults and `NOT NULL`s the chats migration gives the
        /// three tables, which are exactly what `src/models/chat.rs` leaves
        /// for the database to fill.
        #[tokio::test]
        async fn chats_migration_defaults_match_the_model() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let columns: Vec<SchemaColumn> = sqlx::query_as(
                "SELECT table_name, column_name, column_default, is_nullable
                   FROM information_schema.columns
                  WHERE table_schema = 'public'
                    AND table_name IN ('chats', 'chat_messages', 'chat_message_references')",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_chat_column_defaults(&columns);
            assert_chat_required_columns(&columns);
            assert!(
                column_of(&columns, "chat_messages", "deleted_at")
                    .column_default
                    .is_none()
                    && column_of(&columns, "chat_messages", "deleted_at").is_nullable == "YES",
                "a live message has no deleted_at until it is withdrawn"
            );
        }

        /// The instance-settings migration creates the tiny key/value
        /// table `src/models/instance_setting.rs` reads: the `text`
        /// primary key its upsert conflicts on, the jsonb value and the
        /// `now()`-defaulted `updated_at`.
        #[tokio::test]
        async fn instance_settings_migration_creates_the_settings_table() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let columns: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT column_name, data_type, is_nullable
                   FROM information_schema.columns
                  WHERE table_schema = 'public' AND table_name = 'instance_settings'
               ORDER BY column_name",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(
                columns,
                vec![
                    ("key".to_owned(), "text".to_owned(), "NO".to_owned()),
                    (
                        "updated_at".to_owned(),
                        "timestamp with time zone".to_owned(),
                        "NO".to_owned()
                    ),
                    ("value".to_owned(), "jsonb".to_owned(), "NO".to_owned()),
                ],
                "the ticket's three columns with the ticket's types"
            );

            let (primary_key, key_columns): (String, String) = sqlx::query_as(
                "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
                  WHERE con.contype = 'p' AND cls.relname = 'instance_settings'
               GROUP BY con.conname",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                key_columns, "key",
                "the key alone is the primary key {primary_key}"
            );

            let updated_default: Option<String> = sqlx::query_scalar(
                "SELECT column_default FROM information_schema.columns
                  WHERE table_schema = 'public' AND table_name = 'instance_settings'
                    AND column_name = 'updated_at'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                updated_default.as_deref(),
                Some("now()"),
                "updated_at defaults to now()"
            );
        }

        /// The instance-rules migration creates the ordered rule list
        /// `src/models/instance_rule.rs` reads and writes: the minted
        /// `uuid` primary key, the rule text, the defaulted hint and
        /// priority, and the `now()`-stamped timestamps.
        #[tokio::test]
        async fn instance_rules_migration_creates_the_rules_table() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let columns: Vec<RuleColumn> = sqlx::query_as(
                "SELECT column_name, data_type, is_nullable, column_default
                   FROM information_schema.columns
                  WHERE table_schema = 'public' AND table_name = 'instance_rules'
               ORDER BY ordinal_position",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            let names: Vec<&str> = columns
                .iter()
                .map(|column| column.column_name.as_str())
                .collect();
            assert_eq!(
                names,
                vec![
                    "id",
                    "text",
                    "hint",
                    "priority",
                    "inserted_at",
                    "updated_at"
                ],
                "the ticket's columns, in the ticket's order"
            );
            assert_rule_column_types(&columns);
            assert_eq!(
                rule_column(&columns, "id").column_default,
                None,
                "the id is minted in Rust (decision 4)"
            );
            assert_eq!(
                rule_column(&columns, "text").column_default,
                None,
                "text has no default: the rule is required"
            );
            assert_eq!(
                rule_column(&columns, "hint").column_default.as_deref(),
                Some("''::text"),
                "the hint defaults to empty"
            );
            assert_eq!(
                rule_column(&columns, "priority").column_default.as_deref(),
                Some("0"),
                "the priority defaults to the middle of the list"
            );
            assert_rule_timestamps(&columns);

            let (primary_key, key_columns): (String, String) = sqlx::query_as(
                "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
                  WHERE con.contype = 'p' AND cls.relname = 'instance_rules'
               GROUP BY con.conname",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                key_columns, "id",
                "the id alone is the primary key {primary_key}"
            );
        }

        /// The list-members position migration gives `list_members` the
        /// ordinal `GET /api/v1/lists/:id/accounts` pages on: a `bigint`
        /// `NOT NULL` column defaulted from the sequence it owns (so a plain
        /// insert takes the next number, which is the order members were added
        /// in), the primary key still the `(list_id, user_id)` pair that makes
        /// membership idempotent, and one index serving a list's own positions.
        #[tokio::test]
        async fn list_members_position_migration_adds_the_ordinal() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let (data_type, is_nullable, default): (String, String, Option<String>) =
                sqlx::query_as(
                    "SELECT data_type, is_nullable, column_default
                       FROM information_schema.columns
                      WHERE table_schema = 'public'
                        AND table_name = 'list_members'
                        AND column_name = 'position'",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(data_type, "bigint", "position is a bigint ordinal");
            assert_eq!(is_nullable, "NO", "and every membership has one");
            assert_eq!(
                default.as_deref(),
                Some("nextval('list_members_position_seq'::regclass)"),
                "the database fills it: {default:?}"
            );

            let sequence: (String,) =
                sqlx::query_as("SELECT pg_get_serial_sequence('list_members', 'position')")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                sequence.0, "public.list_members_position_seq",
                "one sequence, owned by the column and dropped with it"
            );

            let key: Vec<String> = sqlx::query_scalar(
                "SELECT att.attname
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
                  WHERE con.contype = 'p' AND cls.relname = 'list_members'
               ORDER BY att.attname",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(
                key,
                vec!["list_id".to_owned(), "user_id".to_owned()],
                "the pair is still the key: position is an order, not an id"
            );

            let indexes: Vec<(String, String)> = sqlx::query_as(
                "SELECT indexname, indexdef FROM pg_indexes
                  WHERE schemaname = 'public' AND tablename = 'list_members'",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            let (_, definition) = indexes
                .iter()
                .find(|(name, _)| name == "list_members_list_id_position_idx")
                .unwrap();
            assert!(
                definition.contains("list_id") && definition.contains("position"),
                "the page's index covers the list filter and its cursor: {definition}"
            );
        }

        /// The chat message idempotency migration (WBE-147) adds the one
        /// column the `Idempotency-Key` replay lookup needs — a nullable
        /// `text` `idempotency_key` on `chat_messages` — and the partial
        /// index that serves the sender-and-key search, keeping every
        /// keyless message out of it.
        #[tokio::test]
        async fn chat_message_idempotency_migration_adds_the_key() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let (data_type, is_nullable, default): (String, String, Option<String>) =
                sqlx::query_as(
                    "SELECT data_type, is_nullable, column_default
                       FROM information_schema.columns
                      WHERE table_schema = 'public'
                        AND table_name = 'chat_messages'
                        AND column_name = 'idempotency_key'",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(data_type, "text", "the key is stored as text");
            assert_eq!(is_nullable, "YES", "a keyless message keeps NULL");
            assert!(default.is_none(), "and nothing defaults to a key");

            let indexes: Vec<(String, String)> = sqlx::query_as(
                "SELECT indexname, indexdef FROM pg_indexes
                  WHERE schemaname = 'public' AND tablename = 'chat_messages'",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            let (_, definition) = indexes
                .iter()
                .find(|(name, _)| name == "chat_messages_sender_id_idempotency_key_idx")
                .unwrap_or_else(|| panic!("the replay index exists: {indexes:?}"));
            assert!(
                definition.contains("sender_id")
                    && definition.contains("idempotency_key")
                    && definition.contains("idempotency_key IS NOT NULL"),
                "the index covers sender and key, keyed rows only: {definition}"
            );
        }

        /// The external-identity migration (WBE-218) creates the table the
        /// `/oauth/{provider}/callback` looks a login up by: the minted
        /// `uuid` primary key, the `user_id` foreign key that takes a
        /// provider link down with its account, and the `(provider, uid)`
        /// pair that makes one provider account exactly one login.
        #[tokio::test]
        async fn user_identities_migration_links_providers_to_users() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let columns: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT column_name, data_type, is_nullable
                   FROM information_schema.columns
                  WHERE table_schema = 'public' AND table_name = 'user_identities'
               ORDER BY ordinal_position",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(
                columns,
                vec![
                    ("id".to_owned(), "uuid".to_owned(), "NO".to_owned()),
                    ("user_id".to_owned(), "uuid".to_owned(), "NO".to_owned()),
                    ("provider".to_owned(), "text".to_owned(), "NO".to_owned()),
                    ("uid".to_owned(), "text".to_owned(), "NO".to_owned()),
                    (
                        "inserted_at".to_owned(),
                        "timestamp with time zone".to_owned(),
                        "NO".to_owned()
                    ),
                ],
                "the ticket's five columns, in the ticket's order"
            );

            let (key, key_columns): (String, String) = sqlx::query_as(
                "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
                  WHERE con.contype = 'p' AND cls.relname = 'user_identities'
               GROUP BY con.conname",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(key_columns, "id", "the id alone is the key {key}");

            let pairs: Vec<(String, String)> = sqlx::query_as(
                "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
                  WHERE con.contype = 'u' AND cls.relname = 'user_identities'
               GROUP BY con.conname",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(
                pairs,
                vec![(
                    "user_identities_provider_uid_key".to_owned(),
                    "provider,uid".to_owned()
                )],
                "one row per provider account"
            );

            let (foreign, action, target): (String, String, String) = sqlx::query_as(
                "SELECT con.conname, con.confdeltype::text, conf.relname
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_class conf ON conf.oid = con.confrelid
                  WHERE con.contype = 'f'
                    AND cls.relname = 'user_identities'
                    AND con.conname = 'user_identities_user_id_fkey'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(action, "c", "{foreign} deletes with the user");
            assert_eq!(target, "users", "{foreign} points at users");

            let id_default: Option<String> = sqlx::query_scalar(
                "SELECT column_default FROM information_schema.columns
                  WHERE table_schema = 'public' AND table_name = 'user_identities'
                    AND column_name = 'id'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(id_default, None, "the id is minted in Rust (decision 4)");
        }

        /// The login-state migration (WBE-218) creates the table that holds
        /// an `/oauth/authorize` request across a provider round trip: the
        /// issued state as its text primary key (no `uuid` — the token is
        /// what the provider echoes back), the provider it was issued to,
        /// the jsonb parameters, and the expiry with no database default
        /// because its lifetime is the model's to stamp.
        #[tokio::test]
        async fn oauth_login_states_migration_creates_the_state_table() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let columns: Vec<StateColumn> = sqlx::query_as(
                "SELECT column_name, data_type, is_nullable, column_default
                   FROM information_schema.columns
                  WHERE table_schema = 'public' AND table_name = 'oauth_login_states'
               ORDER BY ordinal_position",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            let described = described_of(columns);
            assert_eq!(
                described,
                vec![
                    ("state".to_owned(), "text".to_owned(), "NO".to_owned(), None),
                    (
                        "provider".to_owned(),
                        "text".to_owned(),
                        "NO".to_owned(),
                        None
                    ),
                    (
                        "params".to_owned(),
                        "jsonb".to_owned(),
                        "NO".to_owned(),
                        None
                    ),
                    (
                        "valid_until".to_owned(),
                        "timestamp with time zone".to_owned(),
                        "NO".to_owned(),
                        None
                    ),
                ],
                "the ticket's four columns, none defaulted"
            );

            let (key, key_columns): (String, String) = sqlx::query_as(
                "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
                  WHERE con.contype = 'p' AND cls.relname = 'oauth_login_states'
               GROUP BY con.conname",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                key_columns, "state",
                "the state is the key {key}: it is the only thing that comes back"
            );

            let foreign_keys: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                  WHERE con.contype = 'f' AND cls.relname = 'oauth_login_states'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                foreign_keys, 0,
                "a state is nobody's child: it outlives no table"
            );
        }

        /// The preferred-frontend migration (WBE-122) adds the one column
        /// `PUT /api/v1/pleroma/preferred_frontend` stores: a nullable
        /// `text` `preferred_frontend` on `users` with no default — the
        /// account that never picked a frontend keeps NULL, and the pick
        /// itself is validated against `AVAILABLE_FRONTENDS`, not against
        /// the schema.
        #[tokio::test]
        async fn users_preferred_frontend_migration_adds_the_column() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let (data_type, is_nullable, default): (String, String, Option<String>) =
                sqlx::query_as(
                    "SELECT data_type, is_nullable, column_default
                       FROM information_schema.columns
                      WHERE table_schema = 'public'
                        AND table_name = 'users'
                        AND column_name = 'preferred_frontend'",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(data_type, "text", "the frontend name is stored as text");
            assert_eq!(is_nullable, "YES", "no choice is stored as NULL");
            assert!(default.is_none(), "and nothing defaults to a frontend");
        }

        /// The notification-settings migration (WBE-111) adds the one
        /// column `PUT /api/pleroma/notification_settings` writes: a
        /// `jsonb` `notification_settings` on `users`, `NOT NULL` and
        /// defaulting to both flags `false` — the defaults of Pleroma's
        /// `User.NotificationSetting` embedded schema, so every account
        /// answers the settings before it ever touches them.
        #[tokio::test]
        async fn users_notification_settings_migration_adds_the_column() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let (data_type, is_nullable, default): (String, String, Option<String>) =
                sqlx::query_as(
                    "SELECT data_type, is_nullable, column_default
                       FROM information_schema.columns
                      WHERE table_schema = 'public'
                        AND table_name = 'users'
                        AND column_name = 'notification_settings'",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(data_type, "jsonb", "the flags ride one jsonb blob");
            assert_eq!(is_nullable, "NO", "every account answers them");
            assert_eq!(
                default.as_deref(),
                Some(
                    r#"'{"block_from_strangers": false, "hide_notification_contents": false}'::jsonb"#
                ),
                "both flags default to false: {default:?}"
            );
        }

        /// The suggestions migration (WBE-124) is the pair the suggestion
        /// endpoints read and write: an `is_suggested` flag on `users` that
        /// defaults to `false` (an account joins the pool only when an admin
        /// says so), and `dismissed_suggestions`, whose only columns are the
        /// viewer and the account they dismissed and whose primary key is
        /// that pair — one dismissal per pair, and either account leaving
        /// takes the row with it.
        #[tokio::test]
        async fn suggestions_migration_flags_the_pool_and_keys_the_dismissals() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let (data_type, is_nullable, default): (String, String, Option<String>) =
                sqlx::query_as(
                    "SELECT data_type, is_nullable, column_default
                       FROM information_schema.columns
                      WHERE table_schema = 'public'
                        AND table_name = 'users'
                        AND column_name = 'is_suggested'",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(data_type, "boolean", "the pool is a flag");
            assert_eq!(is_nullable, "NO", "every account answers it one way");
            assert_eq!(
                default.as_deref(),
                Some("false"),
                "and nothing starts in the pool: {default:?}"
            );

            let columns: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT column_name, data_type, is_nullable
                   FROM information_schema.columns
                  WHERE table_schema = 'public' AND table_name = 'dismissed_suggestions'
               ORDER BY ordinal_position",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(
                columns,
                vec![
                    ("user_id".to_owned(), "uuid".to_owned(), "NO".to_owned()),
                    ("target_id".to_owned(), "uuid".to_owned(), "NO".to_owned()),
                ],
                "the dismissal table is the pair and nothing else"
            );

            let (key, key_columns): (String, String) = sqlx::query_as(
                "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
                  WHERE con.contype = 'p' AND cls.relname = 'dismissed_suggestions'
             GROUP BY con.conname",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                key_columns, "target_id,user_id",
                "the pair is the key {key}: one dismissal per viewer and account"
            );

            let foreign_keys: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT con.conname, con.confdeltype::text, conf.relname
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_class conf ON conf.oid = con.confrelid
                  WHERE con.contype = 'f' AND cls.relname = 'dismissed_suggestions'
               ORDER BY con.conname",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(
                foreign_keys,
                vec![
                    (
                        "dismissed_suggestions_target_id_fkey".to_owned(),
                        "c".to_owned(),
                        "users".to_owned()
                    ),
                    (
                        "dismissed_suggestions_user_id_fkey".to_owned(),
                        "c".to_owned(),
                        "users".to_owned()
                    ),
                ],
                "both ends are accounts, and deleting either takes the dismissal"
            );
        }

        /// The password-reset migration (WBE-135) creates the token table
        /// `src/models/password_reset.rs` reads and writes: the minted
        /// `uuid` key, the unique secret text, the cascading account
        /// foreign key, the `false`-defaulted spend flag and the
        /// `now()`-stamped issue time the TTL counts from.
        #[tokio::test]
        async fn password_reset_tokens_migration_creates_the_token_table() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let columns: Vec<StateColumn> = sqlx::query_as(
                "SELECT column_name, data_type, is_nullable, column_default
                   FROM information_schema.columns
                  WHERE table_schema = 'public' AND table_name = 'password_reset_tokens'
               ORDER BY ordinal_position",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(
                described_of(columns),
                vec![
                    ("id".to_owned(), "uuid".to_owned(), "NO".to_owned(), None),
                    ("token".to_owned(), "text".to_owned(), "NO".to_owned(), None),
                    (
                        "user_id".to_owned(),
                        "uuid".to_owned(),
                        "NO".to_owned(),
                        None
                    ),
                    (
                        "used".to_owned(),
                        "boolean".to_owned(),
                        "NO".to_owned(),
                        Some("false".to_owned())
                    ),
                    (
                        "inserted_at".to_owned(),
                        "timestamp with time zone".to_owned(),
                        "NO".to_owned(),
                        Some("now()".to_owned())
                    ),
                ],
                "the ticket's five columns, in the ticket's order"
            );

            let (key, key_columns): (String, String) = sqlx::query_as(
                "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
                  WHERE con.contype = 'p' AND cls.relname = 'password_reset_tokens'
             GROUP BY con.conname",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(key_columns, "id", "the id alone is the key {key}");

            let uniques: Vec<String> = sqlx::query_scalar(
                "SELECT con.conname
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
                  WHERE con.contype = 'u' AND cls.relname = 'password_reset_tokens'
                    AND att.attname = 'token'",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(
                uniques,
                vec!["password_reset_tokens_token_key".to_owned()],
                "the secret text is the unique lookup key"
            );

            let (foreign, action, target): (String, String, String) = sqlx::query_as(
                "SELECT con.conname, con.confdeltype::text, conf.relname
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_class conf ON conf.oid = con.confrelid
                  WHERE con.contype = 'f'
                    AND cls.relname = 'password_reset_tokens'
                    AND con.conname = 'password_reset_tokens_user_id_fkey'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(action, "c", "{foreign} deletes with the account");
            assert_eq!(target, "users", "{foreign} points at users");
        }

        /// The same migration adds `password_reset_pending` to `users`: a
        /// `NOT NULL` `boolean` defaulted to `false`, the login gate
        /// `check_account_ready` reads (the admin force-reset flow that
        /// sets it true is a later ticket).
        #[tokio::test]
        async fn password_reset_tokens_migration_adds_the_pending_flag() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let (pending_type, pending_null, pending_default): (String, String, Option<String>) =
                sqlx::query_as(
                    "SELECT data_type, is_nullable, column_default
                       FROM information_schema.columns
                      WHERE table_schema = 'public'
                        AND table_name = 'users'
                        AND column_name = 'password_reset_pending'",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(pending_type, "boolean", "the login gate is a flag");
            assert_eq!(pending_null, "NO", "every account answers it");
            assert_eq!(
                pending_default.as_deref(),
                Some("false"),
                "and nothing starts pending: {pending_default:?}"
            );
        }

        /// A column of `web_push_subscriptions` or `instance_secrets`, as
        /// `information_schema` describes it: `(name, type, nullability,
        /// default)` — the tuple the two WBE-144 migrations are checked
        /// against.
        type PushColumn = (String, String, String, Option<String>);

        /// The row of `columns` describing the column `name`, panicking
        /// when the table has no such column.
        ///
        /// # Panics
        ///
        /// If `columns` does not carry that column.
        fn push_column<'columns>(
            columns: &'columns [PushColumn],
            name: &str,
        ) -> &'columns PushColumn {
            columns
                .iter()
                .find(|(column, _, _, _)| column == name)
                .unwrap_or_else(|| panic!("the table has no column {name}"))
        }

        /// The `information_schema.columns` description of the named
        /// table's columns, in declaration order.
        async fn push_columns(pool: &sqlx::PgPool, table: &str) -> Vec<PushColumn> {
            sqlx::query_as::<_, PushColumn>(
                "SELECT column_name, data_type, is_nullable, column_default
                   FROM information_schema.columns
                  WHERE table_schema = 'public' AND table_name = $1
               ORDER BY ordinal_position",
            )
            .bind(table)
            .fetch_all(pool)
            .await
            .unwrap()
        }

        /// The types of every `web_push_subscriptions` column, per the
        /// ticket's declaration.
        fn assert_push_column_types(columns: &[PushColumn]) {
            for (name, data_type) in [
                ("id", "uuid"),
                ("user_id", "uuid"),
                ("token_id", "uuid"),
                ("endpoint", "text"),
                ("key_p256dh", "text"),
                ("key_auth", "text"),
                ("alerts", "jsonb"),
                ("policy", "text"),
            ] {
                assert_eq!(
                    push_column(columns, name).1,
                    data_type,
                    "{name} is {data_type}"
                );
            }
        }

        /// The nullability of every `web_push_subscriptions` column: the
        /// ticket's `NOT NULL`s, and the one column it leaves nullable.
        fn assert_push_column_nullability(columns: &[PushColumn]) {
            for (name, nullable) in [
                ("id", "NO"),
                ("user_id", "YES"),
                ("token_id", "NO"),
                ("endpoint", "NO"),
                ("key_p256dh", "NO"),
                ("key_auth", "NO"),
                ("alerts", "NO"),
                ("policy", "NO"),
                ("inserted_at", "NO"),
                ("updated_at", "NO"),
            ] {
                assert_eq!(
                    push_column(columns, name).2,
                    nullable,
                    "{name} is {nullable}"
                );
            }
        }

        /// The defaults of `web_push_subscriptions`: the two the API
        /// leans on, the two timestamps, and the columns the model
        /// always writes itself, which must default nothing.
        fn assert_push_column_defaults(columns: &[PushColumn]) {
            for (name, default) in [
                ("alerts", "'{}'::jsonb"),
                ("policy", "'all'::text"),
                ("inserted_at", "now()"),
                ("updated_at", "now()"),
            ] {
                assert_eq!(
                    push_column(columns, name).3.as_deref(),
                    Some(default),
                    "{name} defaults to {default}"
                );
            }
            for name in [
                "id",
                "user_id",
                "token_id",
                "endpoint",
                "key_p256dh",
                "key_auth",
            ] {
                assert_eq!(
                    push_column(columns, name).3,
                    None,
                    "{name} is written, not defaulted"
                );
            }
        }

        /// The foreign keys of `web_push_subscriptions`: both cascade, so
        /// a departed account or token takes the subscription with it.
        fn assert_push_foreign_keys(keys: &[(String, String, String)]) {
            for (constraint, action, target) in [
                ("web_push_subscriptions_user_id_fkey", "c", "users"),
                ("web_push_subscriptions_token_id_fkey", "c", "oauth_tokens"),
            ] {
                let found = keys
                    .iter()
                    .find(|(name, _, _)| name == constraint)
                    .unwrap_or_else(|| panic!("{constraint} links the table"));
                assert_eq!(found.1, action, "{constraint} deletes as {action}");
                assert_eq!(found.2, target, "{constraint} points at {target}");
            }
        }

        /// The Web Push subscription migration (WBE-144) creates the
        /// table `src/models/web_push_subscription.rs` reads and writes,
        /// keyed the way the API needs it: one subscription per access
        /// token (the unique `token_id` the create-upsert conflicts on),
        /// the owner and the token each cascading the row away with them,
        /// and the ticket's column types, defaults and nullability.
        #[tokio::test]
        async fn web_push_subscriptions_migration_keys_one_row_per_token() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let columns = push_columns(&pool, "web_push_subscriptions").await;
            let names: Vec<&str> = columns
                .iter()
                .map(|(name, _, _, _)| name.as_str())
                .collect();
            assert_eq!(
                names,
                vec![
                    "id",
                    "user_id",
                    "token_id",
                    "endpoint",
                    "key_p256dh",
                    "key_auth",
                    "alerts",
                    "policy",
                    "inserted_at",
                    "updated_at",
                ],
                "the ticket's columns, in the ticket's order"
            );
            assert_push_column_types(&columns);
            assert_push_column_nullability(&columns);
            assert_push_column_defaults(&columns);

            let uniques: Vec<String> = sqlx::query_scalar(
                "SELECT con.conname
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                  WHERE con.contype = 'u' AND cls.relname = 'web_push_subscriptions'",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(
                uniques,
                vec!["web_push_subscriptions_token_id_key".to_owned()],
                "one subscription per access token, the create-upsert's target"
            );

            let keys: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT con.conname, con.confdeltype::text, conf.relname
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_class conf ON conf.oid = con.confrelid
                  WHERE con.contype = 'f' AND cls.relname = 'web_push_subscriptions'",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_push_foreign_keys(&keys);
        }

        /// The instance-secrets migration (WBE-144) creates the tiny
        /// name/value table `src/models/instance_secret.rs` reads: the
        /// `text` primary key its upserts conflict on and the `NOT NULL`
        /// value, with nothing defaulted — every row is a write.
        #[tokio::test]
        async fn instance_secrets_migration_creates_the_secrets_table() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let columns = push_columns(&pool, "instance_secrets").await;
            assert_eq!(
                columns,
                vec![
                    ("name".to_owned(), "text".to_owned(), "NO".to_owned(), None),
                    ("value".to_owned(), "text".to_owned(), "NO".to_owned(), None),
                ],
                "the ticket's two columns, neither defaulted"
            );

            let (key, key_columns): (String, String) = sqlx::query_as(
                "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
                  WHERE con.contype = 'p' AND cls.relname = 'instance_secrets'
               GROUP BY con.conname",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                key_columns, "name",
                "the name alone is the primary key {key}"
            );
        }

        /// The foreign keys the idempotency-keys migration declares: both
        /// cascade, so a departed account or a hard-deleted post retires
        /// the mapping with them.
        fn assert_idempotency_foreign_keys(keys: &[(String, String, String)]) {
            for (constraint, action, target) in [
                ("idempotency_keys_user_id_fkey", "c", "users"),
                ("idempotency_keys_status_id_fkey", "c", "statuses"),
            ] {
                let found = keys
                    .iter()
                    .find(|(name, _, _)| name == constraint)
                    .unwrap_or_else(|| panic!("{constraint} links the table"));
                assert_eq!(found.1, action, "{constraint} deletes as {action}");
                assert_eq!(found.2, target, "{constraint} points at {target}");
            }
        }

        /// The idempotency-keys migration (WBE-87) creates the table
        /// `src/models/idempotency_key.rs` claims and replays through: the
        /// `(user_id, key)` pair as the primary key the create's
        /// `ON CONFLICT DO NOTHING` inserts against, the status the key
        /// created, and both foreign keys cascading — a departed account or
        /// a hard-deleted post retires the mapping with them.
        #[tokio::test]
        async fn idempotency_keys_migration_keys_one_status_per_account_and_key() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let columns: Vec<Described> = sqlx::query_as(
                "SELECT column_name, data_type, is_nullable, column_default
                   FROM information_schema.columns
                  WHERE table_schema = 'public' AND table_name = 'idempotency_keys'
               ORDER BY ordinal_position",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(
                columns,
                vec![
                    (
                        "user_id".to_owned(),
                        "uuid".to_owned(),
                        "NO".to_owned(),
                        None
                    ),
                    ("key".to_owned(), "text".to_owned(), "NO".to_owned(), None),
                    (
                        "status_id".to_owned(),
                        "uuid".to_owned(),
                        "NO".to_owned(),
                        None
                    ),
                    (
                        "inserted_at".to_owned(),
                        "timestamp with time zone".to_owned(),
                        "NO".to_owned(),
                        Some("now()".to_owned())
                    ),
                ],
                "the ticket's four columns, in the ticket's order, only the timestamp defaulted"
            );

            let (key, key_columns): (String, String) = sqlx::query_as(
                "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
                  WHERE con.contype = 'p' AND cls.relname = 'idempotency_keys'
               GROUP BY con.conname",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                key_columns, "key,user_id",
                "the pair is the key {key}: one status per account and key"
            );

            let keys: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT con.conname, con.confdeltype::text, conf.relname
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_class conf ON conf.oid = con.confrelid
                  WHERE con.contype = 'f' AND cls.relname = 'idempotency_keys'",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_idempotency_foreign_keys(&keys);

            let indexes: Vec<String> = sqlx::query_scalar(
                "SELECT indexname FROM pg_indexes
                  WHERE schemaname = 'public' AND tablename = 'idempotency_keys'",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert!(
                indexes.contains(&"idempotency_keys_inserted_at_idx".to_owned()),
                "the sweep's walk is indexed: {indexes:?}"
            );
        }

        /// The email-notifications migration (WBE-220) adds the one column
        /// the HTML unsubscribe page (a later ticket) clears: a `NOT NULL`
        /// `boolean` `email_notifications` on `users` defaulted to `true`,
        /// the flag Pleroma's `User.switch_email_notifications`
        /// (`lib/pleroma/user.ex`) moves — an account is opted in to email
        /// until it says otherwise.
        #[tokio::test]
        async fn users_email_notifications_migration_adds_the_column() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let (data_type, is_nullable, default): (String, String, Option<String>) =
                sqlx::query_as(
                    "SELECT data_type, is_nullable, column_default
                       FROM information_schema.columns
                      WHERE table_schema = 'public'
                        AND table_name = 'users'
                        AND column_name = 'email_notifications'",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(data_type, "boolean", "the mail switch is a flag");
            assert_eq!(is_nullable, "NO", "every account answers it one way");
            assert_eq!(
                default.as_deref(),
                Some("true"),
                "accounts start opted in to email: {default:?}"
            );
        }

        /// The received-activities migration (WBE-163) creates the dedupe
        /// ledger the `federator.incoming` dispatcher claims through: the
        /// activity's `ActivityPub` id as the text primary key the
        /// `INSERT … ON CONFLICT DO NOTHING` claims against, and the
        /// `now()`-stamped `inserted_at` the daily prune ages rows out by
        /// — indexed, so the prune reads rather than scans.
        #[tokio::test]
        async fn received_activities_migration_keys_the_dedupe_ledger() {
            let db = crate::test_db::test_db().await.expect("test database slot");
            let pool = db.pool().clone();
            let columns: Vec<Described> = sqlx::query_as(
                "SELECT column_name, data_type, is_nullable, column_default
                   FROM information_schema.columns
                  WHERE table_schema = 'public' AND table_name = 'received_activities'
               ORDER BY ordinal_position",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(
                columns,
                vec![
                    ("ap_id".to_owned(), "text".to_owned(), "NO".to_owned(), None),
                    (
                        "inserted_at".to_owned(),
                        "timestamp with time zone".to_owned(),
                        "NO".to_owned(),
                        Some("now()".to_owned())
                    ),
                ],
                "the ticket's two columns, in the ticket's order"
            );

            let (primary_key, key_columns): (String, String) = sqlx::query_as(
                "SELECT con.conname, string_agg(att.attname, ',' ORDER BY att.attname)
                   FROM pg_constraint con
                   JOIN pg_class cls ON cls.oid = con.conrelid
                   JOIN pg_attribute att ON att.attrelid = cls.oid AND att.attnum = ANY(con.conkey)
                  WHERE con.contype = 'p' AND cls.relname = 'received_activities'
               GROUP BY con.conname",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                key_columns, "ap_id",
                "the id alone is the key {primary_key}"
            );

            let indexes: Vec<(String, String)> = sqlx::query_as(
                "SELECT indexname, indexdef FROM pg_indexes
                  WHERE schemaname = 'public' AND tablename = 'received_activities'",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert!(
                indexes
                    .iter()
                    .any(|(name, _)| name == "received_activities_inserted_at_idx"),
                "the prune's age scan is indexed: {indexes:?}"
            );
        }
    }
}
