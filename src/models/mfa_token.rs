//! The `mfa_tokens` table: the short-lived challenge credentials of the
//! OAuth MFA flow (WBE-132).
//!
//! When a login meets an MFA-enabled account, no token is issued; instead
//! [`create`] mints a five-minute challenge row (Pleroma's
//! `Pleroma.MFA.Token.create`, whose `@expires` is 300 seconds) pointing
//! at the account and, through [`MfaToken::authorization_id`], at the
//! pending authorization that carries the grant's parameters. The client
//! presents the token at `POST /oauth/mfa/challenge` or the browser page
//! completes with it; [`get_valid`] reads it back until it expires, and
//! the endpoints [`delete`] it the moment the second factor passes.
//!
//! `token` is secret material: the manual [`Debug`] impl below redacts
//! it, so it can never reach a log line.

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::AppResult,
    util::{
        id::new_id,
        token::{CREDENTIAL_BYTES, random_token},
    },
};

/// Lifetime of a challenge token: five minutes, the window in which
/// [`get_valid`] accepts it (Pleroma's `Pleroma.MFA.Token` `@expires` of
/// 300 seconds).
pub const MFA_TOKEN_TTL: Duration = Duration::from_mins(5);

/// One row of `mfa_tokens`: a pending second-factor challenge.
#[derive(sqlx::FromRow)]
pub struct MfaToken {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4).
    pub id: Uuid,
    /// The challenge token a client exchanges for a token pair. Never in
    /// `Debug` output.
    pub token: String,
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
}

/// [`token`](Self::token) is secret material and renders as
/// `<redacted>`; everything else shows.
impl fmt::Debug for MfaToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MfaToken")
            .field("id", &self.id)
            .field("token", &"<redacted>")
            .field("user_id", &self.user_id)
            .field("authorization_id", &self.authorization_id)
            .field("valid_until", &self.valid_until)
            .field("inserted_at", &self.inserted_at)
            .finish()
    }
}

/// The `INSERT` behind [`create`]: the token is minted by the caller of
/// the statement, the expiry is computed from the bound lifetime by the
/// database clock, and `RETURNING *` brings back the row with its
/// timestamp.
const INSERT_SQL: &str = "INSERT INTO mfa_tokens (
     id, token, user_id, authorization_id, valid_until
) VALUES ($1, $2, $3, $4, now() + $5) RETURNING *";

/// Creates a five-minute challenge for `user_id` and returns the stored
/// row.
///
/// The id is a fresh `UUIDv7` (decision 4) and the token a 43-character
/// random credential (Pleroma's `Token.Utils.generate_token`, the shape
/// every OAuth credential here carries). `authorization_id` links the
/// pending authorization whose parameters the finished challenge
/// grants; `None` links nothing. The token column is unique, so a minted
/// collision is rejected.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(
    pool: &PgPool,
    user_id: Uuid,
    authorization_id: Option<Uuid>,
) -> AppResult<MfaToken> {
    let token = sqlx::query_as::<_, MfaToken>(INSERT_SQL)
        .bind(new_id())
        .bind(random_token(CREDENTIAL_BYTES))
        .bind(user_id)
        .bind(authorization_id)
        .bind(MFA_TOKEN_TTL)
        .fetch_one(pool)
        .await?;
    Ok(token)
}

/// Fetches the challenge `token` while it is still valid: `None` for an
/// unknown token and for one at or past `valid_until`, the two cases the
/// endpoints are told not to distinguish.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_valid(pool: &PgPool, token: &str) -> AppResult<Option<MfaToken>> {
    let found = sqlx::query_as::<_, MfaToken>(
        "SELECT * FROM mfa_tokens WHERE token = $1 AND valid_until > now()",
    )
    .bind(token)
    .fetch_optional(pool)
    .await?;
    Ok(found)
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
    use crate::models::{
        oauth_app::{NewOauthApp, create as create_app},
        oauth_token,
        user::{NewUser, insert, local_ap_id},
    };

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = "https://test.example";

    /// Inserts a local user named `nickname` and returns the row.
    async fn insert_user(pool: &PgPool, nickname: &str) -> crate::models::user::User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .unwrap()
    }

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
        let token = create(&pool, alice.id, Some(authorization.id))
            .await
            .unwrap();

        assert!(token.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(token.token.len(), 43, "the 43-char credential shape");
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

        let found = get_valid(&pool, &token.token)
            .await
            .unwrap()
            .expect("a fresh challenge is valid");
        assert_eq!(found.id, token.id, "found by its text");
        assert_eq!(found.token, token.token, "the secret round trips");

        let unlinked = create(&pool, alice.id, None).await.unwrap();
        assert!(unlinked.authorization_id.is_none(), "NULL links nothing");
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
        expire(&pool, token.id).await;
        assert!(
            get_valid(&pool, &token.token).await.unwrap().is_none(),
            "a challenge past valid_until is refused"
        );
    }

    #[tokio::test]
    async fn delete_retires_the_challenge() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = insert_user(&pool, "carol").await;
        let token = create(&pool, carol.id, None).await.unwrap();
        delete(&pool, token.id).await.unwrap();
        assert_eq!(token_rows(&pool, token.id).await, 0, "the row is gone");
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
            .unwrap();
        let own_account = create(&pool, dave.id, None).await.unwrap();

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
        let token = create(&pool, erin.id, None).await.unwrap();
        let debug = format!("{token:?}");
        assert!(!debug.contains(&token.token), "the token must not leak");
        assert!(
            debug.contains(r#"token: "<redacted>""#),
            "the redaction marker names the field: {debug}"
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
