//! Which apps count as the first-party frontend's own (WBE-822).
//!
//! `POST /oauth/authorize` approves only for a token whose app is
//! `trusted`. A frontend registers its own app at runtime, per browser,
//! through `POST /api/v1/apps`, so no admin can mark it by hand. The rule
//! here trusts such an app by itself: **every `redirect_uri` is on the
//! origin (scheme, host and port) of `FRONTEND_BASE_URL`.** An
//! out-of-band URI does not qualify, nor does a custom scheme.
//!
//! Codes for such an app can only be delivered to the frontend itself,
//! and its tokens come from a login on the frontend, so trusting it grants
//! nothing a logged-in user does not already hold.

use sqlx::PgPool;
use url::Url;
use uuid::Uuid;

use crate::error::AppResult;

/// The origin of `url` when it is an `http(s)` URL with a host.
fn tuple_origin(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }
    Some(parsed.origin().ascii_serialization())
}

/// Whether `redirect_uris` (whitespace separated) is non-empty and every
/// entry lies on the origin of `frontend_base_url`.
///
/// A `frontend_base_url` that is not an `http(s)` URL trusts nothing.
#[must_use]
pub fn on_frontend_origin(redirect_uris: &str, frontend_base_url: &str) -> bool {
    let Some(frontend) = tuple_origin(frontend_base_url) else {
        return false;
    };
    let mut entries = redirect_uris.split_whitespace().peekable();
    entries.peek().is_some()
        && entries.all(|uri| tuple_origin(uri).as_deref() == Some(frontend.as_str()))
}

/// The apps not yet trusted, with their stored `redirect_uris`.
const UNTRUSTED_SQL: &str = "SELECT id, redirect_uris FROM oauth_apps WHERE NOT trusted";

/// Marks the app behind each id trusted.
const TRUST_SQL: &str = "UPDATE oauth_apps SET trusted = true, updated_at = now()
   WHERE id = ANY($1) AND NOT trusted";

/// Marks every existing app that [`on_frontend_origin`] accepts as trusted
/// and returns how many rows changed.
///
/// Run once at boot, after the migrations, so apps registered before the
/// rule existed (or under another `FRONTEND_BASE_URL`) catch up.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn trust_frontend_apps(pool: &PgPool, frontend_base_url: &str) -> AppResult<u64> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(UNTRUSTED_SQL).fetch_all(pool).await?;
    let ids: Vec<Uuid> = rows
        .into_iter()
        .filter(|(_, uris)| on_frontend_origin(uris, frontend_base_url))
        .map(|(id, _)| id)
        .collect();
    if ids.is_empty() {
        return Ok(0);
    }
    let changed = sqlx::query(TRUST_SQL)
        .bind(&ids)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::oauth_app::{self, NewOauthApp};

    /// The frontend the tests trust.
    const FRONTEND: &str = "https://fe.example";

    #[test]
    fn frontend_origin_uris_qualify() {
        for uris in [
            "https://fe.example/",
            "https://fe.example/oauth/done",
            "https://fe.example/a\nhttps://fe.example/b",
            "https://FE.example:443/x",
        ] {
            assert!(on_frontend_origin(uris, FRONTEND), "`{uris}` qualifies");
        }
        assert!(
            on_frontend_origin("https://fe.example/cb", "https://fe.example/app/"),
            "the frontend path is ignored"
        );
    }

    #[test]
    fn other_origins_oob_and_empty_do_not_qualify() {
        for uris in [
            "",
            "   ",
            "urn:ietf:wg:oauth:2.0:oob",
            "https://elsewhere.example/cb",
            "http://fe.example/cb",
            "https://fe.example:8443/cb",
            "https://fe.example.evil.example/cb",
            "https://fe.example/a https://elsewhere.example/b",
            "https://fe.example/a urn:ietf:wg:oauth:2.0:oob",
            "myapp://fe.example/cb",
        ] {
            assert!(!on_frontend_origin(uris, FRONTEND), "`{uris}` does not");
        }
    }

    #[test]
    fn a_frontend_url_without_an_http_origin_trusts_nothing() {
        assert!(!on_frontend_origin("https://fe.example/", "not a url"));
        assert!(!on_frontend_origin("myapp://x/", "myapp://x/"));
    }

    /// Registers an app with `uris` as its redirect URIs.
    async fn register(pool: &PgPool, uris: &str) -> oauth_app::OauthApp {
        oauth_app::create(
            pool,
            &NewOauthApp {
                client_name: "App".to_owned(),
                redirect_uris: uris.to_owned(),
                scopes: "read".to_owned(),
                website: None,
                user_id: None,
            },
        )
        .await
        .expect("app inserts")
    }

    /// Whether the stored app `id` is trusted.
    async fn is_trusted(pool: &PgPool, id: Uuid) -> bool {
        oauth_app::get_by_id(pool, id)
            .await
            .expect("lookup")
            .expect("row")
            .trusted
    }

    #[tokio::test]
    async fn the_backfill_trusts_only_matching_apps() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let inside = register(&pool, "https://fe.example/").await;
        let mixed = register(&pool, "https://fe.example/ https://other.example/").await;
        let oob = register(&pool, "urn:ietf:wg:oauth:2.0:oob").await;

        let changed = trust_frontend_apps(&pool, FRONTEND)
            .await
            .expect("the backfill runs");
        assert_eq!(changed, 1, "one app matched");
        assert!(is_trusted(&pool, inside.id).await, "the frontend app");
        assert!(!is_trusted(&pool, mixed.id).await, "a mixed app is not");
        assert!(!is_trusted(&pool, oob.id).await, "an oob app is not");
        let again = trust_frontend_apps(&pool, FRONTEND)
            .await
            .expect("a second run");
        assert_eq!(again, 0, "nothing left to change");
    }
}
