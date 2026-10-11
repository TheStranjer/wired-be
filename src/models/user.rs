//! The `users` table: one table for local accounts and remote actors.
//!
//! Per `docs/ARCHITECTURE.md` decision 7 a local user (`alice`) and a remote
//! mirror (`alice@remote.example`) share the table, told apart by the `local`
//! flag. The reference shape is Pleroma's `User` schema
//! (`lib/pleroma/user.ex`).
//!
//! This module currently reads only the columns the bearer-auth checks and
//! the OAuth fixtures need; later tickets extend [`User`] as their endpoints
//! surface more of the row. `SELECT *` reads tolerate the undeclared columns,
//! so the row type grows without touching the queries.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

/// A Postgres `boolean` column as a named flag.
///
/// `User` carries several booleans each; Clippy's `struct_excessive_bools`
/// (denied, threshold 2) wants them modelled, not piled up, so every
/// boolean column is a `Flag`. The `sqlx` transparent derive makes it
/// encode, decode and compare exactly like the inner `bool`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct Flag(bool);

impl Flag {
    /// Wraps a raw boolean as a flag.
    #[must_use]
    pub const fn new(value: bool) -> Self {
        Self(value)
    }

    /// The raw boolean behind the flag.
    #[must_use]
    pub const fn get(self) -> bool {
        self.0
    }
}

/// A `bool` is already a valid flag value; `true.into()` sets one.
impl From<bool> for Flag {
    fn from(value: bool) -> Self {
        Self(value)
    }
}

/// One row of `users`: a local account or a mirror of a remote actor
/// (decision 7), read as far as the current queries need.
#[derive(Debug, sqlx::FromRow)]
pub struct User {
    /// `UUIDv7` primary key (decision 4).
    pub id: Uuid,
    /// Handle: `alice` locally, `alice@remote.example` for remote users.
    /// Case-insensitive (citext), unique.
    pub nickname: String,
    /// `ActivityPub` actor id, unique.
    pub ap_id: String,
    /// Whether this instance owns the account.
    pub local: Flag,
    /// Host of a remote actor; `None` for local users.
    pub domain: Option<String>,
    /// Whether the account is active (not deactivated or suspended).
    /// A deactivated account's bearer tokens stop authenticating.
    pub is_active: Flag,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed.
    pub updated_at: DateTime<Utc>,
}

/// Fetches the user whose primary key is `id`, `None` when no row
/// matches.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> AppResult<Option<User>> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::util::id::new_id;

    /// Inserts a bare local user row straight in SQL (the full
    /// registration model arrives with its own ticket) and returns its id.
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

    #[test]
    fn flags_wrap_unwrap_and_convert() {
        assert!(Flag::new(true).get(), "new/get round trip");
        assert!(!Flag::new(false).get(), "the false side too");
        assert_eq!(Flag::from(true), Flag::new(true), "From<bool> agrees");
        assert!(!Flag::default().get(), "the default flag is false");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn get_by_id_reads_the_row(pool: PgPool) {
        let id = insert_user(&pool, "alice").await;
        let user = get_by_id(&pool, id)
            .await
            .unwrap()
            .expect("the row is there");
        assert_eq!(user.id, id, "the id round trips");
        assert_eq!(user.nickname, "alice", "the handle round trips");
        assert_eq!(
            user.ap_id, "https://test.example/users/alice",
            "the actor id round trips"
        );
        assert!(user.local.get(), "a local row is local");
        assert!(user.domain.is_none(), "local users have no domain");
        assert!(user.is_active.get(), "the column default is active");
        assert!(
            user.inserted_at <= user.updated_at,
            "the database stamps both timestamps"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn get_by_id_of_an_unknown_user_is_none(pool: PgPool) {
        assert!(
            get_by_id(&pool, new_id()).await.unwrap().is_none(),
            "an unknown id finds nothing"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_flag_decodes_from_the_column(pool: PgPool) {
        let id = insert_user(&pool, "suspended").await;
        let outcome = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(outcome.rows_affected(), 1, "the update hit the row");
        let user = get_by_id(&pool, id).await.unwrap().expect("the row");
        assert!(
            !user.is_active.get(),
            "a deactivated account reads as false"
        );
    }
}
