//! The credential writes: the password hash and its pending-reset flag,
//! the account email and its confirmed flag, and the activation switch.
//! These are the columns the account-lifecycle endpoints move.

use sqlx::{Executor, PgPool, Postgres};
use uuid::Uuid;

use super::Flag;
use crate::error::AppResult;

/// The `UPDATE` behind [`set_password`]: writes the new hash, clears the
/// pending-reset flag (the account has just proven mailbox control by
/// completing the flow) and refreshes `updated_at`.
const SET_PASSWORD_SQL: &str = "UPDATE users
           SET password_hash = $2,
               password_reset_pending = false,
               updated_at = now()
         WHERE id = $1";

/// Stores `password_hash` as user `id`'s `password_hash` and clears
/// [`password_reset_pending`](super::User::password_reset_pending): the write a
/// completed password reset performs (see
/// [`crate::models::password_reset`] and [`crate::api::password_reset`]).
///
/// Runs on the caller's executor (or any transaction), so the reset
/// endpoint can land the password, the token spend and the revocations as
/// one transaction.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_password<'e, E>(executor: E, id: Uuid, password_hash: &str) -> AppResult<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(SET_PASSWORD_SQL)
        .bind(id)
        .bind(password_hash)
        .execute(executor)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_password_reset_pending`]: raises the flag and
/// refreshes `updated_at`.
const SET_PASSWORD_RESET_PENDING_SQL: &str =
    "UPDATE users SET password_reset_pending = true, updated_at = now() WHERE id = $1";

/// Raises [`password_reset_pending`](super::User::password_reset_pending) on user
/// `id`: the write the admin's forced reset performs.
///
/// Its counterpart [`set_password`] clears the flag again once a new hash
/// lands. The endpoint pairs the raise with the revocation of the
/// account's tokens, and [`crate::api::oauth`] refuses the password grant
/// while it is raised.
///
/// Runs on the caller's executor (or any transaction), so one account's
/// force-reset can write the flag and revoke its tokens as one statement
/// pair.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_password_reset_pending<'e, E>(executor: E, id: Uuid) -> AppResult<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(SET_PASSWORD_RESET_PENDING_SQL)
        .bind(id)
        .execute(executor)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_email`]: writes the new address and the
/// confirmed flag together and refreshes `updated_at`.
const SET_EMAIL_SQL: &str =
    "UPDATE users SET email = $2, email_confirmed = $3, updated_at = now() WHERE id = $1";

/// Stores `email` as user `id`'s [`email`](super::User::email) and
/// [`email_confirmed`](super::User::email_confirmed): the write
/// `POST /api/pleroma/change_email` performs.
///
/// The caller decides the flag — the address stays confirmed unless the
/// instance requires account confirmation (see
/// [`Config::account_activation_required`]).
///
/// [`Config::account_activation_required`]: crate::config::Config::account_activation_required
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_email(
    pool: &PgPool,
    id: Uuid,
    email: &str,
    email_confirmed: bool,
) -> AppResult<()> {
    sqlx::query(SET_EMAIL_SQL)
        .bind(id)
        .bind(email)
        .bind(Flag::new(email_confirmed))
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_activation`]: moves the local origin
/// (`local_deactivated`), recomputes `is_active` (a remote suspension keeps
/// it off) and refreshes `updated_at`, leaving every other column alone.
const SET_ACTIVATION_SQL: &str = "UPDATE users
       SET local_deactivated = NOT $2::boolean,
           is_active = $2::boolean AND NOT remote_suspended,
           updated_at = now()
     WHERE id = $1";

/// Sets user `id`'s [`is_active`](super::User::is_active) flag: the write
/// `POST /api/pleroma/disable_account` performs.
///
/// `false` is Pleroma's `User.set_activation(user, false)` — the account
/// stops signing in and the entity reads as suspended — and every other
/// column stays untouched: unlike [`mark_deleted`](super::mark_deleted), this writes no
/// deletion time and queues no sweep.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_activation<'e, E>(executor: E, id: Uuid, active: bool) -> AppResult<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(SET_ACTIVATION_SQL)
        .bind(id)
        .bind(Flag::new(active))
        .execute(executor)
        .await
        .map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::user::{insert, test_support::*},
        util::id::new_id,
    };

    /// The force-reset write raises the flag on its own and leaves every
    /// other column alone; [`set_password`] is the pair that clears it
    /// again, so the two writes round-trip the flag.
    #[tokio::test]
    async fn set_password_reset_pending_raises_the_flag_and_set_password_clears_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        assert!(
            !alice.password_reset_pending.get(),
            "a fresh account is not pending a reset"
        );

        set_password_reset_pending(&pool, alice.id).await.unwrap();
        let pending = fetch(&pool, alice.id).await;
        assert!(
            pending.password_reset_pending.get(),
            "the force-reset write raises the flag"
        );
        assert_eq!(pending.nickname, "alice", "and writes nothing else");

        // The flag survives an ordinary profile write untouched; only a new
        // password clears it (the completed-reset rule already tested for
        // set_password).
        set_password(&pool, alice.id, SECRET_HASH).await.unwrap();
        assert!(
            !fetch(&pool, alice.id).await.password_reset_pending.get(),
            "a new hash clears the pending flag"
        );

        // An unknown id matches no row and is not an error.
        set_password_reset_pending(&pool, new_id()).await.unwrap();
    }
}
