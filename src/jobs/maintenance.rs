//! Periodic maintenance jobs: the housekeeping sweeps that reclaim rows whose
//! useful life has ended.
//!
//! `docs/ARCHITECTURE.md` decision 11 keeps background work in Postgres rows;
//! this module holds the recurring sweeps whose kinds live under the
//! `maintenance.` prefix. Each entry is one pass over one table, enqueued by
//! the [`periodic_jobs`](super::scheduler::periodic_jobs) schedule and run
//! through the [`JobRegistry`](super::JobRegistry) like every other kind.
//!
//! [`handle_notifications`] (job kind [`KIND_NOTIFICATIONS`]) is the first.
//! Dismissal is soft: the dismissal endpoints stamp
//! [`dismissed_at`](crate::models::notification::Notification::dismissed_at)
//! (`migrations/1794145737_notification_dismissed_at.sql`) and every read
//! filters those rows out, so they stay on the books forever once hidden.
//! The sweep deletes the ones dismissed longer than `RETENTION_DAYS` ago,
//! once per day, the window Pleroma-style retention gives them.
//!
//! [`handle_tokens`] (job kind [`KIND_TOKENS`]) is the second: one hourly
//! pass over the four short-lived credential tables, deleting the rows no
//! read accepts any more — `oauth_tokens` expired beyond the week their
//! refresh token buys them, spent or expired `oauth_authorizations` codes,
//! expired `mfa_tokens` challenges, and spent or lapsed
//! `password_reset_tokens` — summing the rows reclaimed into one count.
//! The reference is Pleroma's `ClearOauthTokenWorker`
//! (`lib/pleroma/workers/cron/clear_oauth_token_worker.ex`), which clears
//! expired tokens on a daily cron; here one sweep covers every credential
//! table, on the hour.

use serde_json::Value;
use sqlx::{PgPool, query};
use tracing::info;

use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError},
    state::AppState,
};

/// The job kind of the dismissed-notification sweep, run from the
/// [`periodic_jobs`](super::scheduler::periodic_jobs) registry through the
/// [`JobRegistry`](super::JobRegistry) handler [`handle_notifications`], on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND).
pub const KIND_NOTIFICATIONS: &str = "maintenance.notifications";

/// How long a dismissed notification stays in the table before the sweep
/// reclaims it: thirty days. Reads stopped answering it the moment it was
/// dismissed; this is only how long the row itself lives on.
const RETENTION_DAYS: i32 = 30;

/// The `DELETE` behind [`handle_notifications`]: every notification row
/// whose soft dismissal (`dismissed_at IS NOT NULL`) is older than
/// [`RETENTION_DAYS`] days.
const SWEEP_SQL: &str = "DELETE FROM notifications
        WHERE dismissed_at IS NOT NULL
          AND dismissed_at < now() - make_interval(days => $1)";

/// Runs one sweep and returns the number of rows it deleted.
///
/// Split out from [`handle_notifications`] so the handler is only the
/// logging-and-error shape the [`JobRegistry`](super::JobRegistry) expects.
async fn delete_dismissed(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let deleted = query(SWEEP_SQL).bind(RETENTION_DAYS).execute(pool).await?;
    Ok(deleted.rows_affected())
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_NOTIFICATIONS`]:
/// one sweep of the notifications dismissed longer than `RETENTION_DAYS`
/// days ago, logging how many rows it deleted.
///
/// A failed sweep is a [`JobError::Retry`], and the scheduler enqueues the
/// next period regardless, so the same sweep runs again shortly; the rows are
/// only invisible, never needed, so a pass lost to a database outage costs
/// nothing but the storage.
pub fn handle_notifications(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        match delete_dismissed(&state.db).await {
            Ok(deleted) => {
                info!(deleted, "dismissed notifications deleted");
                Ok(())
            }
            Err(err) => Err(JobError::Retry(format!(
                "dismissed notification sweep failed: {err}"
            ))),
        }
    })
}

/// The job kind of the OAuth/MFA/password-reset token sweep, run from the
/// [`periodic_jobs`](super::scheduler::periodic_jobs) registry through the
/// [`JobRegistry`](super::JobRegistry) handler [`handle_tokens`], on
/// [`QUEUE_BACKGROUND`](super::QUEUE_BACKGROUND).
pub const KIND_TOKENS: &str = "maintenance.tokens";

/// How long an expired `oauth_tokens` row stays in the table before the
/// sweep reclaims it: seven days. The pair's refresh token remains a
/// usable credential for exactly this long past `valid_until` — the token
/// endpoint, not a retention job, decides when a refresh stops working —
/// so deleting earlier would break clients entitled to refresh.
const OAUTH_TOKEN_GRACE_DAYS: i32 = 7;

/// The `DELETE` behind [`delete_long_expired_oauth_tokens`]: every token
/// row whose `valid_until` is more than [`OAUTH_TOKEN_GRACE_DAYS`] days in
/// the past.
const DELETE_OAUTH_TOKENS_SQL: &str = "DELETE FROM oauth_tokens
         WHERE valid_until < now() - make_interval(days => $1)";

/// The `DELETE` behind [`delete_dead_authorizations`]: every code that has
/// been exchanged (`used = true`) or has reached `valid_until`. Codes are
/// one-time credentials with no grace: [`consume_authorization`] refuses
/// them at or past expiry, so such a row can never work again.
///
/// [`consume_authorization`]: crate::models::oauth_token::consume_authorization
const DELETE_AUTHORIZATIONS_SQL: &str = "DELETE FROM oauth_authorizations
         WHERE used = true OR valid_until <= now()";

/// The `DELETE` behind [`delete_expired_mfa_tokens`]: every challenge row
/// at or past `valid_until`, the boundary
/// [`mfa_token::get_valid`](crate::models::mfa_token::get_valid) already
/// refuses.
const DELETE_MFA_TOKENS_SQL: &str = "DELETE FROM mfa_tokens WHERE valid_until <= now()";

/// The `DELETE` behind [`delete_dead_password_reset_tokens`]: every reset
/// token the reset flow has stopped answering — spent, or issued at or
/// beyond the bound interval, the negation of
/// [`password_reset::get_valid`](crate::models::password_reset::get_valid)'s
/// `inserted_at > now() - $1::interval`. Rows inside their window stay.
const DELETE_PASSWORD_RESET_TOKENS_SQL: &str = "DELETE FROM password_reset_tokens
         WHERE used = true OR inserted_at <= now() - $1::interval";

/// Deletes the `oauth_tokens` expired beyond [`OAUTH_TOKEN_GRACE_DAYS`]
/// days, reporting how many rows went.
async fn delete_long_expired_oauth_tokens(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let deleted = query(DELETE_OAUTH_TOKENS_SQL)
        .bind(OAUTH_TOKEN_GRACE_DAYS)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

/// Deletes the `oauth_authorizations` that are spent or expired, reporting
/// how many rows went.
async fn delete_dead_authorizations(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let deleted = query(DELETE_AUTHORIZATIONS_SQL).execute(pool).await?;
    Ok(deleted.rows_affected())
}

/// Deletes the `mfa_tokens` at or past `valid_until`, reporting how many
/// rows went.
async fn delete_expired_mfa_tokens(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let deleted = query(DELETE_MFA_TOKENS_SQL).execute(pool).await?;
    Ok(deleted.rows_affected())
}

/// Deletes the `password_reset_tokens` that are spent or outside the
/// validity window `ttl` — the window [`handle_tokens`] reads from
/// [`Config::password_reset_ttl_secs`](crate::config::Config::password_reset_ttl_secs)
/// and [`password_reset::get_valid`](crate::models::password_reset::get_valid)
/// applies on reads — reporting how many rows went.
async fn delete_dead_password_reset_tokens(
    pool: &PgPool,
    ttl: chrono::Duration,
) -> Result<u64, sqlx::Error> {
    let deleted = query(DELETE_PASSWORD_RESET_TOKENS_SQL)
        .bind(ttl)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

/// Runs one token sweep: the four table deletes, their rows-affected
/// summed into one count.
///
/// Split out from [`handle_tokens`] so the handler is only the
/// logging-and-error shape the [`JobRegistry`](super::JobRegistry) expects.
async fn sweep_tokens(state: &AppState) -> Result<u64, sqlx::Error> {
    let ttl = chrono::Duration::try_seconds(
        i64::try_from(state.config.password_reset_ttl_secs).unwrap_or(i64::MAX),
    )
    .unwrap_or(chrono::Duration::MAX);
    let oauth_tokens = delete_long_expired_oauth_tokens(&state.db).await?;
    let authorizations = delete_dead_authorizations(&state.db).await?;
    let mfa_tokens = delete_expired_mfa_tokens(&state.db).await?;
    let reset_tokens = delete_dead_password_reset_tokens(&state.db, ttl).await?;
    Ok(oauth_tokens + authorizations + mfa_tokens + reset_tokens)
}

/// The [`JobRegistry`](super::JobRegistry) handler for [`KIND_TOKENS`]: one
/// sweep of the dead credential rows, logging how many it deleted.
///
/// The four deletes cover `oauth_tokens` expired beyond the
/// `OAUTH_TOKEN_GRACE_DAYS`-day refresh grace, `oauth_authorizations`
/// spent or expired, `mfa_tokens` expired, and `password_reset_tokens`
/// spent or outside the configured ttl window. Every row they match is one
/// no read path accepts any more, so the sweep only reclaims storage.
///
/// A failed pass is a [`JobError::Retry`], and the scheduler enqueues the
/// next hour regardless, so the same sweep runs again shortly; a pass lost
/// to a database outage costs nothing but the storage.
pub fn handle_tokens(state: AppState, _attempt: AttemptInfo, _args: Value) -> HandlerFuture {
    Box::pin(async move {
        match sweep_tokens(&state).await {
            Ok(deleted) => {
                info!(deleted, "expired tokens deleted");
                Ok(())
            }
            Err(err) => Err(JobError::Retry(format!("token sweep failed: {err}"))),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        http_client::HttpClient,
        jobs::{QUEUE_BACKGROUND, scheduler::periodic_jobs},
        models::{
            oauth_app::{NewOauthApp, OauthApp, create as create_app},
            user::{NewUser, User, insert, local_ap_id},
        },
        streaming::hub::Hub,
        uploads::UploaderBackend,
    };

    /// The period the notification sweep is scheduled on: one day.
    const EVERY: Duration = Duration::from_hours(24);

    /// The period the token sweep is scheduled on: one hour.
    const HOURLY: Duration = Duration::from_secs(3_600);

    /// One day in the seconds the fixture helpers count ages in.
    const DAY: i64 = 86_400;

    /// Test state over `pool` with the fixed test config.
    fn app_state(pool: &PgPool) -> AppState {
        state_with_config(pool, Config::for_tests())
    }

    /// Test state over `pool` with an explicit `config`, for handlers whose
    /// work reads the configuration — the token sweep's reset window.
    fn state_with_config(pool: &PgPool, config: Config) -> AppState {
        let config = Arc::new(config);
        AppState {
            db: pool.clone(),
            hub: Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// Test state over a pool that cannot answer: closed before it ever
    /// connected, which is how a handler sees a database that went away.
    async fn broken_state() -> AppState {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool needs no database");
        pool.close().await;
        app_state(&pool)
    }

    /// Inserts a local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// Inserts a notification for `recipient` caused by `actor` whose
    /// dismissal is stamped `secs` seconds in the past, so a test can put the
    /// table into the state the sweep is about without waiting it out.
    async fn dismissed_ago(pool: &PgPool, recipient: &User, actor: &User, secs: i64) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO notifications (id, user_id, from_account_id, type, dismissed_at)
                    VALUES ($1, $2, $3, 'mention', now() - make_interval(secs => $4))";
        let written = query(sql)
            .bind(id)
            .bind(recipient.id)
            .bind(actor.id)
            .bind(secs)
            .execute(pool)
            .await
            .expect("the fixture notification inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Inserts a notification for `recipient` caused by `actor` that was
    /// never dismissed, returning its id.
    async fn undismissed(pool: &PgPool, recipient: &User, actor: &User) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO notifications (id, user_id, from_account_id, type, dismissed_at)
                    VALUES ($1, $2, $3, 'mention', NULL)";
        let written = query(sql)
            .bind(id)
            .bind(recipient.id)
            .bind(actor.id)
            .execute(pool)
            .await
            .expect("the fixture notification inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Registers an OAuth app named `name`: the issuer the credential rows
    /// point at through their `app_id` foreign keys.
    async fn register_app(pool: &PgPool, name: &str) -> OauthApp {
        create_app(
            pool,
            &NewOauthApp {
                client_name: name.to_owned(),
                redirect_uris: "https://client.example/callback".to_owned(),
                scopes: "read".to_owned(),
                website: None,
                user_id: None,
            },
        )
        .await
        .expect("the app fixture inserts")
    }

    /// Inserts an `oauth_tokens` row for `owner` issued to `client` whose
    /// `valid_until` is `secs` seconds from now (negative for the past), so
    /// a test can set the table into any point of the grace window.
    async fn oauth_token_expiring_in(
        pool: &PgPool,
        owner: &User,
        client: &OauthApp,
        secs: i64,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO oauth_tokens
                       (id, token, refresh_token, user_id, app_id, scopes, valid_until)
                   VALUES ($1, $2, $3, $4, $5, 'read', now() + make_interval(secs => $6))";
        let written = query(sql)
            .bind(id)
            .bind(format!("access-{id}"))
            .bind(format!("refresh-{id}"))
            .bind(owner.id)
            .bind(client.id)
            .bind(secs)
            .execute(pool)
            .await
            .expect("the fixture oauth token inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Inserts an `oauth_authorizations` code for `owner` issued to
    /// `client`, expiring `secs` seconds from now and already exchanged
    /// when `used`, returning its id.
    async fn authorization_row(
        pool: &PgPool,
        owner: &User,
        client: &OauthApp,
        secs: i64,
        used: bool,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO oauth_authorizations
                       (id, token, user_id, app_id, scopes, valid_until, used)
                   VALUES ($1, $2, $3, $4, 'read', now() + make_interval(secs => $5), $6)";
        let written = query(sql)
            .bind(id)
            .bind(format!("code-{id}"))
            .bind(owner.id)
            .bind(client.id)
            .bind(secs)
            .bind(used)
            .execute(pool)
            .await
            .expect("the fixture authorization inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Inserts an `mfa_tokens` challenge for `owner` whose `valid_until` is
    /// `secs` seconds from now, returning its id.
    async fn mfa_token_expiring_in(pool: &PgPool, owner: &User, secs: i64) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO mfa_tokens (id, token, user_id, authorization_id, valid_until)
                   VALUES ($1, $2, $3, NULL, now() + make_interval(secs => $4))";
        let written = query(sql)
            .bind(id)
            .bind(format!("mfa-{id}"))
            .bind(owner.id)
            .bind(secs)
            .execute(pool)
            .await
            .expect("the fixture mfa challenge inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Inserts a `password_reset_tokens` row for `owner` issued `age_secs`
    /// in the past and spent when `used`, returning its id.
    async fn password_reset_row(pool: &PgPool, owner: &User, age_secs: i64, used: bool) -> Uuid {
        let id = Uuid::now_v7();
        let sql = "INSERT INTO password_reset_tokens (id, token, user_id, used, inserted_at)
                   VALUES ($1, $2, $3, $4, now() - make_interval(secs => $5))";
        let written = query(sql)
            .bind(id)
            .bind(format!("reset-{id}"))
            .bind(owner.id)
            .bind(used)
            .bind(age_secs)
            .execute(pool)
            .await
            .expect("the fixture reset token inserts");
        assert_eq!(written.rows_affected(), 1, "one row was written");
        id
    }

    /// Whether `table` — one of the four swept credential tables, named by
    /// the test — still holds the row with primary key `id`. The table
    /// names are test constants, never user input, so the dynamic
    /// statement is asserted safe (the same bypass `src/test_db.rs`
    /// documents).
    async fn row_exists(pool: &PgPool, table: &str, id: Uuid) -> bool {
        let rows = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE id = $1"
        )))
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the row count reads");
        rows == 1
    }

    /// The ids of every notification row left in the table, ordered, so a
    /// test can see exactly which rows survived the sweep.
    async fn surviving(pool: &PgPool) -> Vec<Uuid> {
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM notifications ORDER BY id")
            .fetch_all(pool)
            .await
            .expect("the rows read")
    }

    /// The handler's attempt info: the first of however many the sweep gets,
    /// since its work does not depend on which pass it is.
    const FIRST_ATTEMPT: AttemptInfo = AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };

    #[tokio::test]
    async fn the_handler_deletes_only_the_stale_dismissed_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let stale = dismissed_ago(&pool, &alice, &bob, 31 * DAY).await;
        let recent = dismissed_ago(&pool, &alice, &bob, DAY).await;
        let live = undismissed(&pool, &alice, &bob).await;

        let outcome = handle_notifications(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "a sweep of stale rows succeeds");
        let survivors = surviving(&pool).await;
        assert!(
            !survivors.contains(&stale),
            "the row dismissed beyond the retention window is gone"
        );
        let mut expected = vec![recent, live];
        expected.sort();
        assert_eq!(
            survivors, expected,
            "only the stale row is gone: the recently dismissed one and \
             the undismissed one stay"
        );
    }

    #[tokio::test]
    async fn the_handler_passes_over_a_table_it_need_not_touch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let outcome = handle_notifications(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "nothing to delete is a clean pass");
        assert!(surviving(&pool).await.is_empty(), "and it wrote nothing");
    }

    /// A sweep that cannot reach the database is a retryable failure, so the
    /// queue keeps the pass while the scheduler enqueues the next period: the
    /// stale rows wait for a working sweep instead of being assumed gone.
    #[tokio::test]
    async fn an_unreachable_database_retries_the_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = broken_state().await;

        let outcome = handle_notifications(state, FIRST_ATTEMPT, json!({})).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.starts_with("dismissed notification sweep failed"),
                "the retry states what failed: {detail}"
            ),
            Ok(()) => panic!("a closed pool must not read as a successful sweep"),
            Err(other) => panic!("a closed pool is retryable, not {other:?}"),
        }
        assert!(
            surviving(&pool).await.is_empty(),
            "the healthy pool sees no work the broken one claimed to do"
        );
    }

    /// The sweep is wired into both registries the ticket names: the worker
    /// can run its kind, and the scheduler places it on the maintenance queue
    /// once a day.
    #[test]
    fn the_kind_is_registered_and_scheduled() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_NOTIFICATIONS).is_some(),
            "the worker knows `{KIND_NOTIFICATIONS}`"
        );

        let job = periodic_jobs()
            .into_iter()
            .find(|job| job.kind == KIND_NOTIFICATIONS)
            .expect("the sweep is on the periodic schedule");
        assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
        assert_eq!(job.every, EVERY, "once a day");
    }

    /// The handler the registry binds to the kind is this module's
    /// [`handle_notifications`], not some other sweep: calling the registered
    /// handler deletes the rows.
    #[tokio::test]
    async fn the_registered_handler_is_this_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let stale = dismissed_ago(&pool, &alice, &bob, 31 * DAY).await;
        let registry = crate::job_registry();
        let handler = registry
            .get(KIND_NOTIFICATIONS)
            .expect("the sweep's kind is registered");

        let outcome = handler(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "the registered handler runs");
        let survivors = surviving(&pool).await;
        assert!(
            !survivors.contains(&stale),
            "and it is this module's sweep: the stale row is gone"
        );
        assert!(survivors.is_empty(), "and nothing was left behind");
    }

    /// The token sweep deletes exactly the dead rows of each table and
    /// keeps everything still inside its validity window.
    #[tokio::test]
    async fn the_token_sweep_deletes_only_the_dead_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let client = register_app(&pool, "Client").await;
        let expired_token = oauth_token_expiring_in(&pool, &alice, &client, -8 * DAY).await;
        let graced_token = oauth_token_expiring_in(&pool, &alice, &client, -6 * DAY).await;
        let expired_code = authorization_row(&pool, &alice, &client, -60, false).await;
        let spent_code = authorization_row(&pool, &alice, &client, 600, true).await;
        let live_code = authorization_row(&pool, &alice, &client, 600, false).await;
        let stale_challenge = mfa_token_expiring_in(&pool, &alice, -1).await;
        let fresh_challenge = mfa_token_expiring_in(&pool, &alice, 300).await;
        let spent_reset = password_reset_row(&pool, &alice, 60, true).await;
        let lapsed_reset = password_reset_row(&pool, &alice, DAY + 60, false).await;
        let fresh_reset = password_reset_row(&pool, &alice, 60, false).await;

        let outcome = handle_tokens(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "a sweep over dead and live rows succeeds");
        assert!(
            !row_exists(&pool, "oauth_tokens", expired_token).await,
            "a token expired eight days ago is past its refresh-token week"
        );
        assert!(
            row_exists(&pool, "oauth_tokens", graced_token).await,
            "a token expired six days ago can still be refreshed and stays"
        );
        assert!(
            !row_exists(&pool, "oauth_authorizations", expired_code).await,
            "an expired code goes even unused: it can never be redeemed"
        );
        assert!(
            !row_exists(&pool, "oauth_authorizations", spent_code).await,
            "a spent code goes even fresh: it has been redeemed once already"
        );
        assert!(
            row_exists(&pool, "oauth_authorizations", live_code).await,
            "a fresh unused code stays"
        );
        assert!(
            !row_exists(&pool, "mfa_tokens", stale_challenge).await,
            "an expired challenge goes"
        );
        assert!(
            row_exists(&pool, "mfa_tokens", fresh_challenge).await,
            "a pending challenge stays"
        );
        assert!(
            !row_exists(&pool, "password_reset_tokens", spent_reset).await,
            "a spent reset token goes even fresh"
        );
        assert!(
            !row_exists(&pool, "password_reset_tokens", lapsed_reset).await,
            "a reset token older than the default one-day window goes"
        );
        assert!(
            row_exists(&pool, "password_reset_tokens", fresh_reset).await,
            "a fresh unused reset token stays"
        );
    }

    /// The reset window comes from the configuration, not from a literal:
    /// the same row survives the default one-day ttl and dies under an
    /// overridden one-hour one.
    #[tokio::test]
    async fn the_reset_window_comes_from_the_config() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let aged = password_reset_row(&pool, &alice, 3_700, false).await;

        let default_pass = handle_tokens(app_state(&pool), FIRST_ATTEMPT, json!({})).await;
        assert_eq!(default_pass, Ok(()), "the default-config sweep succeeds");
        assert!(
            row_exists(&pool, "password_reset_tokens", aged).await,
            "an hour-old token sits inside the default one-day window"
        );

        let mut config = Config::for_tests();
        config.password_reset_ttl_secs = 3_600;
        let short_window_pass =
            handle_tokens(state_with_config(&pool, config), FIRST_ATTEMPT, json!({})).await;
        assert_eq!(
            short_window_pass,
            Ok(()),
            "the overridden-ttl sweep succeeds"
        );
        assert!(
            !row_exists(&pool, "password_reset_tokens", aged).await,
            "the same token is outside a one-hour window and goes"
        );
    }

    #[tokio::test]
    async fn the_token_sweep_passes_over_tables_it_need_not_touch() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        let outcome = handle_tokens(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "nothing to delete is a clean pass");
    }

    /// A sweep that cannot reach the database is a retryable failure, so
    /// the queue keeps the pass while the scheduler enqueues the next hour:
    /// the dead rows wait for a working sweep instead of being assumed gone.
    #[tokio::test]
    async fn an_unreachable_database_retries_the_token_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let client = register_app(&pool, "Client").await;
        let expired = oauth_token_expiring_in(&pool, &alice, &client, -8 * DAY).await;
        let state = broken_state().await;

        let outcome = handle_tokens(state, FIRST_ATTEMPT, json!({})).await;

        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.starts_with("token sweep failed"),
                "the retry states what failed: {detail}"
            ),
            Ok(()) => panic!("a closed pool must not read as a successful sweep"),
            Err(other) => panic!("a closed pool is retryable, not {other:?}"),
        }
        assert!(
            row_exists(&pool, "oauth_tokens", expired).await,
            "the healthy pool sees no work the broken one claimed to do"
        );
    }

    /// The token sweep is wired into both registries the ticket names: the
    /// worker can run its kind, and the scheduler places it on the
    /// maintenance queue once an hour.
    #[test]
    fn the_token_kind_is_registered_and_scheduled() {
        let registry = crate::job_registry();
        assert!(
            registry.get(KIND_TOKENS).is_some(),
            "the worker knows `{KIND_TOKENS}`"
        );

        let job = periodic_jobs()
            .into_iter()
            .find(|job| job.kind == KIND_TOKENS)
            .expect("the sweep is on the periodic schedule");
        assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
        assert_eq!(job.every, HOURLY, "once an hour");
    }

    /// The handler the registry binds to the kind is this module's
    /// [`handle_tokens`], not some other sweep: calling the registered
    /// handler deletes the dead rows and keeps the live ones.
    #[tokio::test]
    async fn the_registered_token_handler_is_this_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let client = register_app(&pool, "Client").await;
        let expired = oauth_token_expiring_in(&pool, &alice, &client, -8 * DAY).await;
        let live = authorization_row(&pool, &alice, &client, 600, false).await;
        let registry = crate::job_registry();
        let handler = registry
            .get(KIND_TOKENS)
            .expect("the sweep's kind is registered");

        let outcome = handler(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

        assert_eq!(outcome, Ok(()), "the registered handler runs");
        assert!(
            !row_exists(&pool, "oauth_tokens", expired).await,
            "and it is this module's sweep: the expired token is gone"
        );
        assert!(
            row_exists(&pool, "oauth_authorizations", live).await,
            "and only the dead rows went: the live code stays"
        );
    }
}
