//! The `oauth_apps` table: client applications registered through
//! `POST /api/v1/apps`.
//!
//! The reference shape is Pleroma's `App` schema
//! (`lib/pleroma/web/o_auth/app.ex`): a name, its redirect URIs stored as
//! one space- or newline-separated string, a scope string, optional
//! website, the credential pair and the `trusted` flag. Per the ticket
//! `user_id` is a plain uuid without a foreign key yet — the owner is
//! only known once OAuth authentication exists, and user-unbound app
//! tokens keep it `NULL`.
//!
//! [`create`] mints the primary key (`UUIDv7`, decision 4) and the
//! credential pair itself, mirroring Pleroma's `register_changeset`;
//! callers supply only the registration fields. `client_secret` is
//! secret material: the manual `Debug` impl redacts it, so it can never
//! reach a log line.

use std::fmt;

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

/// One row of `oauth_apps`: a registered client application.
#[derive(sqlx::FromRow)]
pub struct OauthApp {
    /// `UUIDv7` primary key, minted by [`create`] (decision 4).
    pub id: Uuid,
    /// Human-facing application name, as the client sent it.
    pub client_name: String,
    /// Registered redirect URIs, space- or newline-separated exactly as
    /// stored.
    pub redirect_uris: String,
    /// Granted scopes, space-separated (see
    /// [`crate::oauth::scopes::Scopes`]).
    pub scopes: String,
    /// Application homepage, if set.
    pub website: Option<String>,
    /// Public OAuth client id: a 43-character URL-safe random token.
    pub client_id: String,
    /// OAuth client secret. Never in `Debug` output.
    pub client_secret: String,
    /// Whether the app skips the authorization confirmation screen.
    pub trusted: bool,
    /// Owning user; `None` for user-unbound registrations. A plain uuid,
    /// no foreign key yet.
    pub user_id: Option<Uuid>,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed.
    pub updated_at: DateTime<Utc>,
}

/// [`client_secret`](Self::client_secret) is secret material and renders
/// as `<redacted>`; everything else shows.
impl fmt::Debug for OauthApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OauthApp")
            .field("id", &self.id)
            .field("client_name", &self.client_name)
            .field("redirect_uris", &self.redirect_uris)
            .field("scopes", &self.scopes)
            .field("website", &self.website)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("trusted", &self.trusted)
            .field("user_id", &self.user_id)
            .field("inserted_at", &self.inserted_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// What [`create`] inserts, besides what it mints itself (`id`,
/// `client_id`, `client_secret`) and what the database stamps
/// (`trusted`'s default, the timestamps).
#[derive(Clone, Debug)]
pub struct NewOauthApp {
    /// Application name to store.
    pub client_name: String,
    /// Redirect URIs in their stored spelling (space/newline separated).
    pub redirect_uris: String,
    /// Scopes as one space-separated string.
    pub scopes: String,
    /// Optional homepage URL.
    pub website: Option<String>,
    /// Owner; `None` unless an authenticated registration binds the app.
    pub user_id: Option<Uuid>,
}

/// The `INSERT` behind [`create`]: every registration column is bound
/// from a [`NewOauthApp`] plus the minted id and credentials, with
/// `RETURNING *` so the row comes back with its database-side defaults
/// and timestamps.
const INSERT_SQL: &str = "INSERT INTO oauth_apps (
     id, client_name, redirect_uris, scopes, website, client_id,
     client_secret, user_id
) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *";

/// Inserts an app registration and returns the stored row.
///
/// The id is a fresh `UUIDv7` (decision 4) and the credential pair two
/// independent 43-character random tokens (Pleroma's
/// `register_changeset`); `trusted` takes the column default `false`.
/// `client_id` is unique, so a credential collision is rejected.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(pool: &PgPool, new_app: &NewOauthApp) -> AppResult<OauthApp> {
    let app = sqlx::query_as::<_, OauthApp>(INSERT_SQL)
        .bind(new_id())
        .bind(new_app.client_name.as_str())
        .bind(new_app.redirect_uris.as_str())
        .bind(new_app.scopes.as_str())
        .bind(new_app.website.as_deref())
        .bind(random_token(CREDENTIAL_BYTES))
        .bind(random_token(CREDENTIAL_BYTES))
        .bind(new_app.user_id)
        .fetch_one(pool)
        .await?;
    Ok(app)
}

/// Fetches the app whose OAuth client id is `client_id`, `None` when no
/// row matches. The comparison is exact (plain `text`).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_client_id(pool: &PgPool, client_id: &str) -> AppResult<Option<OauthApp>> {
    let app = sqlx::query_as::<_, OauthApp>("SELECT * FROM oauth_apps WHERE client_id = $1")
        .bind(client_id)
        .fetch_optional(pool)
        .await?;
    Ok(app)
}

/// Fetches the app whose primary key is `id`, `None` when no row
/// matches.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> AppResult<Option<OauthApp>> {
    let app = sqlx::query_as::<_, OauthApp>("SELECT * FROM oauth_apps WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(app)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    /// A [`NewOauthApp`] named `name` with one redirect URI and `read`.
    fn new_app(name: &str) -> NewOauthApp {
        NewOauthApp {
            client_name: name.to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
            scopes: "read".to_owned(),
            website: None,
            user_id: None,
        }
    }

    /// Whether every character is in the URL-safe base64 alphabet.
    fn is_url_safe(token: &str) -> bool {
        token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn create_round_trips_every_column(pool: PgPool) {
        let owner = new_id();
        let app = create(
            &pool,
            &NewOauthApp {
                client_name: "My Client".to_owned(),
                redirect_uris: "https://a.example/cb\nhttps://b.example/cb".to_owned(),
                scopes: "read write follow".to_owned(),
                website: Some("https://client.example".to_owned()),
                user_id: Some(owner),
            },
        )
        .await
        .unwrap();
        assert!(app.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(app.client_name, "My Client", "name round trips");
        assert_eq!(
            app.redirect_uris, "https://a.example/cb\nhttps://b.example/cb",
            "the stored redirect_uris string survives verbatim"
        );
        assert_eq!(app.scopes, "read write follow", "scopes round trip");
        assert_eq!(
            app.website.as_deref(),
            Some("https://client.example"),
            "website round trips"
        );
        assert!(
            app.client_id.len() == 43 && is_url_safe(&app.client_id),
            "client_id is a 43-char URL-safe token, got `{}`",
            app.client_id
        );
        assert!(
            app.client_secret.len() == 43 && is_url_safe(&app.client_secret),
            "client_secret is a 43-char URL-safe token"
        );
        assert_ne!(app.client_id, app.client_secret, "the tokens differ");
        assert!(!app.trusted, "trusted takes the column default");
        assert_eq!(app.user_id, Some(owner), "the owner uuid round trips");
        assert!(
            app.inserted_at <= app.updated_at,
            "the database stamps both timestamps"
        );

        let by_id = get_by_id(&pool, app.id).await.unwrap().expect("by id");
        let by_client_id = get_by_client_id(&pool, &app.client_id)
            .await
            .unwrap()
            .expect("by client_id");
        assert_eq!(by_id.client_secret, app.client_secret, "same row");
        assert_eq!(by_client_id.id, app.id, "the client id finds the same row");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn defaults_apply_when_only_the_required_fields_are_set(pool: PgPool) {
        let app = create(&pool, &new_app("Plain")).await.unwrap();
        assert_eq!(app.scopes, "read", "the caller-provided default");
        assert!(app.website.is_none(), "website stays NULL");
        assert!(app.user_id.is_none(), "an unauthenticated app has no owner");
        assert!(!app.trusted, "registrations are not trusted");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn unknown_lookups_are_none(pool: PgPool) {
        assert!(
            get_by_id(&pool, new_id()).await.unwrap().is_none(),
            "an unknown id finds nothing"
        );
        assert!(
            get_by_client_id(&pool, "no-such-client")
                .await
                .unwrap()
                .is_none(),
            "an unknown client id finds nothing"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn every_registration_gets_its_own_credentials(pool: PgPool) {
        let first = create(&pool, &new_app("First")).await.unwrap();
        let second = create(&pool, &new_app("First")).await.unwrap();
        assert_eq!(first.client_name, second.client_name, "names may repeat");
        assert_ne!(first.id, second.id, "ids differ");
        assert_ne!(first.client_id, second.client_id, "client ids differ");
        assert_ne!(
            first.client_secret, second.client_secret,
            "client secrets differ"
        );
        assert_eq!(
            first.client_id.len(),
            random_token(CREDENTIAL_BYTES).len(),
            "both credentials use the fixed length"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn debug_redacts_the_client_secret(pool: PgPool) {
        let app = create(&pool, &new_app("Secretive")).await.unwrap();
        let debug = format!("{app:?}");
        assert!(
            !debug.contains(&app.client_secret),
            "the client secret must not leak"
        );
        assert!(
            debug.contains(r#"client_secret: "<redacted>""#),
            "the redaction marker names the field"
        );
        assert!(
            debug.contains(&app.client_id),
            "the public client id still shows"
        );
        assert!(
            debug.contains("Secretive"),
            "the rest of the row is visible"
        );
    }
}
