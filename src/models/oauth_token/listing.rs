//! Listing a user's still-valid tokens with their app names:
//! [`list_valid_with_app_names`] backs `GET /api/oauth_tokens`.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

/// One row of [`list_valid_with_app_names`]: a still-valid token joined
/// with the name of the application that issued it.
#[derive(Debug, sqlx::FromRow)]
pub struct TokenWithAppName {
    /// The token's `UUIDv7` primary key (decision 4).
    pub id: Uuid,
    /// `oauth_apps.client_name` of the issuing application.
    pub app_name: String,
    /// The scopes the token was granted, space-separated.
    pub scopes: String,
    /// When the token stops being valid; always in the future.
    pub valid_until: DateTime<Utc>,
}

/// The `SELECT` behind [`list_valid_with_app_names`]: only the four
/// columns the token list renders, `JOIN`ed for the app name (the
/// `ON DELETE CASCADE` foreign key on `app_id` makes the join total),
/// unexpired rows only, and `ORDER BY t.id DESC` so the newest token
/// leads (`UUIDv7` text sorts by mint time, decision 4).
const LIST_VALID_WITH_APP_NAMES_SQL: &str =
    "SELECT t.id, a.client_name AS app_name, t.scopes, t.valid_until
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

#[cfg(test)]
mod tests {
    use super::super::create_token;
    use super::super::test_support::*;
    use super::*;
    use crate::util::id::new_id;

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
        assert_eq!(first.scopes, newest.scopes, "the granted scopes ride along");
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
}
