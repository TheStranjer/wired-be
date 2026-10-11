//! The `oauth_tokens` table: issued bearer credentials.
//!
//! The reference shape is Pleroma's `Token` schema
//! (`lib/pleroma/web/o_auth/token.ex`). A token row carries an access
//! `token` and a `refresh_token` — both 43-character URL-safe random
//! tokens from [`random_token`] — and grants `scopes` until
//! `valid_until`. `user_id` is nullable: `NULL` is an app-only token
//! minted from the `client_credentials` grant, acting as the application
//! itself. A new token's lifetime defaults to [`DEFAULT_TOKEN_TTL`]
//! (30 days, the span Pleroma's `OAUTH_TOKEN_TTL` starts at); callers
//! that read a config pass their own span as [`NewToken::ttl`].
//!
//! The `/oauth/token` endpoints that mint these rows arrive with their
//! own ticket; until then [`create_token`] is the only way one is born,
//! and tests mint fixtures through it directly.
//!
//! `token` and `refresh_token` are secret material: the manual `Debug`
//! impl redacts them, so they can never reach a log line.

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::oauth_app::OauthApp,
    util::{
        id::new_id,
        token::{CREDENTIAL_BYTES, random_token},
    },
};

/// Default lifetime of a newly created access token: 30 days;
/// [`NewToken::ttl`] `None` means this.
pub const DEFAULT_TOKEN_TTL: Duration = Duration::from_hours(720);

/// One row of `oauth_tokens`: an issued bearer token pair.
#[derive(sqlx::FromRow)]
pub struct OauthToken {
    /// `UUIDv7` primary key, minted by [`create_token`] (decision 4).
    pub id: Uuid,
    /// The access token, presented as `Authorization: Bearer`. Never in
    /// `Debug` output.
    pub token: String,
    /// The refresh token, exchanged for a fresh pair. Never in `Debug`
    /// output.
    pub refresh_token: String,
    /// The user this token acts as; `None` for an app-only token from
    /// the `client_credentials` grant.
    pub user_id: Option<Uuid>,
    /// The application the token was issued for.
    pub app_id: Uuid,
    /// Granted scopes, space-separated (see
    /// [`crate::oauth::scopes::Scopes`]).
    pub scopes: String,
    /// Expiry; [`crate::auth`] refuses tokens at or past it.
    pub valid_until: DateTime<Utc>,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed.
    pub updated_at: DateTime<Utc>,
}

/// [`token`](Self::token) and [`refresh_token`](Self::refresh_token) are
/// secret material and render as `<redacted>`; everything else shows.
impl fmt::Debug for OauthToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OauthToken")
            .field("id", &self.id)
            .field("token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("user_id", &self.user_id)
            .field("app_id", &self.app_id)
            .field("scopes", &self.scopes)
            .field("valid_until", &self.valid_until)
            .field("inserted_at", &self.inserted_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// What [`create_token`] inserts, besides what it mints itself (`id`,
/// `token`, `refresh_token`) and what the database stamps (the
/// timestamps).
#[derive(Clone, Debug)]
pub struct NewToken {
    /// The user the token acts as; `None` mints an app-only
    /// `client_credentials` token.
    pub user_id: Option<Uuid>,
    /// The application the token is issued for.
    pub app_id: Uuid,
    /// Scopes as one space-separated string.
    pub scopes: String,
    /// Token lifetime; `None` applies [`DEFAULT_TOKEN_TTL`].
    pub ttl: Option<Duration>,
}

/// The `INSERT` behind [`create_token`]: both credential columns are
/// minted by the caller of the statement, the expiry is computed from
/// the bound lifetime by the database clock, and `RETURNING *` brings
/// back the row with its timestamps.
const INSERT_TOKEN_SQL: &str = "INSERT INTO oauth_tokens (
     id, token, refresh_token, user_id, app_id, scopes, valid_until
) VALUES ($1, $2, $3, $4, $5, $6, now() + $7) RETURNING *";

/// Inserts a token pair for `new_token` and returns the stored row.
///
/// The id is a fresh `UUIDv7` (decision 4); `token` and `refresh_token`
/// are two independent 43-character random tokens (Pleroma's
/// `Token.Utils.generate_token`), and `valid_until` is the database's
/// `now()` plus the requested lifetime (default
/// [`DEFAULT_TOKEN_TTL`]). Both token columns are unique, so a minted
/// collision is rejected.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create_token(pool: &PgPool, new_token: &NewToken) -> AppResult<OauthToken> {
    let ttl = new_token.ttl.unwrap_or(DEFAULT_TOKEN_TTL);
    let token = sqlx::query_as::<_, OauthToken>(INSERT_TOKEN_SQL)
        .bind(new_id())
        .bind(random_token(CREDENTIAL_BYTES))
        .bind(random_token(CREDENTIAL_BYTES))
        .bind(new_token.user_id)
        .bind(new_token.app_id)
        .bind(new_token.scopes.as_str())
        .bind(ttl)
        .fetch_one(pool)
        .await?;
    Ok(token)
}

/// The `SELECT` behind [`get_with_app`]: the `oauth_tokens` row
/// verbatim, then every `oauth_apps` column but its `id` aliased under
/// an `app_` prefix — the two tables share `scopes`, `user_id` and the
/// timestamps, so a bare `a.*` would read the token's values into the
/// app. `a.id` needs no alias or column: the join condition makes it
/// [`OauthToken::app_id`], so it is not selected at all, and the
/// `ON DELETE CASCADE` foreign key on `app_id` makes the joined app
/// total.
const TOKEN_WITH_APP_SQL: &str = "SELECT t.*,
        a.client_name AS app_client_name,
        a.redirect_uris AS app_redirect_uris, a.scopes AS app_scopes,
        a.website AS app_website, a.client_id AS app_client_id,
        a.client_secret AS app_client_secret, a.trusted AS app_trusted,
        a.user_id AS app_user_id, a.inserted_at AS app_inserted_at,
        a.updated_at AS app_updated_at
   FROM oauth_tokens t
   JOIN oauth_apps a ON a.id = t.app_id
  WHERE t.token = $1";

/// Join row for [`get_with_app`]: the [`OauthToken`] flattened from the
/// leading `t.*` columns (the first match wins every shared name) and
/// the [`OauthApp`]'s columns read through their `app_` aliases.
#[derive(sqlx::FromRow)]
struct TokenWithAppRow {
    /// The `oauth_tokens` row.
    #[sqlx(flatten)]
    token: OauthToken,
    /// `oauth_apps.client_name`.
    app_client_name: String,
    /// `oauth_apps.redirect_uris`.
    app_redirect_uris: String,
    /// `oauth_apps.scopes`, aliased `app_scopes` in SQL.
    app_scopes: String,
    /// `oauth_apps.website`.
    app_website: Option<String>,
    /// `oauth_apps.client_id`.
    app_client_id: String,
    /// `oauth_apps.client_secret`.
    app_client_secret: String,
    /// `oauth_apps.trusted`.
    app_trusted: bool,
    /// `oauth_apps.user_id`.
    app_user_id: Option<Uuid>,
    /// `oauth_apps.inserted_at`.
    app_inserted_at: DateTime<Utc>,
    /// `oauth_apps.updated_at`.
    app_updated_at: DateTime<Utc>,
}

impl TokenWithAppRow {
    /// Splits the join row into its token and app; the app's primary key
    /// is the join's other side, [`OauthToken::app_id`].
    fn into_parts(self) -> (OauthToken, OauthApp) {
        let app = OauthApp {
            id: self.token.app_id,
            client_name: self.app_client_name,
            redirect_uris: self.app_redirect_uris,
            scopes: self.app_scopes,
            website: self.app_website,
            client_id: self.app_client_id,
            client_secret: self.app_client_secret,
            trusted: self.app_trusted,
            user_id: self.app_user_id,
            inserted_at: self.app_inserted_at,
            updated_at: self.app_updated_at,
        };
        (self.token, app)
    }
}

/// Fetches the access token `token` together with the application that
/// issued it, exactly as stored.
///
/// **No validity filter**: an expired row is still returned, leaving
/// expiry and account activity to the caller. [`crate::auth`] uses it to
/// tell an unknown or expired token (401) from a deactivated owner (also
/// 401 for the extractors that lump the two together).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_with_app(pool: &PgPool, token: &str) -> AppResult<Option<(OauthToken, OauthApp)>> {
    let row = sqlx::query_as::<_, TokenWithAppRow>(TOKEN_WITH_APP_SQL)
        .bind(token)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(TokenWithAppRow::into_parts))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::models::oauth_app::{self, NewOauthApp};

    /// Inserts a bare local user row and returns its id.
    async fn insert_user(pool: &PgPool, nickname: &str) -> Uuid {
        let id = new_id();
        let outcome =
            sqlx::query("INSERT INTO users (id, nickname, ap_id, local) VALUES ($1, $2, $3, true)")
                .bind(id)
                .bind(nickname)
                .bind(format!("https://test.example/users/{nickname}"))
                .execute(pool)
                .await
                .unwrap();
        assert_eq!(
            outcome.rows_affected(),
            1,
            "the fixture insert wrote the row"
        );
        id
    }

    /// Registers a minimal app and returns its row.
    async fn insert_app(pool: &PgPool) -> OauthApp {
        oauth_app::create(
            pool,
            &NewOauthApp {
                client_name: "Token Test".to_owned(),
                redirect_uris: "https://client.example/callback".to_owned(),
                scopes: "read".to_owned(),
                website: None,
                user_id: None,
            },
        )
        .await
        .unwrap()
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_token_round_trips_and_defaults_the_lifetime(pool: PgPool) {
        let app = insert_app(&pool).await;
        let token = create_token(
            &pool,
            &NewToken {
                user_id: None,
                app_id: app.id,
                scopes: "read write".to_owned(),
                ttl: None,
            },
        )
        .await
        .unwrap();
        assert!(token.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(token.token.len(), 43, "a 43-character access token");
        assert_eq!(token.refresh_token.len(), 43, "and refresh token");
        assert_ne!(token.token, token.refresh_token, "the two differ");
        assert_eq!(token.app_id, app.id, "the issuing app round trips");
        assert!(token.user_id.is_none(), "an app-only token has no user");
        assert_eq!(token.scopes, "read write", "scopes round trip");
        let until = token.valid_until;
        let now = Utc::now();
        let min_ttl = Duration::from_hours(719);
        let max_ttl = Duration::from_hours(721);
        let lifetime = until.signed_duration_since(now).to_std().unwrap_or(min_ttl);
        assert!(
            lifetime > min_ttl && lifetime < max_ttl,
            "the default lifetime is about 30 days, got {lifetime:?}"
        );

        let fetched =
            sqlx::query_scalar::<_, String>("SELECT token FROM oauth_tokens WHERE id = $1")
                .bind(token.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(fetched, token.token, "the row is the returned one");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_bound_user_and_a_custom_ttl_round_trip(pool: PgPool) {
        let user_id = insert_user(&pool, "tokuser").await;
        let app = insert_app(&pool).await;
        let token = create_token(
            &pool,
            &NewToken {
                user_id: Some(user_id),
                app_id: app.id,
                scopes: "read".to_owned(),
                ttl: Some(Duration::from_mins(30)),
            },
        )
        .await
        .unwrap();
        assert_eq!(token.user_id, Some(user_id), "the user round trips");
        let lifetime = token
            .valid_until
            .signed_duration_since(Utc::now())
            .to_std()
            .unwrap_or(Duration::ZERO);
        assert!(
            lifetime > Duration::from_mins(25) && lifetime < Duration::from_mins(35),
            "the custom lifetime is honoured, got {lifetime:?}"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn get_with_app_returns_the_token_and_its_issuer(pool: PgPool) {
        let user_id = insert_user(&pool, "joinuser").await;
        let app = insert_app(&pool).await;
        let token = create_token(
            &pool,
            &NewToken {
                user_id: Some(user_id),
                app_id: app.id,
                scopes: "read".to_owned(),
                ttl: Some(Duration::from_secs(60)),
            },
        )
        .await
        .unwrap();

        let (found, found_app) = get_with_app(&pool, &token.token)
            .await
            .unwrap()
            .expect("the join finds the row");
        assert_eq!(found.id, token.id, "the token row");
        assert_eq!(
            found.scopes, "read",
            "the token's own scopes, not the app's"
        );
        assert_eq!(found_app.id, app.id, "the app id is the token's app_id");
        assert_eq!(found_app.client_name, "Token Test", "the app columns");
        assert_eq!(found_app.client_id, app.client_id, "the app credentials");
        assert_eq!(
            found_app.client_secret, app.client_secret,
            "join is complete"
        );
        assert_eq!(found_app.scopes, app.scopes, "the app's own scopes");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn get_with_app_of_an_unknown_token_is_none(pool: PgPool) {
        assert!(
            get_with_app(&pool, "no-such-token")
                .await
                .unwrap()
                .is_none(),
            "an unknown token finds nothing"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn debug_redacts_both_token_columns(pool: PgPool) {
        let app = insert_app(&pool).await;
        let token = create_token(
            &pool,
            &NewToken {
                user_id: None,
                app_id: app.id,
                scopes: "read".to_owned(),
                ttl: None,
            },
        )
        .await
        .unwrap();
        let debug = format!("{token:?}");
        assert!(
            !debug.contains(&token.token),
            "the access token must not leak"
        );
        assert!(
            !debug.contains(&token.refresh_token),
            "the refresh token must not leak"
        );
        assert!(debug.contains(r#"token: "<redacted>""#), "the marker");
    }
}
