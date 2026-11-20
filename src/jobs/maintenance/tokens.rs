//! The credential sweep: one hourly pass over the five short-lived
//! credential tables — `oauth_tokens`, `oauth_authorizations`, `mfa_tokens`,
//! `password_reset_tokens` and `oauth_login_states` — deleting the rows no
//! read accepts any more and summing what it reclaimed into one count.
//!
//! [`handle_tokens`] is the [`JobRegistry`](crate::jobs::JobRegistry) handler
//! of job kind [`KIND_TOKENS`]; `sweep_tokens` runs the five `DELETE`s,
//! taking the reset-token window from the configuration instead of a literal.

use serde_json::Value;
use sqlx::{PgPool, query};
use tracing::info;

use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError},
    state::AppState,
};

/// The job kind of the OAuth/MFA/password-reset token sweep, run from the
/// [`periodic_jobs`](crate::jobs::scheduler::periodic_jobs) registry through the
/// [`JobRegistry`](crate::jobs::JobRegistry) handler [`handle_tokens`], on
/// [`QUEUE_BACKGROUND`](crate::jobs::QUEUE_BACKGROUND).
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

/// The `DELETE` behind [`delete_expired_login_states`]: every
/// `oauth_login_states` round-trip state at or past its `valid_until` —
/// the negation of the `valid_until > now()` reads
/// [`oauth_login_state::get_valid`] and
/// [`oauth_login_state::get_live`] answer live states by. A finished
/// registration deletes its own row; this is the sweep for the ones its
/// browser never came back for.
///
/// [`oauth_login_state::get_valid`]: crate::models::oauth_login_state::get_valid
/// [`oauth_login_state::get_live`]: crate::models::oauth_login_state::get_live
const DELETE_LOGIN_STATES_SQL: &str = "DELETE FROM oauth_login_states WHERE valid_until <= now()";

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

/// Deletes the `oauth_login_states` past their ten-minute window, reporting
/// how many rows went.
async fn delete_expired_login_states(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let deleted = query(DELETE_LOGIN_STATES_SQL).execute(pool).await?;
    Ok(deleted.rows_affected())
}

/// Runs one token sweep: the five table deletes, their rows-affected
/// summed into one count.
///
/// Split out from [`handle_tokens`] so the handler is only the
/// logging-and-error shape the [`JobRegistry`](crate::jobs::JobRegistry) expects.
async fn sweep_tokens(state: &AppState) -> Result<u64, sqlx::Error> {
    let ttl = chrono::Duration::try_seconds(
        i64::try_from(state.config.password_reset_ttl_secs).unwrap_or(i64::MAX),
    )
    .unwrap_or(chrono::Duration::MAX);
    let oauth_tokens = delete_long_expired_oauth_tokens(&state.db).await?;
    let authorizations = delete_dead_authorizations(&state.db).await?;
    let mfa_tokens = delete_expired_mfa_tokens(&state.db).await?;
    let reset_tokens = delete_dead_password_reset_tokens(&state.db, ttl).await?;
    let login_states = delete_expired_login_states(&state.db).await?;
    Ok(oauth_tokens + authorizations + mfa_tokens + reset_tokens + login_states)
}

/// The [`JobRegistry`](crate::jobs::JobRegistry) handler for [`KIND_TOKENS`]: one
/// sweep of the dead credential rows, logging how many it deleted.
///
/// The five deletes cover `oauth_tokens` expired beyond the
/// `OAUTH_TOKEN_GRACE_DAYS`-day refresh grace, `oauth_authorizations`
/// spent or expired, `mfa_tokens` expired, `password_reset_tokens` spent
/// or outside the configured ttl window, and `oauth_login_states` past
/// their ten minutes. Every row they match is one no read path accepts any
/// more, so the sweep only reclaims storage.
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
mod tests;
