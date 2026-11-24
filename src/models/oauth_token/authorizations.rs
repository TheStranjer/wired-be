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
    oauth::pkce::{self, METHOD_S256},
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
    /// The redirect URI the code was issued for; the exchange must
    /// present the same one. `NULL` for legacy rows and for codes never
    /// delivered to a client.
    pub redirect_uri: Option<String>,
    /// Expiry; fresh codes live [`AUTHORIZATION_CODE_TTL`].
    pub valid_until: DateTime<Utc>,
    /// Whether [`consume_authorization`] has already redeemed the code.
    pub used: bool,
    /// The PKCE challenge (RFC 7636) the code is bound to; the exchange
    /// must present the verifier that hashes to it. `NULL` for a code
    /// issued without PKCE.
    pub code_challenge: Option<String>,
    /// How [`code_challenge`](Self::code_challenge) was derived; always
    /// `S256` when a challenge is stored.
    pub code_challenge_method: Option<String>,
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
            .field("redirect_uri", &self.redirect_uri)
            .field("valid_until", &self.valid_until)
            .field("used", &self.used)
            .field("code_challenge", &self.code_challenge)
            .field("code_challenge_method", &self.code_challenge_method)
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
     id, token, user_id, app_id, scopes, valid_until, redirect_uri,
     code_challenge, code_challenge_method
) VALUES ($1, $2, $3, $4, $5, now() + $6, NULLIF($7::text, ''), $8, $9) RETURNING *";

/// What a new code is bound to besides its app: where it may be redeemed
/// and the PKCE challenge it needs.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodeBinding<'a> {
    /// The redirect URI the code is issued for; empty for a code that is
    /// never delivered to a client.
    pub redirect_uri: &'a str,
    /// The validated PKCE challenge; `None` binds nothing.
    pub code_challenge: Option<&'a str>,
}

/// Creates an authorization code for `user_id` approving `scopes`.
///
/// It is issued on behalf of `app_id`, bound to `redirect_uri`, and
/// bound to no PKCE challenge; the stored row comes back.
/// See [`create_authorization_with_challenge`] for the PKCE form.
///
/// The id is a fresh `UUIDv7` and the code a 43-character random token
/// (Pleroma's `add_token`); the row starts unused and expires after
/// [`AUTHORIZATION_CODE_TTL`]. An empty `redirect_uri` (a code that is
/// never delivered to a client) is stored as `NULL`.
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
    redirect_uri: &str,
) -> AppResult<OauthAuthorization> {
    let binding = CodeBinding {
        redirect_uri,
        code_challenge: None,
    };
    create_authorization_with_challenge(pool, user_id, app_id, scopes, binding).await
}

/// [`create_authorization`] for a code that may carry a PKCE challenge.
///
/// The challenge in `binding` is already validated by
/// [`crate::oauth::pkce::parse_challenge`]; the stored method is always
/// `S256`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create_authorization_with_challenge(
    pool: &PgPool,
    user_id: Uuid,
    app_id: Uuid,
    scopes: &str,
    binding: CodeBinding<'_>,
) -> AppResult<OauthAuthorization> {
    let CodeBinding {
        redirect_uri,
        code_challenge,
    } = binding;
    let authorization = sqlx::query_as::<_, OauthAuthorization>(INSERT_AUTHORIZATION_SQL)
        .bind(new_id())
        .bind(random_token(CREDENTIAL_BYTES))
        .bind(user_id)
        .bind(app_id)
        .bind(scopes)
        .bind(AUTHORIZATION_CODE_TTL)
        .bind(redirect_uri)
        .bind(code_challenge)
        .bind(code_challenge.map(|_| METHOD_S256))
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
/// Presents no PKCE verifier, so a code bound to a challenge is never
/// redeemed here; see [`consume_authorization_with_verifier`] for the
/// rules.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn consume_authorization(
    pool: &PgPool,
    code: &str,
    app_id: Uuid,
    redirect_uri: Option<&str>,
) -> AppResult<Option<OauthAuthorization>> {
    consume_authorization_with_verifier(pool, code, app_id, redirect_uri, None).await
}

/// The row [`consume_authorization_with_verifier`] may redeem, locked so
/// a concurrent attempt waits for the verdict and then finds it used.
const SELECT_REDEEMABLE_SQL: &str = "SELECT * FROM oauth_authorizations
      WHERE token = $1 AND app_id = $2 AND used = false AND valid_until > now()
        AND (redirect_uri IS NULL OR redirect_uri = $3)
        FOR UPDATE";

/// Redeems the authorization code `code` for `app_id`, returning the row
/// marked used, or `None`.
///
/// Inside one transaction the unused, unexpired row of `app_id` is locked
/// and, when it passes every gate, flipped to `used = true`. A second
/// attempt on the same code never succeeds — not even concurrently — and
/// the wrong app cannot burn (or read) another app's code. A row bound to
/// a redirect URI is consumed only when `redirect_uri` is that same URI
/// (RFC 6749 §4.1.3); a missing or different one leaves it untouched. A
/// row with no stored URI (legacy, or never delivered) accepts any
/// `redirect_uri`. A row bound to a PKCE challenge is consumed only when
/// `code_verifier` is the verifier it hashes from
/// ([`crate::oauth::pkce::verifies`], constant time); a missing or wrong
/// one leaves the code unused. A row bound to none ignores the verifier.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn consume_authorization_with_verifier(
    pool: &PgPool,
    code: &str,
    app_id: Uuid,
    redirect_uri: Option<&str>,
    code_verifier: Option<&str>,
) -> AppResult<Option<OauthAuthorization>> {
    let mut tx = pool.begin().await?;
    let Some(found) = sqlx::query_as::<_, OauthAuthorization>(SELECT_REDEEMABLE_SQL)
        .bind(code)
        .bind(app_id)
        .bind(redirect_uri)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(None);
    };
    if !pkce::verifies(found.code_challenge.as_deref(), code_verifier) {
        // Dropping the transaction rolls it back: the code stays unused.
        return Ok(None);
    }
    let consumed = sqlx::query_as::<_, OauthAuthorization>(
        "UPDATE oauth_authorizations SET used = true, updated_at = now()
          WHERE id = $1 RETURNING *",
    )
    .bind(found.id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(consumed))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::util::id::new_id;

    /// The redirect URI the codes in these tests are issued for.
    const REDIRECT_A: &str = "https://client.example/a";
    /// Another URI of the same client, never the one a code was issued for.
    const REDIRECT_B: &str = "https://client.example/b";

    #[tokio::test]
    async fn a_code_only_redeems_with_the_redirect_uri_it_was_issued_for() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let omar = insert_user(&pool, "omar").await;
        let app = insert_app(&pool, "Bound").await;
        let authorization = create_authorization(&pool, omar.id, app.id, "read", REDIRECT_A)
            .await
            .unwrap();
        assert_eq!(
            authorization.redirect_uri.as_deref(),
            Some(REDIRECT_A),
            "the URI round trips"
        );

        for presented in [Some(REDIRECT_B), None, Some("")] {
            assert!(
                consume_authorization(&pool, &authorization.token, app.id, presented)
                    .await
                    .unwrap()
                    .is_none(),
                "{presented:?} does not redeem a code issued for {REDIRECT_A}"
            );
            assert!(
                authorization_unused(&pool, authorization.id).await,
                "the refused exchange left the row unused"
            );
        }

        let consumed = consume_authorization(&pool, &authorization.token, app.id, Some(REDIRECT_A))
            .await
            .unwrap();
        assert!(consumed.is_some(), "the matching URI redeems the code");
    }

    #[tokio::test]
    async fn a_code_without_a_stored_redirect_uri_is_consumable() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let pam = insert_user(&pool, "pam").await;
        let app = insert_app(&pool, "Legacy").await;
        let authorization = create_authorization(&pool, pam.id, app.id, "read", "")
            .await
            .unwrap();
        assert!(
            authorization.redirect_uri.is_none(),
            "an empty URI is stored as NULL"
        );

        let consumed = consume_authorization(&pool, &authorization.token, app.id, Some(REDIRECT_B))
            .await
            .unwrap();
        assert!(consumed.is_some(), "a NULL-URI legacy row accepts any URI");
    }

    #[tokio::test]
    async fn authorization_codes_are_one_time() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let judy = insert_user(&pool, "judy").await;
        let app = insert_app(&pool, "Redeem").await;
        let authorization = create_authorization(&pool, judy.id, app.id, "read write", REDIRECT_A)
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

        let consumed = consume_authorization(&pool, &authorization.token, app.id, Some(REDIRECT_A))
            .await
            .unwrap()
            .expect("the owning app can redeem the code");
        assert_eq!(consumed.id, authorization.id, "the same row comes back");
        assert!(consumed.used, "redemption flips the flag");
        assert!(
            !authorization_unused(&pool, authorization.id).await,
            "the stored row is used now"
        );

        let again = consume_authorization(&pool, &authorization.token, app.id, Some(REDIRECT_A))
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
        let authorization = create_authorization(&pool, ken.id, app.id, "read", REDIRECT_A)
            .await
            .unwrap();

        let denied = consume_authorization(&pool, &authorization.token, thief.id, Some(REDIRECT_A))
            .await
            .unwrap();
        assert!(denied.is_none(), "another app's exchange fails");
        assert!(
            authorization_unused(&pool, authorization.id).await,
            "and does not even mark the code used"
        );

        let ok = consume_authorization(&pool, &authorization.token, app.id, Some(REDIRECT_A))
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
        let authorization = create_authorization(&pool, luna.id, app.id, "read", REDIRECT_A)
            .await
            .unwrap();
        expire_authorization(&pool, authorization.id).await;
        assert!(
            consume_authorization(&pool, &authorization.token, app.id, Some(REDIRECT_A))
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
        let authorization = create_authorization(&pool, nick.id, app.id, "read write", REDIRECT_A)
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

    /// The RFC 7636 Appendix B verifier and its S256 challenge.
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    /// The challenge [`VERIFIER`] hashes to.
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    #[tokio::test]
    async fn a_pkce_bound_code_redeems_only_with_its_verifier() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let quinn = insert_user(&pool, "quinn").await;
        let app = insert_app(&pool, "Pkce").await;
        let authorization = create_authorization_with_challenge(
            &pool,
            quinn.id,
            app.id,
            "read",
            CodeBinding {
                redirect_uri: REDIRECT_A,
                code_challenge: Some(CHALLENGE),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            authorization.code_challenge.as_deref(),
            Some(CHALLENGE),
            "the challenge round trips"
        );
        assert_eq!(
            authorization.code_challenge_method.as_deref(),
            Some("S256"),
            "the method is recorded"
        );

        let wrong = "b".repeat(43);
        for presented in [None, Some(wrong.as_str()), Some(CHALLENGE), Some("")] {
            assert!(
                consume_authorization_with_verifier(
                    &pool,
                    &authorization.token,
                    app.id,
                    Some(REDIRECT_A),
                    presented
                )
                .await
                .unwrap()
                .is_none(),
                "a missing or wrong verifier is refused"
            );
            assert!(
                authorization_unused(&pool, authorization.id).await,
                "the refusal left the code unused"
            );
        }
        assert!(
            consume_authorization(&pool, &authorization.token, app.id, Some(REDIRECT_A))
                .await
                .unwrap()
                .is_none(),
            "the verifier-less entry point cannot redeem a bound code"
        );

        let consumed = consume_authorization_with_verifier(
            &pool,
            &authorization.token,
            app.id,
            Some(REDIRECT_A),
            Some(VERIFIER),
        )
        .await
        .unwrap()
        .expect("the right verifier redeems");
        assert!(consumed.used, "the code is flipped");
        assert!(
            consume_authorization_with_verifier(
                &pool,
                &authorization.token,
                app.id,
                Some(REDIRECT_A),
                Some(VERIFIER)
            )
            .await
            .unwrap()
            .is_none(),
            "a second redemption is refused"
        );
    }

    #[tokio::test]
    async fn a_code_without_a_challenge_ignores_the_verifier() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let rae = insert_user(&pool, "rae").await;
        let app = insert_app(&pool, "NoPkce").await;
        let authorization = create_authorization(&pool, rae.id, app.id, "read", REDIRECT_A)
            .await
            .unwrap();
        assert!(
            authorization.code_challenge.is_none() && authorization.code_challenge_method.is_none(),
            "nothing is bound"
        );
        assert!(
            consume_authorization_with_verifier(
                &pool,
                &authorization.token,
                app.id,
                Some(REDIRECT_A),
                Some(VERIFIER)
            )
            .await
            .unwrap()
            .is_some(),
            "a stray verifier is harmless"
        );
    }
}
