//! The `password_reset_tokens` table: the single-use tokens that
//! `POST /auth/password` mints and the `/api/v1/pleroma/password_reset`
//! endpoints consume (see [`crate::api::password_reset`]).
//!
//! The reference is Pleroma's `Pleroma.PasswordResetToken`
//! (`lib/pleroma/password_reset_token.ex`): a token row carries the random
//! secret text ([`random_token`] over [`CREDENTIAL_BYTES`], the same 43
//! URL-safe characters Pleroma's `:crypto.strong_rand_bytes(32) |>
//! Base.url_encode64()` mints), the account it can reset, a `used` flag and
//! its issue time. A token is valid while it is unused and its
//! [`inserted_at`](PasswordResetToken::inserted_at) is within
//! [`PASSWORD_RESET_TTL_SECS_VAR`](crate::config::PASSWORD_RESET_TTL_SECS_VAR)
//! ([`password_reset_ttl_secs`](crate::config::Config::password_reset_ttl_secs))
//! of now; [`get_valid`] is exactly that gate, and completing a reset spends
//! the token with [`mark_used`] and deletes the account's other tokens with
//! [`delete_others`].
//!
//! `token` is secret material until the email is delivered: the manual
//! [`Debug`](PasswordResetToken) impl below redacts it, so it can never
//! reach a log line.

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use sqlx::{Executor, PgPool, Postgres};
use uuid::Uuid;

use crate::{
    error::AppResult,
    util::{
        id::new_id,
        token::{CREDENTIAL_BYTES, random_token},
    },
};

/// [`create_for_user`]: the minted id and random secret against the two
/// columns an insert supplies; `used` and `inserted_at` come from the
/// column defaults and `RETURNING *` brings the row back stamped.
const INSERT_SQL: &str =
    "INSERT INTO password_reset_tokens (id, token, user_id) VALUES ($1, $2, $3) RETURNING *";

/// [`get_valid`]: the one lookup the reset flow runs — the exact,
/// case-sensitive token text is the unique key, and only an unused row
/// whose issue time is inside the bound interval is a hit.
const SELECT_VALID_SQL: &str = "SELECT * FROM password_reset_tokens \
     WHERE token = $1 AND NOT used AND inserted_at > now() - $2::interval";

/// [`mark_used`]: spend the token in the row, only while it is still
/// unspent, so two concurrent spenders cannot both win.
const MARK_USED_SQL: &str =
    "UPDATE password_reset_tokens SET used = true WHERE id = $1 AND NOT used";

/// [`delete_others`]: every token of the account except the one being
/// spent, so a completed reset leaves exactly one (now-used) row behind.
const DELETE_OTHERS_SQL: &str = "DELETE FROM password_reset_tokens WHERE user_id = $1 AND id <> $2";

/// One row of `password_reset_tokens`: a token emailed to one account.
#[derive(Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PasswordResetToken {
    /// `UUIDv7` primary key, minted by [`create_for_user`] (decision 4).
    pub id: Uuid,
    /// The secret text carried in the reset link; unique, and redacted
    /// from `Debug` output.
    pub token: String,
    /// The account this token can reset (foreign key, deletes with it).
    pub user_id: Uuid,
    /// Whether the token has been spent; a used token never resets again.
    pub used: bool,
    /// Issue time; [`get_valid`]'s expiry counts from here.
    pub inserted_at: DateTime<Utc>,
}

/// [`token`](Self::token) is secret material and renders as `<redacted>`;
/// everything else shows.
impl fmt::Debug for PasswordResetToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasswordResetToken")
            .field("id", &self.id)
            .field("token", &"<redacted>")
            .field("user_id", &self.user_id)
            .field("used", &self.used)
            .field("inserted_at", &self.inserted_at)
            .finish()
    }
}

impl PasswordResetToken {
    /// Whether this token's issue time is older than `ttl` when the clock
    /// reads `now` — the expiry half of [`get_valid`], spelled on the row
    /// so it is testable without waiting out a real window. A token whose
    /// age equals `ttl` exactly is still valid; Pleroma's `expired?` uses
    /// the same strict comparison.
    #[must_use]
    pub fn is_expired(&self, now: DateTime<Utc>, ttl: Duration) -> bool {
        let Ok(span) = chrono::Duration::from_std(ttl) else {
            // An unrepresentable span is a window no real token can age
            // out of: treat it as "never expires" rather than refusing
            // every reset.
            return false;
        };
        now - self.inserted_at > span
    }
}

/// Mints a reset token for the account `user_id` and returns the stored
/// row. Nothing is sent here — the endpoint hands the token's link to the
/// mailer (see [`crate::mailer::templates::password_reset`]).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError). The random token
/// colliding with a stored one is a database failure too — vanishingly
/// unlikely for 32 CSPRNG bytes, and the account can request again.
pub async fn create_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<PasswordResetToken> {
    let row = sqlx::query_as::<_, PasswordResetToken>(INSERT_SQL)
        .bind(new_id())
        .bind(random_token(CREDENTIAL_BYTES))
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Fetches the still-valid (unused, unexpired) token whose text is exactly
/// `token`, `None` when no row matches.
///
/// Unknown, spent or lapsed tokens are deliberately not told apart, the way
/// Pleroma's `reset` renders one `invalid_token.html` for all three.
///
/// The window is applied in SQL against the database clock, so a token
/// minted on one process and checked on another ages on one clock.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_valid(
    pool: &PgPool,
    token: &str,
    ttl: Duration,
) -> AppResult<Option<PasswordResetToken>> {
    let validity = chrono::Duration::from_std(ttl)
        // Same rule as [`PasswordResetToken::is_expired`]: a span too
        // large for the interval type is a window that never closes.
        .unwrap_or(chrono::Duration::MAX);
    let row = sqlx::query_as::<_, PasswordResetToken>(SELECT_VALID_SQL)
        .bind(token)
        .bind(validity)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Marks the token `id` used: the spend that closes a completed reset.
///
/// The update is conditional (`AND NOT used`), so of two concurrent spends
/// exactly one reports `true`; the loser reports `false` and the caller
/// must treat the token as invalid.
///
/// Runs on the caller's executor (or any open transaction), so the reset
/// endpoint can land the spend with the password write and the
/// revocations.
///
/// # Errors
///
/// Any database failure (connection, constraint) arrives as
/// [`AppError::Database`](crate::error::AppError). A token that is already
/// spent, or that no longer exists, is not an error: it answers `false`.
pub async fn mark_used<'e, E>(executor: E, id: Uuid) -> AppResult<bool>
where
    E: Executor<'e, Database = Postgres>,
{
    let outcome = sqlx::query(MARK_USED_SQL)
        .bind(id)
        .execute(executor)
        .await?;
    Ok(outcome.rows_affected() > 0)
}

/// Deletes every reset token of the account `user_id` except the row
/// `keep_id`, reporting how many went.
///
/// A completed reset runs this beside [`mark_used`] so the account is left
/// with exactly one spent token: the other links in the wild (or in an old
/// email) stop working at once.
///
/// Runs on the caller's executor (or any open transaction), like
/// [`mark_used`].
///
/// # Errors
///
/// Any database failure (connection) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_others<'e, E>(executor: E, user_id: Uuid, keep_id: Uuid) -> AppResult<u64>
where
    E: Executor<'e, Database = Postgres>,
{
    let deleted = sqlx::query(DELETE_OTHERS_SQL)
        .bind(user_id)
        .bind(keep_id)
        .execute(executor)
        .await?;
    Ok(deleted.rows_affected())
}

#[cfg(test)]
mod tests {
    use chrono::Duration as ChronoDuration;
    use sqlx::Connection as _;

    use super::*;

    /// The window every expiry test passes: one hour.
    const HOUR: Duration = Duration::from_hours(1);

    /// A [`PasswordResetToken`] row assembled by hand for the pure
    /// [`is_expired`] checks: only `inserted_at` matters there.
    fn token_aged(seconds: i64) -> PasswordResetToken {
        PasswordResetToken {
            id: Uuid::now_v7(),
            token: "secret-text".to_owned(),
            user_id: Uuid::now_v7(),
            used: false,
            inserted_at: Utc::now() - ChronoDuration::seconds(seconds),
        }
    }

    /// A fresh slot row for the given account, minted through the model.
    async fn mint(pool: &PgPool, user_id: Uuid) -> PasswordResetToken {
        create_for_user(pool, user_id)
            .await
            .expect("the insert runs")
    }

    /// The stored row behind `id`, re-read to prove what a write changed.
    async fn stored(pool: &PgPool, id: Uuid) -> PasswordResetToken {
        let row = sqlx::query_as::<_, PasswordResetToken>(
            "SELECT * FROM password_reset_tokens WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .expect("the row reads back");
        row.expect("the token exists")
    }

    /// Backdates the token `id`'s `inserted_at` by `seconds`, the way a
    /// token minted before the window would have aged.
    async fn age_token(pool: &PgPool, id: Uuid, seconds: i64) {
        let aged = sqlx::query(
            "UPDATE password_reset_tokens SET inserted_at = now() - $2::interval WHERE id = $1",
        )
        .bind(id)
        .bind(ChronoDuration::seconds(seconds))
        .execute(pool)
        .await
        .expect("the ageing update runs");
        assert_eq!(aged.rows_affected(), 1, "the update hit the token row");
    }

    /// A local user id for the token rows to point at (the table has a
    /// foreign key into `users`).
    async fn account(pool: &PgPool, nickname: &str) -> Uuid {
        let config = crate::config::Config::for_tests();
        let ap_id = crate::models::user::local_ap_id(&config.base_url, nickname);
        let new = crate::models::user::NewUser::local(nickname, &ap_id);
        crate::models::user::insert(pool, &new)
            .await
            .expect("the account fixture inserts")
            .id
    }

    #[test]
    fn is_expired_weighs_the_age_against_the_window() {
        let now = Utc::now();
        let make = |age_secs| PasswordResetToken {
            id: Uuid::now_v7(),
            token: "s".to_owned(),
            user_id: Uuid::now_v7(),
            used: false,
            inserted_at: now - ChronoDuration::seconds(age_secs),
        };
        assert!(
            !make(0).is_expired(now, HOUR),
            "a brand-new token is not expired"
        );
        assert!(
            make(3_601).is_expired(now, HOUR),
            "one second past the hour is expired"
        );
        assert!(
            !make(3_600).is_expired(now, HOUR),
            "exactly at the boundary is not yet expired (age > ttl)"
        );
        assert!(
            !make(100_000).is_expired(now, Duration::MAX),
            "an unrepresentable span never expires a token"
        );
    }

    #[test]
    fn debug_redacts_the_token() {
        let debug = format!("{:?}", token_aged(0));
        assert!(
            !debug.contains("secret-text"),
            "the token must not leak into Debug output"
        );
        assert!(
            debug.contains(r#"token: "<redacted>""#),
            "the redaction marker names the field"
        );
    }

    #[tokio::test]
    async fn create_mints_a_fresh_unused_token() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user_id = account(&pool, "minter").await;
        let first = mint(&pool, user_id).await;
        assert_ne!(first.id, Uuid::nil(), "the id is minted");
        assert_eq!(
            first.token.len(),
            43,
            "32 bytes are 43 base64url characters"
        );
        assert_eq!(first.user_id, user_id, "the token names its account");
        assert!(!first.used, "a fresh token is unused");
        assert!(
            first
                .token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "the token is URL-safe verbatim"
        );
        let second = mint(&pool, user_id).await;
        assert_ne!(first.token, second.token, "two mints do not collide");
    }

    #[tokio::test]
    async fn get_valid_answers_only_the_named_unused_token() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user_id = account(&pool, "validity").await;
        let token = mint(&pool, user_id).await;
        let fetched = get_valid(&pool, &token.token, HOUR)
            .await
            .expect("the lookup runs");
        assert_eq!(fetched.as_ref(), Some(&token), "a fresh token is valid");
        assert_eq!(
            get_valid(&pool, "never-minted-text", HOUR)
                .await
                .expect("the lookup runs"),
            None,
            "an unknown token finds nothing"
        );
        let other_account = mint(&pool, account(&pool, "other").await).await;
        assert_ne!(other_account.token, token.token, "distinct secrets");
    }

    #[tokio::test]
    async fn get_valid_refuses_a_spent_and_a_lapsed_token() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user_id = account(&pool, "spent").await;
        let spent = mint(&pool, user_id).await;
        assert!(
            mark_used(&pool, spent.id).await.expect("the spend runs"),
            "the first spend wins"
        );
        assert_eq!(
            get_valid(&pool, &spent.token, HOUR)
                .await
                .expect("the lookup runs"),
            None,
            "a used token is not valid"
        );
        assert!(stored(&pool, spent.id).await.used, "and the flag says so");

        let lapsed = mint(&pool, user_id).await;
        age_token(&pool, lapsed.id, 7_200).await;
        assert_eq!(
            get_valid(&pool, &lapsed.token, HOUR)
                .await
                .expect("the lookup runs"),
            None,
            "a token two hours old is outside a one-hour window"
        );
        assert_eq!(
            get_valid(&pool, &lapsed.token, Duration::from_hours(24))
                .await
                .expect("the lookup runs")
                .as_ref(),
            Some(&stored(&pool, lapsed.id).await),
            "the same token is still valid inside a 24-hour window"
        );
    }

    #[tokio::test]
    async fn mark_used_flips_the_flag_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user_id = account(&pool, "marker").await;
        let token = mint(&pool, user_id).await;
        assert!(
            mark_used(&pool, token.id).await.expect("the spend runs"),
            "the first spend lands"
        );
        assert!(stored(&pool, token.id).await.used, "the row is spent");
    }

    #[tokio::test]
    async fn mark_used_twice_answers_false_the_second_time() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user_id = account(&pool, "twice").await;
        let token = mint(&pool, user_id).await;
        assert!(
            mark_used(&pool, token.id).await.expect("the first spend"),
            "the first spend wins"
        );
        assert!(
            !mark_used(&pool, token.id).await.expect("the second spend"),
            "the second spend of the same token loses"
        );
        assert!(stored(&pool, token.id).await.used, "the row stays spent");
    }

    #[tokio::test]
    async fn mark_used_of_a_vanished_token_answers_false() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            !mark_used(&pool, Uuid::now_v7())
                .await
                .expect("a missing row is not a database failure"),
            "no token carries this id"
        );
    }

    #[tokio::test]
    async fn mark_used_runs_on_an_open_transaction() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user_id = account(&pool, "txn").await;
        let token = mint(&pool, user_id).await;
        let mut conn = pool.acquire().await.expect("a connection");
        let mut tx = conn.begin().await.expect("a transaction begins");
        assert!(
            mark_used(&mut *tx, token.id)
                .await
                .expect("the spend runs on the transaction"),
            "the spend lands inside the transaction"
        );
        tx.rollback().await.expect("the rollback runs");
        assert!(
            !stored(&pool, token.id).await.used,
            "a rolled-back transaction spent nothing"
        );
    }

    #[tokio::test]
    async fn delete_others_keeps_only_the_named_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user_id = account(&pool, "cleaner").await;
        let other_user = account(&pool, "untouched").await;
        let kept = mint(&pool, user_id).await;
        let doomed = mint(&pool, user_id).await;
        let foreign = mint(&pool, other_user).await;

        let deleted = delete_others(&pool, user_id, kept.id)
            .await
            .expect("the delete runs");
        assert_eq!(deleted, 1, "exactly the account's other token went");
        assert!(
            get_valid(&pool, &doomed.token, HOUR)
                .await
                .expect("the lookup runs")
                .is_none(),
            "the deleted token is gone"
        );
        assert_eq!(
            get_valid(&pool, &foreign.token, HOUR)
                .await
                .expect("the lookup runs")
                .as_ref(),
            Some(&foreign),
            "another account's token is untouched"
        );
        assert_eq!(
            delete_others(&pool, user_id, kept.id)
                .await
                .expect("the second delete runs"),
            0,
            "deleting again removes nothing"
        );
    }

    #[tokio::test]
    async fn a_deleted_account_loses_its_tokens() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user_id = account(&pool, "doomed").await;
        let token = mint(&pool, user_id).await;
        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user_id)
            .execute(&pool)
            .await
            .expect("the account delete runs");
        assert_eq!(deleted.rows_affected(), 1, "the account row went");
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM password_reset_tokens WHERE id = $1")
                .bind(token.id)
                .fetch_one(&pool)
                .await
                .expect("the count reads");
        assert_eq!(rows, 0, "the foreign key cascaded the token away");
    }
}
