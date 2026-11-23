//! The `oauth_tokens` and `oauth_authorizations` tables: issued bearer
//! credentials and the one-time authorization codes they are exchanged
//! from.
//!
//! The reference shapes are Pleroma's `Token` and `Authorization`
//! schemas (`lib/pleroma/web/o_auth/token.ex`,
//! `lib/pleroma/web/o_auth/authorization.ex`). A token row carries an
//! access `token` and a `refresh_token` — both 43-character URL-safe
//! random tokens from [`random_token`](crate::util::token::random_token) — and grants `scopes` until
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
use uuid::Uuid;

/// The one-time authorization codes and their exchange.
mod authorizations;
/// Deleting tokens: by id, the expired sweep, per user and the revocations.
mod deletes;
/// Listing a user's tokens, with and without their app names.
mod listing;
/// The fixtures the children's tests share.
#[cfg(test)]
mod test_support;
/// Issuing tokens and the lookups by credential, id and refresh token.
mod tokens;
/// The token lookup that joins the issuing application.
mod with_app;

pub use authorizations::{
    OauthAuthorization, consume_authorization, create_authorization, get_authorization_by_id,
};
pub use deletes::{
    delete_all_for_user, delete_all_for_user_except, delete_expired, delete_for_user, delete_token,
};
pub use listing::{TokenWithAppName, list_for_user, list_valid_with_app_names};
pub use tokens::{create_token, get_by_id, get_by_refresh_token, get_valid_token};
pub use with_app::get_with_app;

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

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::{config::DEFAULT_OAUTH_TOKEN_TTL_SECS, util::id::new_id};

    #[test]
    fn default_ttl_matches_the_config_default() {
        assert_eq!(
            DEFAULT_TOKEN_TTL.as_secs(),
            u64::try_from(DEFAULT_OAUTH_TOKEN_TTL_SECS).unwrap(),
            "the model's fallback and OAUTH_TOKEN_TTL_SECS' default agree"
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
            consume_authorization(&pool, "no-such-code", new_id(), None)
                .await
                .unwrap()
                .is_none(),
            "an unknown code finds nothing"
        );
        // Deleting nothing is not an error.
        delete_token(&pool, new_id()).await.unwrap();
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

        let authorization = create_authorization(&pool, mia.id, app.id, "read", "")
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
}
