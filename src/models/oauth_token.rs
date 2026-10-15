//! The `oauth_tokens` and `oauth_authorizations` tables: issued bearer
//! credentials and the one-time authorization codes they are exchanged
//! from.
//!
//! The reference shapes are Pleroma's `Token` and `Authorization`
//! schemas (`lib/pleroma/web/o_auth/token.ex`,
//! `lib/pleroma/web/o_auth/authorization.ex`). A token row carries an
//! access `token` and a `refresh_token` — both 43-character URL-safe
//! random tokens from [`random_token`] — and grants `scopes` until
//! `valid_until`. `user_id` is nullable: `NULL` is an app-only token
//! minted from the `client_credentials` grant, acting as the
//! application itself. A new token's lifetime defaults to
//! [`DEFAULT_TOKEN_TTL`] (30 days, the value the `OAUTH_TOKEN_TTL_SECS`
//! setting starts at; see [`crate::config`]); callers that read the
//! config pass the configured span as [`NewToken::ttl`].
//!
//! An authorization row is the `/oauth/authorize` redirect's `code`:
//! [`create_authorization`] mints one valid for ten minutes, and
//! [`consume_authorization`] exchanges it for a row exactly once — the
//! `UPDATE ... WHERE used = false` flips the flag in the same statement
//! that returns the row, so a second attempt (or one from the wrong
//! app) yields `None`.
//!
//! `token` and `refresh_token` are secret material: the manual `Debug`
//! impls in this module redact them, so they can never reach a log line.

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        oauth_app::OauthApp,
        user::{self, User},
    },
    util::{
        id::new_id,
        token::{CREDENTIAL_BYTES, random_token},
    },
};

/// Default lifetime of a newly created access token: 30 days, the same
/// span [`DEFAULT_OAUTH_TOKEN_TTL_SECS`] spells in seconds (a test keeps
/// the two equal); [`NewToken::ttl`] `None` means this.
///
/// [`DEFAULT_OAUTH_TOKEN_TTL_SECS`]: crate::config::DEFAULT_OAUTH_TOKEN_TTL_SECS
pub const DEFAULT_TOKEN_TTL: Duration = Duration::from_hours(720);

/// Lifetime of an authorization code: ten minutes, the window between
/// the `/oauth/authorize` redirect and the token exchange in which
/// [`consume_authorization`] accepts it.
pub const AUTHORIZATION_CODE_TTL: Duration = Duration::from_mins(10);

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
    /// Expiry; [`get_valid_token`] refuses tokens at or past it.
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
    /// Token lifetime; `None` applies [`DEFAULT_TOKEN_TTL`]. Callers
    /// pass the configured `OAUTH_TOKEN_TTL_SECS` here.
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

/// Join row for [`get_valid_token`]: every [`OauthToken`] column plus
/// the owning user's `is_active` flag, which a `LEFT JOIN` leaves `NULL`
/// for user-unbound rows.
#[derive(sqlx::FromRow)]
struct ValidTokenRow {
    /// The token row itself, flattened over the `t.*` columns.
    #[sqlx(flatten)]
    token: OauthToken,
    /// `users.is_active` of the joined user; `None` for app-only tokens.
    user_is_active: Option<bool>,
}

/// Looks up the access token `token` and, when it is still valid,
/// returns it with its user (`None` for an app-only token).
///
/// A token is valid when its row exists, `valid_until` lies in the
/// future, and — for user-bound tokens — the joined user is still
/// active. Anything else (unknown token, expired, deactivated user) is
/// `None`; a suspended account's bearer tokens stop working the moment
/// the flag flips.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_valid_token(
    pool: &PgPool,
    token: &str,
) -> AppResult<Option<(OauthToken, Option<User>)>> {
    let row = sqlx::query_as::<_, ValidTokenRow>(
        "SELECT t.*, u.is_active AS user_is_active
           FROM oauth_tokens t
           LEFT JOIN users u ON u.id = t.user_id
          WHERE t.token = $1 AND t.valid_until > now()",
    )
    .bind(token)
    .fetch_optional(pool)
    .await?;
    let Some(ValidTokenRow {
        token: found,
        user_is_active,
    }) = row
    else {
        return Ok(None);
    };
    // An app-only token (NULL user_id) answers NULL for the joined flag
    // and passes; a user-bound one needs a live, active user.
    if found.user_id.is_some() && !user_is_active.unwrap_or_default() {
        return Ok(None);
    }
    let user = match found.user_id {
        Some(user_id) => user::get_by_id(pool, user_id).await?,
        None => None,
    };
    Ok(Some((found, user)))
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
/// expiry and account activity to the caller (unlike
/// [`get_valid_token`], which hides both). [`crate::auth`] uses it to
/// tell an expired token (401) from a deactivated owner (403).
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

/// Fetches the token whose `refresh_token` is `refresh_token`, `None`
/// when no row matches.
///
/// No expiry is applied: a refresh token is precisely what a client
/// presents after its access token has lapsed; the caller decides how
/// long the pair itself may live.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_refresh_token(
    pool: &PgPool,
    refresh_token: &str,
) -> AppResult<Option<OauthToken>> {
    let token =
        sqlx::query_as::<_, OauthToken>("SELECT * FROM oauth_tokens WHERE refresh_token = $1")
            .bind(refresh_token)
            .fetch_optional(pool)
            .await?;
    Ok(token)
}

/// Deletes the token with primary key `id`. An unknown `id` matches no
/// row and changes nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_token(pool: &PgPool, id: Uuid) -> AppResult<()> {
    sqlx::query("DELETE FROM oauth_tokens WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// Deletes every token whose `valid_until` has passed, reporting how
/// many rows went.
///
/// The sweep Pleroma schedules per token
/// (`Pleroma.Workers.PurgeExpiredToken`) is a batch delete here; a later
/// ticket runs it on a cadence. User-bound rows are also removed by the
/// `ON DELETE CASCADE` from `users` long before they expire.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_expired(pool: &PgPool) -> AppResult<u64> {
    let deleted = sqlx::query("DELETE FROM oauth_tokens WHERE valid_until <= now()")
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

/// Fetches every token (expired included) belonging to user `user_id`,
/// newest first by creation order (`UUIDv7` ids sort by mint time,
/// decision 4). A user with no tokens yields an empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<OauthToken>> {
    let tokens = sqlx::query_as::<_, OauthToken>(
        "SELECT * FROM oauth_tokens WHERE user_id = $1 ORDER BY id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(tokens)
}

/// One row of [`list_valid_with_app_names`]: a still-valid token joined
/// with the name of the application that issued it.
#[derive(Debug, sqlx::FromRow)]
pub struct TokenWithAppName {
    /// The token's `UUIDv7` primary key (decision 4).
    pub id: Uuid,
    /// `oauth_apps.client_name` of the issuing application.
    pub app_name: String,
    /// When the token stops being valid; always in the future.
    pub valid_until: DateTime<Utc>,
}

/// The `SELECT` behind [`list_valid_with_app_names`]: only the three
/// columns the token list renders, `JOIN`ed for the app name (the
/// `ON DELETE CASCADE` foreign key on `app_id` makes the join total),
/// unexpired rows only, and `ORDER BY t.id DESC` so the newest token
/// leads (`UUIDv7` text sorts by mint time, decision 4).
const LIST_VALID_WITH_APP_NAMES_SQL: &str = "SELECT t.id, a.client_name AS app_name, t.valid_until
   FROM oauth_tokens t
   JOIN oauth_apps a ON a.id = t.app_id
  WHERE t.user_id = $1 AND t.valid_until > now()
  ORDER BY t.id DESC";

/// Fetches the still-valid (unexpired) tokens of user `user_id` with
/// their app names, newest first: what `GET /api/oauth_tokens` lists.
///
/// Expired and app-only rows (no `user_id`) never appear; a user with
/// no live tokens yields an empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list_valid_with_app_names(
    pool: &PgPool,
    user_id: Uuid,
) -> AppResult<Vec<TokenWithAppName>> {
    let rows = sqlx::query_as::<_, TokenWithAppName>(LIST_VALID_WITH_APP_NAMES_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Deletes the token `id` only when it belongs to user `user_id`,
/// reporting whether a row went: this backs
/// `DELETE /api/oauth_tokens/{id}`.
///
/// A foreign, unknown or app-only (NULL `user_id`) token deletes
/// nothing and answers `false`, which is how the endpoint tells "gone"
/// from 404.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_for_user(pool: &PgPool, user_id: Uuid, id: Uuid) -> AppResult<bool> {
    let deleted = sqlx::query("DELETE FROM oauth_tokens WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() == 1)
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
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::DEFAULT_OAUTH_TOKEN_TTL_SECS,
        models::oauth_app::{NewOauthApp, OauthApp, create as create_app},
        models::user::NewUser,
    };

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = "https://test.example";

    /// Whether every character is in the URL-safe base64 alphabet.
    fn is_url_safe(token: &str) -> bool {
        token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }

    /// Inserts a local user named `nickname` and returns the row.
    async fn insert_user(pool: &PgPool, nickname: &str) -> User {
        user::insert(
            pool,
            &NewUser::local(nickname, &user::local_ap_id(BASE, nickname)),
        )
        .await
        .unwrap()
    }

    /// Registers an app named `name` and returns it.
    async fn insert_app(pool: &PgPool, name: &str) -> OauthApp {
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

    /// A [`NewToken`] for `app_id` acting as `user_id`, default lifetime.
    fn new_token(user_id: Option<Uuid>, app_id: Uuid) -> NewToken {
        NewToken {
            user_id,
            app_id,
            scopes: "read".to_owned(),
            ttl: None,
        }
    }

    /// Moves token `id`'s `valid_until` an hour into the past.
    async fn expire_token(pool: &PgPool, id: Uuid) {
        let outcome = sqlx::query(
            "UPDATE oauth_tokens SET valid_until = now() - interval '1 hour' WHERE id = $1",
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(outcome.rows_affected(), 1, "the expiry update hit the row");
    }

    /// Moves authorization `id`'s `valid_until` an hour into the past.
    async fn expire_authorization(pool: &PgPool, id: Uuid) {
        let outcome = sqlx::query(
            "UPDATE oauth_authorizations
                SET valid_until = now() - interval '1 hour'
              WHERE id = $1",
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(outcome.rows_affected(), 1, "the expiry update hit the row");
    }

    /// Deactivates user `id`, the way a suspension would.
    async fn deactivate_user(pool: &PgPool, id: Uuid) {
        let outcome = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            outcome.rows_affected(),
            1,
            "the deactivation update hit the row"
        );
    }

    /// Deletes user `id`, firing the cascades.
    async fn delete_user(pool: &PgPool, id: Uuid) {
        let outcome = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(outcome.rows_affected(), 1, "the user delete hit the row");
    }

    /// Rows left in `oauth_tokens` with primary key `id`.
    async fn token_rows(pool: &PgPool, id: Uuid) -> i64 {
        let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_tokens WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
        rows
    }

    /// Rows left in `oauth_authorizations` with primary key `id`.
    async fn authorization_rows(pool: &PgPool, id: Uuid) -> i64 {
        let (rows,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM oauth_authorizations WHERE id = $1")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap();
        rows
    }

    /// Whether authorization `id` is still stored unused.
    async fn authorization_unused(pool: &PgPool, id: Uuid) -> bool {
        let (used,): (bool,) =
            sqlx::query_as("SELECT used FROM oauth_authorizations WHERE id = $1")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap();
        !used
    }

    #[test]
    fn default_ttl_matches_the_config_default() {
        assert_eq!(
            DEFAULT_TOKEN_TTL.as_secs(),
            u64::try_from(DEFAULT_OAUTH_TOKEN_TTL_SECS).unwrap(),
            "the model's fallback and OAUTH_TOKEN_TTL_SECS' default agree"
        );
    }

    #[tokio::test]
    async fn create_token_round_trips_and_validates() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert_user(&pool, "alice").await;
        let app = insert_app(&pool, "Client").await;
        let token = create_token(
            &pool,
            &NewToken {
                scopes: "read write follow".to_owned(),
                ..new_token(Some(alice.id), app.id)
            },
        )
        .await
        .unwrap();

        assert!(token.id.get_version_num() == 7, "id is a UUIDv7");
        assert!(
            token.token.len() == 43 && is_url_safe(&token.token),
            "the access token is a 43-char URL-safe random token"
        );
        assert!(
            token.refresh_token.len() == 43 && is_url_safe(&token.refresh_token),
            "the refresh token is one too"
        );
        assert_ne!(token.token, token.refresh_token, "the two tokens differ");
        assert_eq!(
            token.user_id,
            Some(alice.id),
            "the user binding round trips"
        );
        assert_eq!(token.app_id, app.id, "the app binding round trips");
        assert_eq!(token.scopes, "read write follow", "scopes round trip");
        let days = DEFAULT_TOKEN_TTL.as_secs();
        let remaining = token
            .valid_until
            .signed_duration_since(Utc::now())
            .num_seconds();
        assert!(
            (i64::try_from(days).unwrap() - 60..=i64::try_from(days).unwrap() + 60)
                .contains(&remaining),
            "the default lifetime is about 30 days out, got {remaining}s"
        );
        assert!(
            token.inserted_at <= token.updated_at,
            "the database stamps both timestamps"
        );

        let (found, user) = get_valid_token(&pool, &token.token)
            .await
            .unwrap()
            .expect("a fresh token is valid");
        assert_eq!(found.id, token.id, "the token is found by its text");
        assert_eq!(found.token, token.token, "the secret round trips");
        assert_eq!(
            user.map(|loaded| loaded.id),
            Some(alice.id),
            "the owning user is loaded with it"
        );
        let by_refresh = get_by_refresh_token(&pool, &token.refresh_token)
            .await
            .unwrap()
            .expect("the refresh token finds the row");
        assert_eq!(by_refresh.id, token.id, "same row, other credential");
        let listed = list_for_user(&pool, alice.id).await.unwrap();
        assert_eq!(listed.len(), 1, "the user's list holds the token");
        assert_eq!(listed.first().map(|t| t.id), Some(token.id), "same row");
    }

    #[tokio::test]
    async fn custom_ttl_is_applied() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app = insert_app(&pool, "Dial-Down").await;
        let token = create_token(
            &pool,
            &NewToken {
                ttl: Some(Duration::from_secs(60)),
                ..new_token(None, app.id)
            },
        )
        .await
        .unwrap();
        let remaining = token
            .valid_until
            .signed_duration_since(Utc::now())
            .num_seconds();
        assert!(
            (30..=70).contains(&remaining),
            "a one-minute ttl expires a minute out, got {remaining}s"
        );
    }

    #[tokio::test]
    async fn app_only_tokens_validate_without_a_user() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app = insert_app(&pool, "Machine").await;
        let token = create_token(&pool, &new_token(None, app.id)).await.unwrap();
        let (found, user) = get_valid_token(&pool, &token.token)
            .await
            .unwrap()
            .expect("a client_credentials token is valid on its own");
        assert_eq!(found.id, token.id, "the row comes back");
        assert!(user.is_none(), "there is no user to load");
        let nobody = new_id();
        assert!(
            list_for_user(&pool, nobody).await.unwrap().is_empty(),
            "an app-only token belongs to no user's list"
        );
    }

    #[tokio::test]
    async fn expired_tokens_are_not_returned() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert_user(&pool, "carol").await;
        let app = insert_app(&pool, "Stale").await;
        let token = create_token(&pool, &new_token(Some(alice.id), app.id))
            .await
            .unwrap();
        expire_token(&pool, token.id).await;
        assert!(
            get_valid_token(&pool, &token.token)
                .await
                .unwrap()
                .is_none(),
            "a token past valid_until is refused"
        );
        assert!(
            get_by_refresh_token(&pool, &token.refresh_token)
                .await
                .unwrap()
                .is_some(),
            "the raw lookup still sees the row (expiry is the caller's call)"
        );
    }

    #[tokio::test]
    async fn deactivated_users_tokens_are_not_returned() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dave = insert_user(&pool, "dave").await;
        let app = insert_app(&pool, "Suspension").await;
        let token = create_token(&pool, &new_token(Some(dave.id), app.id))
            .await
            .unwrap();
        assert!(
            get_valid_token(&pool, &token.token)
                .await
                .unwrap()
                .is_some(),
            "freshly issued, the token works"
        );
        deactivate_user(&pool, dave.id).await;
        assert!(
            get_valid_token(&pool, &token.token)
                .await
                .unwrap()
                .is_none(),
            "a suspended user's bearer token stops working"
        );
    }

    #[tokio::test]
    async fn get_with_app_returns_the_stored_rows_verbatim() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let nina = insert_user(&pool, "nina").await;
        let app = insert_app(&pool, "Bearer").await;
        let token = create_token(
            &pool,
            &NewToken {
                scopes: "write follow".to_owned(),
                ..new_token(Some(nina.id), app.id)
            },
        )
        .await
        .unwrap();

        let (found, found_app) = get_with_app(&pool, &token.token)
            .await
            .unwrap()
            .expect("a stored token is found with its app");
        assert_eq!(found.id, token.id, "the token row comes back");
        assert_eq!(found.token, token.token, "the secret round trips");
        assert_eq!(found.scopes, "write follow", "the token keeps its scopes");
        assert_eq!(found_app.id, app.id, "the app id is the join target");
        assert_eq!(found_app.client_name, app.client_name, "name round trips");
        assert_eq!(
            found_app.redirect_uris, app.redirect_uris,
            "redirect uris round trip"
        );
        assert_eq!(
            found_app.scopes, app.scopes,
            "the app reads its own aliased scopes, not the token's"
        );
        assert_eq!(found_app.website, app.website, "website round trips");
        assert_eq!(found_app.client_id, app.client_id, "client id round trips");
        assert_eq!(
            found_app.client_secret, app.client_secret,
            "the whole app row arrives"
        );
        assert_eq!(found_app.trusted, app.trusted, "trusted round trips");
        assert_eq!(
            found_app.user_id, app.user_id,
            "the app reads its own NULL owner, not the token's user"
        );
        assert_eq!(
            found_app.inserted_at, app.inserted_at,
            "the app keeps its own timestamps"
        );

        // App-only tokens join their app just the same.
        let app_only = create_token(&pool, &new_token(None, app.id)).await.unwrap();
        let (_, app_of_it) = get_with_app(&pool, &app_only.token)
            .await
            .unwrap()
            .expect("a client_credentials token is found too");
        assert_eq!(app_of_it.id, app.id, "same issuing app");

        assert!(
            get_with_app(&pool, "no-such-token")
                .await
                .unwrap()
                .is_none(),
            "an unknown access token finds nothing"
        );
    }

    #[tokio::test]
    async fn get_with_app_applies_no_validity_filter() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let oscar = insert_user(&pool, "oscar").await;
        let app = insert_app(&pool, "Unfiltered").await;
        let token = create_token(&pool, &new_token(Some(oscar.id), app.id))
            .await
            .unwrap();
        assert!(
            get_with_app(&pool, &token.token).await.unwrap().is_some(),
            "fresh, it is found"
        );
        expire_token(&pool, token.id).await;
        assert!(
            get_with_app(&pool, &token.token).await.unwrap().is_some(),
            "an expired row still arrives (expiry is the caller's call)"
        );
        assert!(
            get_valid_token(&pool, &token.token)
                .await
                .unwrap()
                .is_none(),
            "whereas the validity-filtered lookup hides it"
        );
        deactivate_user(&pool, oscar.id).await;
        assert!(
            get_with_app(&pool, &token.token).await.unwrap().is_some(),
            "and a deactivated owner hides it neither"
        );
    }

    #[tokio::test]
    async fn unknown_credentials_are_none() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            get_valid_token(&pool, "no-such-token")
                .await
                .unwrap()
                .is_none(),
            "an unknown access token finds nothing"
        );
        assert!(
            get_by_refresh_token(&pool, "no-such-refresh")
                .await
                .unwrap()
                .is_none(),
            "an unknown refresh token finds nothing"
        );
        assert!(
            consume_authorization(&pool, "no-such-code", new_id())
                .await
                .unwrap()
                .is_none(),
            "an unknown code finds nothing"
        );
        // Deleting nothing is not an error.
        delete_token(&pool, new_id()).await.unwrap();
    }

    #[tokio::test]
    async fn delete_token_removes_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let erin = insert_user(&pool, "erin").await;
        let app = insert_app(&pool, "Revoke").await;
        let token = create_token(&pool, &new_token(Some(erin.id), app.id))
            .await
            .unwrap();
        delete_token(&pool, token.id).await.unwrap();
        assert_eq!(token_rows(&pool, token.id).await, 0, "the row is gone");
        assert!(
            get_valid_token(&pool, &token.token)
                .await
                .unwrap()
                .is_none(),
            "a deleted token no longer validates"
        );
    }

    #[tokio::test]
    async fn delete_expired_sweeps_only_expired_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let frank = insert_user(&pool, "frank").await;
        let app = insert_app(&pool, "Janitor").await;
        let stale = create_token(&pool, &new_token(Some(frank.id), app.id))
            .await
            .unwrap();
        let also_stale = create_token(&pool, &new_token(Some(frank.id), app.id))
            .await
            .unwrap();
        let fresh = create_token(&pool, &new_token(Some(frank.id), app.id))
            .await
            .unwrap();
        expire_token(&pool, stale.id).await;
        expire_token(&pool, also_stale.id).await;

        let swept = delete_expired(&pool).await.unwrap();
        assert_eq!(swept, 2, "exactly the two expired rows went");
        assert_eq!(
            token_rows(&pool, stale.id).await,
            0,
            "the stale row is gone"
        );
        assert_eq!(
            token_rows(&pool, fresh.id).await,
            1,
            "the fresh row survived the sweep"
        );

        let nothing = delete_expired(&pool).await.unwrap();
        assert_eq!(
            nothing, 0,
            "a second sweep with nothing due deletes zero rows"
        );
    }

    #[tokio::test]
    async fn list_for_user_only_shows_that_users_tokens() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let grace = insert_user(&pool, "grace").await;
        let heidi = insert_user(&pool, "heidi").await;
        let app = insert_app(&pool, "Listed").await;
        let mine = create_token(&pool, &new_token(Some(grace.id), app.id))
            .await
            .unwrap();
        let expired = create_token(&pool, &new_token(Some(grace.id), app.id))
            .await
            .unwrap();
        expire_token(&pool, expired.id).await;
        let hers = create_token(&pool, &new_token(Some(heidi.id), app.id))
            .await
            .unwrap();

        let listed = list_for_user(&pool, grace.id).await.unwrap();
        assert_eq!(listed.len(), 2, "both of grace's tokens, expired included");
        let ids: Vec<Uuid> = listed.iter().map(|token| token.id).collect();
        assert!(
            ids.contains(&mine.id) && ids.contains(&expired.id),
            "grace's rows are the ones listed"
        );
        assert!(
            !ids.contains(&hers.id),
            "heidi's token is not in grace's list"
        );
        assert!(
            ids.first().is_some_and(|first| *first == mine.id),
            "creation order (decision 4): the older row leads"
        );
        assert!(
            list_for_user(&pool, new_id()).await.unwrap().is_empty(),
            "an unknown user has no tokens"
        );
    }

    #[tokio::test]
    async fn deleting_a_user_cascades_to_tokens_and_codes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let ivan = insert_user(&pool, "ivan").await;
        let app = insert_app(&pool, "Cascade").await;
        let token = create_token(&pool, &new_token(Some(ivan.id), app.id))
            .await
            .unwrap();
        let authorization = create_authorization(&pool, ivan.id, app.id, "read")
            .await
            .unwrap();
        assert_eq!(token_rows(&pool, token.id).await, 1, "fixtures exist");
        assert_eq!(
            authorization_rows(&pool, authorization.id).await,
            1,
            "fixtures exist"
        );

        delete_user(&pool, ivan.id).await;
        assert_eq!(
            token_rows(&pool, token.id).await,
            0,
            "the user's tokens followed the account"
        );
        assert_eq!(
            authorization_rows(&pool, authorization.id).await,
            0,
            "and so did the pending code"
        );
    }

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
    async fn debug_redacts_tokens_and_codes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mia = insert_user(&pool, "mia").await;
        let app = insert_app(&pool, "Loose-Lips").await;
        let token = create_token(&pool, &new_token(Some(mia.id), app.id))
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
        assert!(
            debug.contains(r#"token: "<redacted>""#)
                && debug.contains(r#"refresh_token: "<redacted>""#),
            "both redaction markers name their field: {debug}"
        );
        assert!(
            debug.contains(&token.scopes),
            "the rest of the row is visible"
        );

        let authorization = create_authorization(&pool, mia.id, app.id, "read")
            .await
            .unwrap();
        let auth_debug = format!("{authorization:?}");
        assert!(
            !auth_debug.contains(&authorization.token),
            "the authorization code must not leak"
        );
        assert!(
            auth_debug.contains(r#"token: "<redacted>""#),
            "the redaction marker names the field"
        );
        assert!(auth_debug.contains("read"), "the rest shows");
    }

    #[tokio::test]
    async fn list_valid_with_app_names_skips_expired_and_sorts_newest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let grace = insert_user(&pool, "grace").await;
        let heidi = insert_user(&pool, "heidi").await;
        let first_app = insert_app(&pool, "First App").await;
        let second_app = insert_app(&pool, "Second App").await;
        let older = create_token(&pool, &new_token(Some(grace.id), first_app.id))
            .await
            .unwrap();
        let stale = create_token(&pool, &new_token(Some(grace.id), second_app.id))
            .await
            .unwrap();
        expire_token(&pool, stale.id).await;
        let newest = create_token(&pool, &new_token(Some(grace.id), second_app.id))
            .await
            .unwrap();
        let hers = create_token(&pool, &new_token(Some(heidi.id), first_app.id))
            .await
            .unwrap();
        let app_only = create_token(&pool, &new_token(None, first_app.id))
            .await
            .unwrap();

        let listed = list_valid_with_app_names(&pool, grace.id).await.unwrap();
        let ids: Vec<Uuid> = listed.iter().map(|row| row.id).collect();
        assert_eq!(
            ids,
            vec![newest.id, older.id],
            "own unexpired rows only, newest first (decision 4)"
        );
        assert!(
            !ids.contains(&hers.id) && !ids.contains(&stale.id) && !ids.contains(&app_only.id),
            "foreign, expired and app-only tokens stay out of the list"
        );
        let first = listed.first().unwrap();
        assert_eq!(first.app_name, "Second App", "the joined app name");
        assert!(
            first.valid_until > Utc::now(),
            "what comes back is still valid"
        );
        assert_eq!(
            listed.get(1).map(|row| row.app_name.as_str()),
            Some("First App"),
            "each row carries its own app's name"
        );
        assert!(
            list_valid_with_app_names(&pool, new_id())
                .await
                .unwrap()
                .is_empty(),
            "an unknown user has no valid tokens"
        );
    }

    #[tokio::test]
    async fn delete_for_user_only_removes_the_owners_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let grace = insert_user(&pool, "grace").await;
        let heidi = insert_user(&pool, "heidi").await;
        let app = insert_app(&pool, "Revoke").await;
        let mine = create_token(&pool, &new_token(Some(grace.id), app.id))
            .await
            .unwrap();
        let theirs = create_token(&pool, &new_token(Some(heidi.id), app.id))
            .await
            .unwrap();
        let app_only = create_token(&pool, &new_token(None, app.id)).await.unwrap();

        assert!(
            !delete_for_user(&pool, grace.id, theirs.id).await.unwrap(),
            "a foreign token is not the caller's to delete"
        );
        assert_eq!(token_rows(&pool, theirs.id).await, 1, "and stays");
        assert!(
            !delete_for_user(&pool, grace.id, app_only.id).await.unwrap(),
            "an app-only token belongs to no user"
        );
        assert!(
            !delete_for_user(&pool, grace.id, new_id()).await.unwrap(),
            "an unknown id deletes nothing"
        );
        assert!(
            delete_for_user(&pool, grace.id, mine.id).await.unwrap(),
            "the own row goes"
        );
        assert_eq!(token_rows(&pool, mine.id).await, 0, "gone for good");
    }
}
