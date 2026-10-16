//! wired-be: an API-only fediverse backend built on Axum.

pub mod api;
pub mod auth;
pub mod config;
pub mod entities;
pub mod error;
pub mod extract;
pub mod federation;
pub mod http_client;
pub mod middleware;
pub mod models;
pub mod oauth;
pub mod pagination;
pub mod services;
pub mod state;
pub mod uploads;
pub mod util;

use std::sync::Arc;

use axum::{
    Router,
    http::{StatusCode, Uri},
    routing::get,
};
use sqlx::PgPool;

use crate::{api::media_files, error::AppError, state::AppState};

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

/// Builds the application router, wrapped in the [`middleware`] stack.
///
/// The [`api`] routers are merged under their prefixes; the seven that
/// serve addresses outside `/api` fixed by their protocols, by stored URLs
/// or by the clients that call them — `/media` ([`media_files`]), `/emoji`
/// ([`api::emoji_packs`]), `/users/{nickname}` and `/internal/fetch`
/// ([`api::activitypub`]), `/.well-known/webfinger`
/// ([`api::well_known`]), `/.well-known/nodeinfo` and
/// `/nodeinfo/{version}` ([`api::nodeinfo`]), `/oauth` ([`api::oauth`]) and
/// the `MongooseIM` auth endpoints plus `/manifest.json` ([`api::misc`]) —
/// are merged at the top level. Paths no route matches get the standard
/// JSON error response (decision 8).
///
/// [`api::emoji_packs`]: crate::api::emoji_packs::files_router
pub fn app(state: AppState) -> Router {
    // The `/api` routers need the configuration itself (the upload route's
    // body limit is sized from it), so the `Arc` is cloned and not the config.
    let config = Arc::clone(&state.config);
    middleware::apply(
        Router::new()
            .route("/health", get(health))
            .merge(api::router(&config))
            .merge(api::activitypub::router())
            .merge(api::oauth::router())
            .merge(api::well_known::router())
            .merge(api::nodeinfo::router())
            .merge(media_files::router())
            .merge(api::emoji_packs::files_router())
            .merge(api::misc::router())
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

    use crate::{config::Config, http_client::HttpClient, uploads::UploaderBackend};

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
            db,
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
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

    /// Migration tests, run against the throwaway database created by `#[sqlx::test]`.
    mod migrations {
        use sqlx::PgPool;

        /// The base extensions migration installs `citext`, `pg_trgm` and `pgcrypto`.
        #[sqlx::test(migrator = "crate::MIGRATOR")]
        async fn extensions_migration_installs_all_three_extensions(pool: PgPool) {
            let row: (i64,) = sqlx::query_as(
                "SELECT count(*) FROM pg_extension WHERE extname IN ('citext','pg_trgm','pgcrypto')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(row.0, 3, "all three extensions should be installed");
        }

        /// The statuses migration creates the six indexes the timeline and
        /// thread queries need — the two newest-first ones partial on
        /// `deleted_at IS NULL`, so tombstones drop out of them for free —
        /// and gives `media_attachments.status_id` its foreign key into the
        /// new table.
        #[sqlx::test(migrator = "crate::MIGRATOR")]
        async fn statuses_migration_indexes_statuses_and_links_media(pool: PgPool) {
            let rows: Vec<(String, String)> = sqlx::query_as(
                "SELECT indexname, indexdef FROM pg_indexes
                  WHERE schemaname = 'public' AND tablename = 'statuses'",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            let names: Vec<&str> = rows.iter().map(|(name, _)| name.as_str()).collect();
            for expected in [
                "statuses_pkey",
                "statuses_uri_key",
                "statuses_user_id_idx",
                "statuses_in_reply_to_id_idx",
                "statuses_conversation_id_idx",
                "statuses_reblog_of_id_idx",
                "statuses_visibility_idx",
                "statuses_quote_of_id_idx",
            ] {
                assert!(names.contains(&expected), "{expected} indexes statuses");
            }
            for partial in ["statuses_user_id_idx", "statuses_visibility_idx"] {
                let definition = rows
                    .iter()
                    .find(|(name, _)| name == partial)
                    .map(|(_, definition)| definition.as_str())
                    .unwrap();
                assert!(
                    definition.contains("deleted_at IS NULL"),
                    "{partial} is partial on live rows only"
                );
            }

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
        #[sqlx::test(migrator = "crate::MIGRATOR")]
        async fn chats_migration_links_both_sides_of_a_conversation(pool: PgPool) {
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
            for (table, constraint, action, target) in [
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
            ] {
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

        /// The unique keys and indexes the chats migration adds: one chat per
        /// participant and account, one reference per chat and message, a
        /// reserved `ap_id`, and the newest-first and unread-only indexes the
        /// list queries read.
        #[sqlx::test(migrator = "crate::MIGRATOR")]
        async fn chats_migration_keys_and_indexes_the_chat_tables(pool: PgPool) {
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
            for (table, constraint) in [
                ("chats", "chats_user_id_recipient_id_key"),
                (
                    "chat_message_references",
                    "chat_message_references_chat_id_message_id_key",
                ),
                ("chat_messages", "chat_messages_ap_id_key"),
            ] {
                assert!(
                    uniques
                        .iter()
                        .any(|(relname, conname)| relname == table && conname == constraint),
                    "{constraint} keeps {table} free of duplicates"
                );
            }

            let indexes: Vec<(String, String)> = sqlx::query_as(
                "SELECT indexname, indexdef FROM pg_indexes
                  WHERE schemaname = 'public'
                    AND tablename IN ('chats', 'chat_messages', 'chat_message_references')",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            for name in [
                "chats_user_id_updated_at_idx",
                "chats_recipient_id_idx",
                "chat_messages_sender_id_idx",
                "chat_message_references_chat_id_idx",
                "chat_message_references_chat_id_unread_idx",
                "chat_message_references_message_id_idx",
            ] {
                assert!(
                    indexes.iter().any(|(indexname, _)| indexname == name),
                    "{name} serves a chat query"
                );
            }
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
        #[sqlx::test(migrator = "crate::MIGRATOR")]
        async fn chats_migration_defaults_match_the_model(pool: PgPool) {
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

            let columns: Vec<SchemaColumn> = sqlx::query_as(
                "SELECT table_name, column_name, column_default, is_nullable
                   FROM information_schema.columns
                  WHERE table_schema = 'public'
                    AND table_name IN ('chats', 'chat_messages', 'chat_message_references')",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            let one = |table: &str, column: &str| -> &SchemaColumn {
                columns
                    .iter()
                    .find(|row| row.table_name == table && row.column_name == column)
                    .unwrap_or_else(|| panic!("{table}.{column} exists"))
            };

            for (table, column, default) in [
                ("chats", "pinned", "false"),
                ("chat_messages", "content", "''::text"),
                ("chat_messages", "raw_text", "''::text"),
                ("chat_message_references", "unread", "true"),
            ] {
                let row = one(table, column);
                assert_eq!(
                    row.column_default.as_deref(),
                    Some(default),
                    "{table}.{column} defaults to {default}"
                );
                assert_eq!(row.is_nullable, "NO", "{table}.{column} is NOT NULL");
            }
            for (table, column) in [
                ("chats", "user_id"),
                ("chats", "recipient_id"),
                ("chat_messages", "sender_id"),
                ("chat_message_references", "chat_id"),
                ("chat_message_references", "message_id"),
            ] {
                assert_eq!(
                    one(table, column).is_nullable,
                    "NO",
                    "{table}.{column} cannot be null"
                );
            }
            assert!(
                one("chat_messages", "deleted_at").column_default.is_none()
                    && one("chat_messages", "deleted_at").is_nullable == "YES",
                "a live message has no deleted_at until it is withdrawn"
            );
        }

        /// The instance-settings migration creates the tiny key/value
        /// table `src/models/instance_setting.rs` reads: the `text`
        /// primary key its upsert conflicts on, the jsonb value and the
        /// `now()`-defaulted `updated_at`.
        #[sqlx::test(migrator = "crate::MIGRATOR")]
        async fn instance_settings_migration_creates_the_settings_table(pool: PgPool) {
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
        #[sqlx::test(migrator = "crate::MIGRATOR")]
        async fn instance_rules_migration_creates_the_rules_table(pool: PgPool) {
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
            let one = |name: &str| -> &RuleColumn {
                columns
                    .iter()
                    .find(|column| column.column_name == name)
                    .unwrap_or_else(|| panic!("instance_rules.{name} exists"))
            };
            for (name, data_type) in [
                ("id", "uuid"),
                ("text", "text"),
                ("hint", "text"),
                ("priority", "integer"),
                ("inserted_at", "timestamp with time zone"),
                ("updated_at", "timestamp with time zone"),
            ] {
                assert_eq!(
                    one(name).data_type,
                    data_type,
                    "instance_rules.{name} is {data_type}"
                );
                assert_eq!(
                    one(name).is_nullable,
                    "NO",
                    "instance_rules.{name} is NOT NULL"
                );
            }
            assert_eq!(
                one("id").column_default,
                None,
                "the id is minted in Rust (decision 4)"
            );
            assert_eq!(
                one("text").column_default,
                None,
                "text has no default: the rule is required"
            );
            assert_eq!(
                one("hint").column_default.as_deref(),
                Some("''::text"),
                "the hint defaults to empty"
            );
            assert_eq!(
                one("priority").column_default.as_deref(),
                Some("0"),
                "the priority defaults to the middle of the list"
            );
            for name in ["inserted_at", "updated_at"] {
                assert_eq!(
                    one(name).column_default.as_deref(),
                    Some("now()"),
                    "{name} defaults to now()"
                );
            }

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
    }
}
