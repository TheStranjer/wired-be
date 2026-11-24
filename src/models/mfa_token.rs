//! The `mfa_tokens` table: the short-lived challenge credentials of the
//! OAuth MFA flow (WBE-132).
//!
//! When a login meets an MFA-enabled account, no token is issued; instead
//! [`create`] mints a five-minute challenge row (Pleroma's
//! `Pleroma.MFA.Token.create`, whose `@expires` is 300 seconds) pointing
//! at the account and, through [`MfaToken::authorization_id`], at the
//! pending authorization that carries the grant's parameters. The client
//! presents the token at `POST /oauth/mfa/challenge`; [`get_valid`] reads
//! it back until it expires, and the endpoint [`delete`]s it the moment
//! the second factor passes.
//!
//! The raw token is never stored: the table keeps only its SHA-256 digest
//! ([`token_digest`]). [`create`] hands the raw text back once, in an
//! [`IssuedMfaToken`], for the login response; [`get_valid`] digests the
//! submitted text before it looks. The manual [`Debug`] impls below redact
//! both, so neither can reach a log line.

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::AppResult,
    util::{
        id::new_id,
        token::{CREDENTIAL_BYTES, random_token, token_digest},
    },
};

/// Lifetime of a challenge token: five minutes, the window in which
/// [`get_valid`] accepts it (Pleroma's `Pleroma.MFA.Token` `@expires` of
/// 300 seconds).
pub const MFA_TOKEN_TTL: Duration = Duration::from_mins(5);

/// How many wrong second-factor codes one challenge tolerates (WBE-668).
///
/// [`get_valid`] refuses a challenge whose
/// [`failed_attempts`](MfaToken::failed_attempts) has reached this, and
/// the endpoints delete it at that point, so a TOTP code cannot be
/// brute-forced inside the five-minute window; the client must restart
/// from the password.
pub const MAX_MFA_ATTEMPTS: i32 = 5;

/// One row of `mfa_tokens`: a pending second-factor challenge.
#[derive(sqlx::FromRow)]
pub struct MfaToken {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4).
    pub id: Uuid,
    /// The SHA-256 digest ([`token_digest`]) of the challenge token a
    /// client exchanges for a token pair. Never in `Debug` output; the
    /// token itself is never stored.
    pub token_digest: String,
    /// The account that must prove its second factor.
    pub user_id: Uuid,
    /// The pending authorization carrying the grant's parameters, when
    /// the challenge was minted for one; `NULL` stands for a challenge
    /// that carries nothing.
    pub authorization_id: Option<Uuid>,
    /// Expiry; [`get_valid`] refuses rows at or past it.
    pub valid_until: DateTime<Utc>,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// Wrong second-factor codes presented so far; [`get_valid`] refuses
    /// the row once this reaches [`MAX_MFA_ATTEMPTS`].
    pub failed_attempts: i32,
}

/// [`token_digest`](Self::token_digest) renders as `<redacted>`;
/// everything else shows.
impl fmt::Debug for MfaToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MfaToken")
            .field("id", &self.id)
            .field("token_digest", &"<redacted>")
            .field("user_id", &self.user_id)
            .field("authorization_id", &self.authorization_id)
            .field("valid_until", &self.valid_until)
            .field("inserted_at", &self.inserted_at)
            .field("failed_attempts", &self.failed_attempts)
            .finish()
    }
}

/// A freshly minted challenge: the stored row and the raw token, which
/// exists only here and goes into the login response once.
pub struct IssuedMfaToken {
    /// The stored row; it holds only the digest of [`token`](Self::token).
    pub row: MfaToken,
    /// The raw challenge token; redacted from `Debug`.
    pub token: String,
}

/// [`token`](IssuedMfaToken::token) renders as `<redacted>`.
impl fmt::Debug for IssuedMfaToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedMfaToken")
            .field("row", &self.row)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// The `INSERT` behind [`create`]: the token digest is computed by the
/// caller of the statement, the expiry is computed from the bound
/// lifetime by the database clock, and `RETURNING *` brings back the row
/// with its timestamp.
const INSERT_SQL: &str = "INSERT INTO mfa_tokens (
     id, token_digest, user_id, authorization_id, valid_until
) VALUES ($1, $2, $3, $4, now() + $5) RETURNING *";

/// Creates a five-minute challenge for `user_id` and returns the stored
/// row with the raw token.
///
/// The id is a fresh `UUIDv7` (decision 4) and the token a 43-character
/// random credential (Pleroma's `Token.Utils.generate_token`, the shape
/// every OAuth credential here carries); only its digest is stored.
/// `authorization_id` links the pending authorization whose parameters
/// the finished challenge grants; `None` links nothing. The digest column
/// is unique, so a minted collision is rejected.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(
    pool: &PgPool,
    user_id: Uuid,
    authorization_id: Option<Uuid>,
) -> AppResult<IssuedMfaToken> {
    let token = random_token(CREDENTIAL_BYTES);
    let row = sqlx::query_as::<_, MfaToken>(INSERT_SQL)
        .bind(new_id())
        .bind(token_digest(&token))
        .bind(user_id)
        .bind(authorization_id)
        .bind(MFA_TOKEN_TTL)
        .fetch_one(pool)
        .await?;
    Ok(IssuedMfaToken { row, token })
}

/// Fetches the challenge whose raw `token` is given (looked up by its
/// digest) while it is still valid.
///
/// `None` for an unknown token, for one at or past `valid_until` and for
/// one that has taken [`MAX_MFA_ATTEMPTS`] wrong codes, the cases the
/// endpoints are told not to distinguish.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_valid(pool: &PgPool, token: &str) -> AppResult<Option<MfaToken>> {
    let found = sqlx::query_as::<_, MfaToken>(
        "SELECT * FROM mfa_tokens
         WHERE token_digest = $1 AND valid_until > now() AND failed_attempts < $2",
    )
    .bind(token_digest(token))
    .bind(MAX_MFA_ATTEMPTS)
    .fetch_optional(pool)
    .await?;
    Ok(found)
}

/// Counts one wrong code against challenge `id`; returns the new total.
///
/// An unknown `id` (a challenge retired in the meantime) counts as
/// already at the cap.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn record_failure(pool: &PgPool, id: Uuid) -> AppResult<i32> {
    let attempts: Option<(i32,)> = sqlx::query_as(
        "UPDATE mfa_tokens SET failed_attempts = failed_attempts + 1
         WHERE id = $1 RETURNING failed_attempts",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(attempts.map_or(MAX_MFA_ATTEMPTS, |(count,)| count))
}

/// Deletes the challenge with primary key `id`, retiring it whatever its
/// expiry. An unknown `id` matches no row and changes nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, id: Uuid) -> AppResult<()> {
    sqlx::query("DELETE FROM mfa_tokens WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        models::{
            oauth_app::{NewOauthApp, create as create_app},
            oauth_token,
        },
        test_fixtures::local_user as insert_user,
    };

    /// Registers an app named `name` and returns it.
    async fn insert_app(pool: &PgPool, name: &str) -> crate::models::oauth_app::OauthApp {
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
        .unwrap()
    }

    /// Rows left in `mfa_tokens` with primary key `id`.
    async fn token_rows(pool: &PgPool, id: Uuid) -> i64 {
        let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM mfa_tokens WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
        rows
    }

    /// Moves challenge `id`'s `valid_until` an hour into the past.
    async fn expire(pool: &PgPool, id: Uuid) {
        let outcome = sqlx::query(
            "UPDATE mfa_tokens SET valid_until = now() - interval '1 hour' WHERE id = $1",
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(outcome.rows_affected(), 1, "the expiry update hit the row");
    }

    #[tokio::test]
    async fn create_round_trips_the_stored_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert_user(&pool, "alice").await;
        let app = insert_app(&pool, "Client").await;
        let authorization = oauth_token::create_authorization(&pool, alice.id, app.id, "read", "")
            .await
            .unwrap();
        let issued = create(&pool, alice.id, Some(authorization.id))
            .await
            .unwrap();
        let IssuedMfaToken {
            row: token,
            token: raw,
        } = issued;

        assert!(token.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(raw.len(), 43, "the 43-char credential shape");
        assert_eq!(token.user_id, alice.id, "the account round trips");
        assert_eq!(
            token.authorization_id,
            Some(authorization.id),
            "the linked authorization round trips"
        );
        let remaining = token
            .valid_until
            .signed_duration_since(Utc::now())
            .num_seconds();
        assert!(
            (240..=305).contains(&remaining),
            "the challenge is five minutes out, got {remaining}s"
        );
        assert!(token.inserted_at <= token.valid_until, "timestamps agree");

        let found = get_valid(&pool, &raw)
            .await
            .unwrap()
            .expect("a fresh challenge is valid");
        assert_eq!(found.id, token.id, "found by its text");
        assert_eq!(
            found.token_digest, token.token_digest,
            "the stored digest round trips"
        );

        let unlinked = create(&pool, alice.id, None).await.unwrap();
        assert!(
            unlinked.row.authorization_id.is_none(),
            "NULL links nothing"
        );
    }

    #[tokio::test]
    async fn the_stored_token_is_the_digest_not_the_secret() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let gina = insert_user(&pool, "gina").await;
        let issued = create(&pool, gina.id, None).await.unwrap();
        let (stored_text,): (String,) =
            sqlx::query_as("SELECT token_digest FROM mfa_tokens WHERE id = $1")
                .bind(issued.row.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_ne!(stored_text, issued.token, "the secret is not stored");
        assert_eq!(
            stored_text,
            token_digest(&issued.token),
            "the digest of the secret is"
        );
        assert_eq!(issued.row.token_digest, stored_text, "the row agrees");
        assert!(
            get_valid(&pool, &stored_text).await.unwrap().is_none(),
            "the stored digest is not itself a usable token"
        );
    }

    #[tokio::test]
    async fn get_valid_finds_the_row_by_its_raw_token() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hank = insert_user(&pool, "hank").await;
        let issued = create(&pool, hank.id, None).await.unwrap();
        let found = get_valid(&pool, &issued.token)
            .await
            .unwrap()
            .expect("the raw token finds the challenge");
        assert_eq!(found.id, issued.row.id, "the same row comes back");
    }

    #[tokio::test]
    async fn unknown_and_expired_challenges_are_none() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bob = insert_user(&pool, "bob").await;
        let token = create(&pool, bob.id, None).await.unwrap();
        assert!(
            get_valid(&pool, "no-such-token").await.unwrap().is_none(),
            "an unknown token finds nothing"
        );
        expire(&pool, token.row.id).await;
        assert!(
            get_valid(&pool, &token.token).await.unwrap().is_none(),
            "a challenge past valid_until is refused"
        );
    }

    #[tokio::test]
    async fn record_failure_counts_and_the_cap_refuses_the_challenge() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let frank = insert_user(&pool, "frank").await;
        let token = create(&pool, frank.id, None).await.unwrap();
        let other = create(&pool, frank.id, None).await.unwrap();
        assert_eq!(
            token.row.failed_attempts, 0,
            "a fresh challenge has no failures"
        );

        for expected in 1..MAX_MFA_ATTEMPTS {
            assert_eq!(
                record_failure(&pool, token.row.id).await.unwrap(),
                expected,
                "each failure bumps the count"
            );
            let found = get_valid(&pool, &token.token)
                .await
                .unwrap()
                .expect("under the cap the challenge is still valid");
            assert_eq!(found.failed_attempts, expected, "the count is stored");
        }
        assert_eq!(
            record_failure(&pool, token.row.id).await.unwrap(),
            MAX_MFA_ATTEMPTS,
            "the last allowed failure reaches the cap"
        );
        assert!(
            get_valid(&pool, &token.token).await.unwrap().is_none(),
            "at the cap the challenge is refused"
        );
        assert!(
            get_valid(&pool, &other.token).await.unwrap().is_some(),
            "another challenge is not affected"
        );
        assert_eq!(
            record_failure(&pool, new_id()).await.unwrap(),
            MAX_MFA_ATTEMPTS,
            "an unknown challenge counts as spent"
        );
    }

    #[tokio::test]
    async fn delete_retires_the_challenge() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = insert_user(&pool, "carol").await;
        let token = create(&pool, carol.id, None).await.unwrap();
        delete(&pool, token.row.id).await.unwrap();
        assert_eq!(token_rows(&pool, token.row.id).await, 0, "the row is gone");
        assert!(
            get_valid(&pool, &token.token).await.unwrap().is_none(),
            "and it no longer validates"
        );
        // Deleting nothing is not an error.
        delete(&pool, new_id()).await.unwrap();
    }

    #[tokio::test]
    async fn deleting_the_user_or_the_authorization_cascades() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dave = insert_user(&pool, "dave").await;
        let app = insert_app(&pool, "Cascade").await;
        let authorization = oauth_token::create_authorization(&pool, dave.id, app.id, "read", "")
            .await
            .unwrap();
        let linked = create(&pool, dave.id, Some(authorization.id))
            .await
            .unwrap()
            .row;
        let own_account = create(&pool, dave.id, None).await.unwrap().row;

        let outcome = sqlx::query("DELETE FROM oauth_authorizations WHERE id = $1")
            .bind(authorization.id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(outcome.rows_affected(), 1, "the authorization delete hit");
        assert_eq!(
            token_rows(&pool, linked.id).await,
            0,
            "the linked challenge followed the authorization"
        );

        let gone = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(dave.id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(gone.rows_affected(), 1, "the user delete hit");
        assert_eq!(
            token_rows(&pool, own_account.id).await,
            0,
            "the remaining challenge followed the account"
        );
    }

    #[tokio::test]
    async fn debug_redacts_the_challenge_token() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let erin = insert_user(&pool, "erin").await;
        let issued = create(&pool, erin.id, None).await.unwrap();
        let debug = format!("{issued:?}");
        assert!(!debug.contains(&issued.token), "the token must not leak");
        assert!(
            !debug.contains(&issued.row.token_digest),
            "the digest must not leak"
        );
        assert!(
            debug.contains(r#"token: "<redacted>""#),
            "the redaction marker names the field: {debug}"
        );
        let row_debug = format!("{:?}", issued.row);
        assert!(
            row_debug.contains(r#"token_digest: "<redacted>""#),
            "the row redacts its digest: {row_debug}"
        );
        assert!(debug.contains(&erin.id.to_string()), "the rest shows");
    }

    #[test]
    fn the_challenge_lifetime_is_pleromas_expiry() {
        assert_eq!(
            MFA_TOKEN_TTL.as_secs(),
            300,
            "Pleroma's MFA.Token expires after five minutes"
        );
    }
}
