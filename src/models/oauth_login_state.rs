//! The `oauth_login_states` table: what a `/oauth/prepare_request` promise
//! carried across the provider round trip (`docs/ARCHITECTURE.md`
//! decision 9).
//!
//! Pleroma hands the same facts to Ueberauth as a JSON-encoded `state`
//! parameter (`prepare_request` and `callback_params` in
//! `lib/pleroma/web/o_auth/o_auth_controller.ex`); here the state the
//! provider echoes back is only an opaque key and the parameters live
//! server side, one row each, valid for [`LOGIN_STATE_TTL`].
//!
//! [`oauth_login_states.sql`]: ../../../migrations/1791870100_oauth_login_states.sql

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use tracing::debug;

use crate::{
    error::AppResult,
    util::token::{CREDENTIAL_BYTES, random_token},
};

/// How long an issued state survives: ten minutes, the window between
/// `GET /oauth/prepare_request` and the provider's callback.
///
/// Past it a row is unreadable through [`get_valid`] and [`get_live`],
/// writes nothing through [`set_pending`], and goes in the hourly
/// `maintenance.tokens` sweep; nothing here reads a state at all once its
/// expiry has passed.
pub const LOGIN_STATE_TTL: Duration = Duration::from_mins(10);

/// One row of `oauth_login_states`: a state this instance issued to a
/// provider, with the parameters it was issued for.
#[derive(Clone, sqlx::FromRow)]
pub struct OauthLoginState {
    /// The opaque state the provider echoed back. Never in `Debug` output:
    /// holding it is what lets a browser finish someone else's login.
    pub state: String,
    /// Provider name the round trip was prepared for.
    pub provider: String,
    /// The original `/oauth/authorize` parameters, plus the `pending` key an
    /// unlinked callback leaves behind for the registration flow.
    pub params: Value,
    /// Expiry; fresh states live [`LOGIN_STATE_TTL`].
    pub valid_until: DateTime<Utc>,
}

/// [`state`](Self::state) is a capability and renders as `<redacted>`;
/// everything else shows.
impl fmt::Debug for OauthLoginState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OauthLoginState")
            .field("state", &"<redacted>")
            .field("provider", &self.provider)
            .field("params", &self.params)
            .field("valid_until", &self.valid_until)
            .finish()
    }
}

/// [`create`]: the row, with the expiry computed from
/// [`LOGIN_STATE_TTL`] by the database clock.
const INSERT_SQL: &str = "INSERT INTO oauth_login_states (state, provider, params, valid_until)
     VALUES ($1, $2, $3, now() + $4)
  RETURNING *";

/// [`get_valid`] and [`get_live`]: one state, and only while it is still
/// valid; the provider arm is the caller's filter in [`get_valid`].
const GET_SQL: &str = "SELECT * FROM oauth_login_states WHERE state = $1 AND valid_until > now()";

/// [`delete`]: a finished round trip's state has nothing left to carry.
const DELETE_SQL: &str = "DELETE FROM oauth_login_states WHERE state = $1";

/// [`set_pending`]: the provider's account info under the `pending` key,
/// leaving the stored authorize parameters beside it.
const SET_PENDING_SQL: &str = "UPDATE oauth_login_states
     SET params = jsonb_set(params, '{pending}', $2::jsonb, true)
  WHERE state = $1 AND valid_until > now()";

/// Stores `params` under a fresh state for `provider` and returns the row,
/// valid for [`LOGIN_STATE_TTL`] from the database's `now()`.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(pool: &PgPool, provider: &str, params: &Value) -> AppResult<OauthLoginState> {
    let issued = sqlx::query_as::<_, OauthLoginState>(INSERT_SQL)
        .bind(random_token(CREDENTIAL_BYTES))
        .bind(provider)
        .bind(params)
        .bind(LOGIN_STATE_TTL)
        .fetch_one(pool)
        .await?;
    debug!(
        provider,
        valid_until = %issued.valid_until,
        "oauth login state issued"
    );
    Ok(issued)
}

/// The live state `state` names, `None` when no row carries it, it belongs
/// to another provider or its ten minutes are up — the three cases the
/// endpoints refuse alike as "Invalid state".
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_valid(
    pool: &PgPool,
    state: &str,
    provider: &str,
) -> AppResult<Option<OauthLoginState>> {
    let found = sqlx::query_as::<_, OauthLoginState>(GET_SQL)
        .bind(state)
        .fetch_optional(pool)
        .await?
        .filter(|row| row.provider == provider);
    Ok(found)
}

/// The live state `state` names, whichever provider it was issued to.
///
/// The registration pages (`/oauth/registration_details`, `/oauth/register`)
/// know only the key the callback handed the browser; the row's own
/// `provider` column is what they work with, so no provider filter belongs
/// on the lookup. `None` when no live row carries the state.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_live(pool: &PgPool, state: &str) -> AppResult<Option<OauthLoginState>> {
    let found = sqlx::query_as::<_, OauthLoginState>(GET_SQL)
        .bind(state)
        .fetch_optional(pool)
        .await?;
    Ok(found)
}

/// Deletes the state `state` names, answering whether a row went away.
///
/// A finished (or abandoned) registration has nothing left to carry: the
/// same key must not be redeemable twice. An unknown state deletes nothing
/// and answers `false`, which is the caller's success just the same — no
/// live row holds it.
///
/// # Errors
///
/// Any database failure (connection) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, state: &str) -> AppResult<bool> {
    let removed = sqlx::query(DELETE_SQL)
        .bind(state)
        .execute(pool)
        .await
        .map(|outcome| outcome.rows_affected())?;
    Ok(removed > 0)
}

/// Moves `provider`'s account info into the `pending` key of the state's
/// parameters, whether or not one was there, answering whether a live row
/// took the write.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_pending(pool: &PgPool, state: &str, pending: &Value) -> AppResult<bool> {
    let written = sqlx::query(SET_PENDING_SQL)
        .bind(state)
        .bind(pending)
        .execute(pool)
        .await
        .map(|outcome| outcome.rows_affected())?;
    Ok(written > 0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The parameters a test issues a state for.
    fn params() -> Value {
        json!({
            "client_id": "client-id",
            "redirect_uri": "https://client.example/callback",
            "scope": "read write",
            "state": "client-state",
        })
    }

    /// [`get_valid`] answers `None` for a state that was never issued.
    #[tokio::test]
    async fn an_unissued_state_is_absent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            get_valid(&pool, "no-such-state", "github")
                .await
                .expect("the lookup reaches the database")
                .is_none(),
            "a fresh table holds no state"
        );
    }

    /// [`create`] issues the state, stamps the expiry and round-trips the
    /// parameters verbatim.
    #[tokio::test]
    async fn create_stores_the_row_and_get_valid_reads_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let issued = create(&pool, "github", &params())
            .await
            .expect("the insert runs");
        assert_eq!(issued.provider, "github", "the provider round-trips");
        assert_eq!(issued.params, params(), "the parameters round-trip");
        assert_eq!(issued.state.len(), 43, "a 43-character URL-safe token");
        let left = issued.valid_until - Utc::now();
        let window =
            chrono::Duration::from_std(LOGIN_STATE_TTL).expect("ten minutes is a chrono span");
        assert!(
            left <= window && left > window - chrono::Duration::minutes(1),
            "valid_until is the ten-minute window from now, got {left}"
        );

        let found = get_valid(&pool, &issued.state, "github")
            .await
            .expect("the lookup reaches the database")
            .expect("the state is live");
        assert_eq!(found.state, issued.state, "the state round-trips");
        assert_eq!(found.params, params(), "with its parameters");
    }

    /// Two issues are two states: the token is random, so nothing collides.
    #[tokio::test]
    async fn each_issue_is_its_own_state() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let first = create(&pool, "github", &params())
            .await
            .expect("the first issue runs");
        let second = create(&pool, "github", &params())
            .await
            .expect("the second issue runs");
        assert_ne!(first.state, second.state, "the states differ");
    }

    /// A state is only readable for the provider it was issued to: that is
    /// the endpoint's "Invalid state" refusal.
    #[tokio::test]
    async fn a_state_is_only_valid_for_its_provider() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let issued = create(&pool, "github", &params())
            .await
            .expect("the insert runs");
        assert!(
            get_valid(&pool, &issued.state, "keycloak")
                .await
                .expect("the lookup runs")
                .is_none(),
            "another provider cannot redeem it"
        );
    }

    /// An expired row is unreadable, and a live one beside it is not.
    #[tokio::test]
    async fn an_expired_state_is_unreadable() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let expired = create(&pool, "github", &params())
            .await
            .expect("the first insert runs");
        let live = create(&pool, "github", &params())
            .await
            .expect("the second insert runs");
        let rewound = sqlx::query("UPDATE oauth_login_states SET valid_until = now() - interval '1 second' WHERE state = $1")
            .bind(&expired.state)
            .execute(&pool)
            .await
            .expect("the rewind runs");
        assert_eq!(rewound.rows_affected(), 1, "the rewind hit one row");

        assert!(
            get_valid(&pool, &expired.state, "github")
                .await
                .expect("the lookup runs")
                .is_none(),
            "the expired state is unreadable"
        );
        assert!(
            get_valid(&pool, &live.state, "github")
                .await
                .expect("the lookup runs")
                .is_some(),
            "the live state beside it is still readable"
        );
    }

    /// [`get_live`] reads a live row without any provider filter — the
    /// registration pages look the state up by key alone — and refuses an
    /// expired one exactly like [`get_valid`] does.
    #[tokio::test]
    async fn get_live_reads_any_provider_and_refuses_the_expired() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let issued = create(&pool, "keycloak", &params())
            .await
            .expect("the insert runs");
        let found = get_live(&pool, &issued.state)
            .await
            .expect("the lookup reaches the database")
            .expect("the state is live");
        assert_eq!(
            found.provider, "keycloak",
            "the provider comes from the row"
        );
        assert_eq!(found.params, params(), "and so do its parameters");

        let rewound = sqlx::query("UPDATE oauth_login_states SET valid_until = now() - interval '1 second' WHERE state = $1")
            .bind(&issued.state)
            .execute(&pool)
            .await
            .expect("the rewind runs");
        assert_eq!(rewound.rows_affected(), 1, "the rewind hit the row");
        assert!(
            get_live(&pool, &issued.state)
                .await
                .expect("the lookup runs")
                .is_none(),
            "the expired state is unreadable"
        );
        assert!(
            get_live(&pool, "never-issued")
                .await
                .expect("the lookup runs")
                .is_none(),
            "and so is one that was never issued"
        );
    }

    /// [`delete`] takes the row, answers whether one went away, and leaves a
    /// second call (and an unknown state) with nothing to remove.
    #[tokio::test]
    async fn delete_removes_the_finished_state_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let issued = create(&pool, "github", &params())
            .await
            .expect("the insert runs");
        assert!(
            !delete(&pool, "never-issued")
                .await
                .expect("the delete runs"),
            "an unknown state removes nothing"
        );
        assert!(
            delete(&pool, &issued.state).await.expect("the delete runs"),
            "the live row goes"
        );
        assert!(
            !delete(&pool, &issued.state).await.expect("the delete runs"),
            "and the second call has nothing left"
        );
        assert!(
            get_live(&pool, &issued.state)
                .await
                .expect("the lookup runs")
                .is_none(),
            "the state is gone for good"
        );
    }

    /// [`set_pending`] adds the `pending` key and leaves the stored
    /// parameters beside it; it answers whether a live row took the write.
    #[tokio::test]
    async fn set_pending_merges_into_the_stored_params() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let issued = create(&pool, "github", &params())
            .await
            .expect("the insert runs");
        let pending = json!({"provider": "github", "uid": "4242", "email": null});
        assert!(
            set_pending(&pool, &issued.state, &pending)
                .await
                .expect("the write runs"),
            "the live row took the write"
        );

        let found = get_valid(&pool, &issued.state, "github")
            .await
            .expect("the lookup runs")
            .expect("the state is still live");
        assert_eq!(
            found.params.get("pending"),
            Some(&pending),
            "the provider info is under `pending`: {found:?}"
        );
        assert_eq!(
            found.params.get("client_id"),
            Some(&json!("client-id")),
            "the authorize parameters are untouched"
        );

        assert!(
            !set_pending(&pool, "never-issued", &pending)
                .await
                .expect("the write reaches the database"),
            "a state that is not there takes no write"
        );
    }

    /// An expired state takes no pending write either.
    #[tokio::test]
    async fn set_pending_skips_an_expired_state() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let issued = create(&pool, "github", &params())
            .await
            .expect("the insert runs");
        let rewound = sqlx::query("UPDATE oauth_login_states SET valid_until = now() - interval '1 second' WHERE state = $1")
            .bind(&issued.state)
            .execute(&pool)
            .await
            .expect("the rewind runs");
        assert_eq!(rewound.rows_affected(), 1, "the rewind hit the row");
        assert!(
            !set_pending(&pool, &issued.state, &json!({"uid": "4242"}))
                .await
                .expect("the write reaches the database"),
            "the expired row refused it"
        );
    }

    /// The debug impl hides the state: it is the only secret on the row.
    #[test]
    fn debug_redacts_the_state() {
        let row = OauthLoginState {
            state: "a-very-secret-state".to_owned(),
            provider: "github".to_owned(),
            params: params(),
            valid_until: Utc::now(),
        };
        let debug = format!("{row:?}");
        assert!(
            !debug.contains("a-very-secret-state"),
            "the state must not leak: {debug}"
        );
        assert!(
            debug.contains(r#"state: "<redacted>""#),
            "the marker names the field: {debug}"
        );
        assert!(debug.contains("github"), "the provider shows: {debug}");
    }
}
