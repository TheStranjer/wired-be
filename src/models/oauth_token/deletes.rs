//! Deleting tokens: one by id, one a user owns, and the revocation of every token of a user with or without sparing the
//! credential that asked.

use sqlx::{Executor, PgPool, Postgres};
use uuid::Uuid;

use crate::error::AppResult;

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

/// Deletes every token belonging to user `user_id` (expired included,
/// app-only rows excluded — their `user_id` is `NULL`), reporting how
/// many went.
///
/// This is Pleroma's `OAuth.Token.Query.delete_by_user`, the revocation a
/// completed password reset runs on the account: no bearer credential the
/// account ever issued may outlive the password change.
///
/// Runs on the caller's executor (or any transaction), so the reset
/// endpoint can land the revocation with the password write.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_all_for_user<'e, E>(executor: E, user_id: Uuid) -> AppResult<u64>
where
    E: Executor<'e, Database = Postgres>,
{
    let deleted = sqlx::query("DELETE FROM oauth_tokens WHERE user_id = $1")
        .bind(user_id)
        .execute(executor)
        .await?;
    Ok(deleted.rows_affected())
}

/// The `DELETE` behind [`delete_all_for_user_except`]: every token of the
/// user except the row `keep_id` names.
const DELETE_ALL_FOR_USER_EXCEPT_SQL: &str =
    "DELETE FROM oauth_tokens WHERE user_id = $1 AND id <> $2";

/// Deletes all tokens belonging to user `user_id` except the row
/// `keep_id`, reporting how many went.
///
/// This is the revocation `POST /api/pleroma/change_password` runs —
/// Pleroma's `Token.Revoke.all_for_user`, minus the credential the request
/// itself authenticated with, which must keep working after the password
/// change.
///
/// `keep_id` names no user-bound row (a foreign id, an app-only token)
/// and everything of the user still goes, so the `<>` can only spare the
/// caller's own credential. Runs on the caller's executor (or any
/// transaction), so the password write and the revocation land together.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_all_for_user_except<'e, E>(
    executor: E,
    user_id: Uuid,
    keep_id: Uuid,
) -> AppResult<u64>
where
    E: Executor<'e, Database = Postgres>,
{
    let deleted = sqlx::query(DELETE_ALL_FOR_USER_EXCEPT_SQL)
        .bind(user_id)
        .bind(keep_id)
        .execute(executor)
        .await?;
    Ok(deleted.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::{create_authorization, create_token, get_with_app};
    use super::*;
    use crate::util::id::new_id;

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
            get_with_app(&pool, &token.token).await.unwrap().is_none(),
            "a deleted token is no longer found"
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
        let authorization = create_authorization(&pool, ivan.id, app.id, "read", "")
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

    /// `delete_all_for_user_except`: every token of the user goes but the
    /// kept row, and neither a foreign user's nor an app-only token is
    /// ever touched, however the keep id is spelled.
    #[tokio::test]
    async fn delete_all_for_user_except_spares_the_kept_row_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let grace = insert_user(&pool, "grace").await;
        let heidi = insert_user(&pool, "heidi").await;
        let app = insert_app(&pool, "Rotate").await;
        let keeper = create_token(&pool, &new_token(Some(grace.id), app.id))
            .await
            .unwrap();
        let other = create_token(&pool, &new_token(Some(grace.id), app.id))
            .await
            .unwrap();
        let theirs = create_token(&pool, &new_token(Some(heidi.id), app.id))
            .await
            .unwrap();
        let app_only = create_token(&pool, &new_token(None, app.id)).await.unwrap();

        let revoked = delete_all_for_user_except(&pool, grace.id, keeper.id)
            .await
            .unwrap();
        assert_eq!(revoked, 1, "only grace's second token went");
        assert_eq!(
            token_rows(&pool, keeper.id).await,
            1,
            "the kept row survives"
        );
        assert_eq!(
            token_rows(&pool, other.id).await,
            0,
            "the other one is gone"
        );
        assert_eq!(token_rows(&pool, theirs.id).await, 1, "heidi keeps hers");
        assert_eq!(
            token_rows(&pool, app_only.id).await,
            1,
            "app-only rows are nobody's"
        );

        let unknown = new_id();
        assert_eq!(
            delete_all_for_user_except(&pool, grace.id, unknown)
                .await
                .unwrap(),
            1,
            "a keep id of no user-bound row spares nothing"
        );
        assert_eq!(
            token_rows(&pool, keeper.id).await,
            0,
            "and the whole set goes"
        );
    }
}
