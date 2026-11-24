//! The access-token read that joins the issuing application:
//! [`get_with_app`] answers the token and its [`OauthApp`] exactly as
//! stored, with no validity filter.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::OauthToken;
use crate::{error::AppResult, models::oauth_app::OauthApp, util::token::sha256_hex};

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

/// The `SELECT` behind [`get_by_refresh_token`]: the same projection and
/// join as [`TOKEN_WITH_APP_SQL`], matched on the refresh token column.
const TOKEN_BY_REFRESH_WITH_APP_SQL: &str = "SELECT t.*,
        a.client_name AS app_client_name,
        a.redirect_uris AS app_redirect_uris, a.scopes AS app_scopes,
        a.website AS app_website, a.client_id AS app_client_id,
        a.client_secret AS app_client_secret, a.trusted AS app_trusted,
        a.user_id AS app_user_id, a.inserted_at AS app_inserted_at,
        a.updated_at AS app_updated_at
   FROM oauth_tokens t
   JOIN oauth_apps a ON a.id = t.app_id
  WHERE t.refresh_token = $1";

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
/// expiry and account status to the caller. [`crate::auth`] applies the
/// validity rule on top of it, telling an expired token (401) from a
/// deactivated owner (403).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_with_app(pool: &PgPool, token: &str) -> AppResult<Option<(OauthToken, OauthApp)>> {
    let row = sqlx::query_as::<_, TokenWithAppRow>(TOKEN_WITH_APP_SQL)
        .bind(sha256_hex(token))
        .fetch_optional(pool)
        .await?;
    Ok(row.map(TokenWithAppRow::into_parts))
}

/// Fetches the pair whose refresh token is `refresh_token` together with
/// the application that issued it, exactly as stored.
///
/// Like [`get_with_app`] it applies **no validity filter**: a lapsed pair
/// is still returned, since revoking it is harmless and wanted.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_refresh_token(
    pool: &PgPool,
    refresh_token: &str,
) -> AppResult<Option<(OauthToken, OauthApp)>> {
    let row = sqlx::query_as::<_, TokenWithAppRow>(TOKEN_BY_REFRESH_WITH_APP_SQL)
        .bind(sha256_hex(refresh_token))
        .fetch_optional(pool)
        .await?;
    Ok(row.map(TokenWithAppRow::into_parts))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::{NewToken, create_token};
    use super::*;

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
        assert_eq!(
            found.token,
            sha256_hex(&token.token),
            "the row holds the digest of the secret"
        );
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
    async fn get_by_refresh_token_finds_the_pair_and_its_app() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let paul = insert_user(&pool, "paul").await;
        let app = insert_app(&pool, "Refresher").await;
        let token = create_token(&pool, &new_token(Some(paul.id), app.id))
            .await
            .unwrap();

        let (found, found_app) = get_by_refresh_token(&pool, &token.refresh_token)
            .await
            .unwrap()
            .expect("a stored refresh token is found with its app");
        assert_eq!(found.id, token.id, "the pair's row comes back");
        assert_eq!(found_app.id, app.id, "joined to the issuing app");
        assert_eq!(found_app.client_id, app.client_id, "the whole app arrives");

        assert!(
            get_by_refresh_token(&pool, &token.token)
                .await
                .unwrap()
                .is_none(),
            "the access token is not a refresh token"
        );
        expire_token(&pool, token.id).await;
        assert!(
            get_by_refresh_token(&pool, &token.refresh_token)
                .await
                .unwrap()
                .is_some(),
            "no validity filter: a lapsed pair is still found"
        );
        assert!(
            get_by_refresh_token(&pool, "no-such-refresh")
                .await
                .unwrap()
                .is_none(),
            "an unknown refresh token finds nothing"
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
        deactivate_user(&pool, oscar.id).await;
        assert!(
            get_with_app(&pool, &token.token).await.unwrap().is_some(),
            "and a deactivated owner hides it neither"
        );
    }
}
