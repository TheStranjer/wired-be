//! The `oauth_authorizations` table: the one-time codes the
//! `/oauth/authorize` redirect hands out and the exchange that redeems
//! them. The code is secret material, so the manual `Debug` impl redacts
//! it.

use std::fmt;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::AUTHORIZATION_CODE_TTL;
use crate::{
    error::AppResult,
    util::{
        id::new_id,
        token::{CREDENTIAL_BYTES, random_token},
    },
};

/// One row of `oauth_authorizations`: a code the client redeems for a
/// token pair.
#[derive(sqlx::FromRow)]
pub struct OauthAuthorization {
    /// `UUIDv7` primary key, minted by [`create_authorization`]
    /// (decision 4).
    pub id: Uuid,
    /// The authorization code (`?code=` on the redirect). Never in
    /// `Debug` output.
    pub token: String,
    /// The user who granted the authorization.
    pub user_id: Uuid,
    /// The application the code was issued for; the exchange must
    /// present the same app.
    pub app_id: Uuid,
    /// Approved scopes, space-separated; the exchange copies them onto
    /// the token.
    pub scopes: String,
    /// Expiry; fresh codes live [`AUTHORIZATION_CODE_TTL`].
    pub valid_until: DateTime<Utc>,
    /// Whether [`consume_authorization`] has already redeemed the code.
    pub used: bool,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed.
    pub updated_at: DateTime<Utc>,
}

/// [`token`](Self::token) is secret material and renders as
/// `<redacted>`; everything else shows.
impl fmt::Debug for OauthAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OauthAuthorization")
            .field("id", &self.id)
            .field("token", &"<redacted>")
            .field("user_id", &self.user_id)
            .field("app_id", &self.app_id)
            .field("scopes", &self.scopes)
            .field("valid_until", &self.valid_until)
            .field("used", &self.used)
            .field("inserted_at", &self.inserted_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// The `INSERT` behind [`create_authorization`]: the code is minted by
/// the caller of the statement, its ten-minute lifetime is computed
/// from the bound interval by the database clock, and `RETURNING *`
/// brings back the row with its defaults (`used`) and timestamps.
const INSERT_AUTHORIZATION_SQL: &str = "INSERT INTO oauth_authorizations (
     id, token, user_id, app_id, scopes, valid_until
) VALUES ($1, $2, $3, $4, $5, now() + $6) RETURNING *";

/// Creates an authorization code for `user_id` approving `scopes` on
/// behalf of `app_id`, and returns the stored row.
///
/// The id is a fresh `UUIDv7` and the code a 43-character random token
/// (Pleroma's `add_token`); the row starts unused and expires after
/// [`AUTHORIZATION_CODE_TTL`].
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create_authorization(
    pool: &PgPool,
    user_id: Uuid,
    app_id: Uuid,
    scopes: &str,
) -> AppResult<OauthAuthorization> {
    let authorization = sqlx::query_as::<_, OauthAuthorization>(INSERT_AUTHORIZATION_SQL)
        .bind(new_id())
        .bind(random_token(CREDENTIAL_BYTES))
        .bind(user_id)
        .bind(app_id)
        .bind(scopes)
        .bind(AUTHORIZATION_CODE_TTL)
        .fetch_one(pool)
        .await?;
    Ok(authorization)
}

/// Fetches the authorization stored with primary key `id`, exactly as
/// stored: no expiry or `used` filter, [`consume_authorization`]'s gates
/// stay the exchange's call. An unknown `id` is `None`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_authorization_by_id(
    pool: &PgPool,
    id: Uuid,
) -> AppResult<Option<OauthAuthorization>> {
    let found =
        sqlx::query_as::<_, OauthAuthorization>("SELECT * FROM oauth_authorizations WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(found)
}

/// Redeems the authorization code `code` for `app_id`, returning the row
/// marked used, or `None`.
///
/// One atomic `UPDATE` does the whole exchange: only an unused row whose
/// `valid_until` lies in the future and whose `app_id` matches flips to
/// `used = true` and comes back. A second attempt on the same code
/// never succeeds — not even concurrently — and the wrong app cannot
/// burn (or read) another app's code.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn consume_authorization(
    pool: &PgPool,
    code: &str,
    app_id: Uuid,
) -> AppResult<Option<OauthAuthorization>> {
    let consumed = sqlx::query_as::<_, OauthAuthorization>(
        "UPDATE oauth_authorizations
            SET used = true, updated_at = now()
          WHERE token = $1 AND app_id = $2 AND used = false AND valid_until > now()
        RETURNING *",
    )
    .bind(code)
    .bind(app_id)
    .fetch_optional(pool)
    .await?;
    Ok(consumed)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::util::id::new_id;

    #[tokio::test]
    async fn authorization_codes_are_one_time() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let judy = insert_user(&pool, "judy").await;
        let app = insert_app(&pool, "Redeem").await;
        let authorization = create_authorization(&pool, judy.id, app.id, "read write")
            .await
            .unwrap();

        assert!(
            authorization.token.len() == 43 && is_url_safe(&authorization.token),
            "the code is a 43-char URL-safe random token"
        );
        assert!(!authorization.used, "a fresh code is unused");
        assert_eq!(authorization.user_id, judy.id, "the grantor round trips");
        assert_eq!(authorization.app_id, app.id, "the app round trips");
        assert_eq!(authorization.scopes, "read write", "scopes round trip");
        let remaining = authorization
            .valid_until
            .signed_duration_since(Utc::now())
            .num_seconds();
        assert!(
            (540..=660).contains(&remaining),
            "the code is ten minutes out, got {remaining}s"
        );

        let consumed = consume_authorization(&pool, &authorization.token, app.id)
            .await
            .unwrap()
            .expect("the owning app can redeem the code");
        assert_eq!(consumed.id, authorization.id, "the same row comes back");
        assert!(consumed.used, "redemption flips the flag");
        assert!(
            !authorization_unused(&pool, authorization.id).await,
            "the stored row is used now"
        );

        let again = consume_authorization(&pool, &authorization.token, app.id)
            .await
            .unwrap();
        assert!(again.is_none(), "a code cannot be spent twice");
    }

    #[tokio::test]
    async fn the_wrong_app_cannot_consume_a_code() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let ken = insert_user(&pool, "ken").await;
        let app = insert_app(&pool, "Owner").await;
        let thief = insert_app(&pool, "Stranger").await;
        let authorization = create_authorization(&pool, ken.id, app.id, "read")
            .await
            .unwrap();

        let denied = consume_authorization(&pool, &authorization.token, thief.id)
            .await
            .unwrap();
        assert!(denied.is_none(), "another app's exchange fails");
        assert!(
            authorization_unused(&pool, authorization.id).await,
            "and does not even mark the code used"
        );

        let ok = consume_authorization(&pool, &authorization.token, app.id)
            .await
            .unwrap();
        assert!(ok.is_some(), "the owning app still gets its code");
    }

    #[tokio::test]
    async fn expired_codes_are_not_consumed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let luna = insert_user(&pool, "luna").await;
        let app = insert_app(&pool, "Timebox").await;
        let authorization = create_authorization(&pool, luna.id, app.id, "read")
            .await
            .unwrap();
        expire_authorization(&pool, authorization.id).await;
        assert!(
            consume_authorization(&pool, &authorization.token, app.id)
                .await
                .unwrap()
                .is_none(),
            "a code past valid_until is refused"
        );
        assert!(
            authorization_unused(&pool, authorization.id).await,
            "the failed exchange left the row untouched"
        );
    }

    #[tokio::test]
    async fn get_authorization_by_id_reads_the_stored_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let nick = insert_user(&pool, "nick").await;
        let app = insert_app(&pool, "Byld").await;
        let authorization = create_authorization(&pool, nick.id, app.id, "read write")
            .await
            .unwrap();

        let found = get_authorization_by_id(&pool, authorization.id)
            .await
            .unwrap()
            .expect("a stored code is found by its id");
        assert_eq!(found.token, authorization.token, "the code round trips");
        assert_eq!(found.user_id, nick.id, "the grantor round trips");
        assert_eq!(found.app_id, app.id, "the app round trips");
        assert_eq!(found.scopes, "read write", "the scopes round trip");
        assert!(!found.used, "no filter is applied: the row comes as stored");

        // An expired row still arrives; the validity gates belong to the
        // caller, as everywhere in this module.
        expire_authorization(&pool, authorization.id).await;
        assert!(
            get_authorization_by_id(&pool, authorization.id)
                .await
                .unwrap()
                .is_some(),
            "expiry is the caller's call"
        );

        assert!(
            get_authorization_by_id(&pool, new_id())
                .await
                .unwrap()
                .is_none(),
            "an unknown id finds nothing"
        );
    }
}
