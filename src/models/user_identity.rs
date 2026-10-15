//! The `user_identities` table: which provider account logs in as which
//! local user (`docs/ARCHITECTURE.md` decision 9).
//!
//! The reference is Pleroma's `Registration` schema
//! (`lib/pleroma/registration.ex`), the row its Ueberauth callback looks up
//! by `(provider, ouid)` before it hands a returning account an
//! authorization code. Here the pair is the same fact under the ticket's
//! `provider`/`uid` names, one row per provider account (the
//! `UNIQUE (provider, uid)` column pair), pointing at a user the
//! `ON DELETE CASCADE` foreign key revokes the link with.
//!
//! Nothing here knows about HTTP: [`find_user`] is the lookup the
//! `/oauth/{provider}/callback` runs, [`link`] the write the registration
//! flow performs.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use crate::{error::AppResult, models::user::User, util::id::new_id};

/// One row of `user_identities`: a provider account and the local user it
/// belongs to.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct UserIdentity {
    /// `UUIDv7` primary key, minted by [`link`] (decision 4).
    pub id: Uuid,
    /// The account the provider identity logs in as.
    pub user_id: Uuid,
    /// Provider name, matching an `OAUTH_PROVIDERS` entry verbatim.
    pub provider: String,
    /// The provider's own stable account id, as text.
    pub uid: String,
    /// When the link was made.
    pub inserted_at: DateTime<Utc>,
}

/// [`find_user`]: the LOCAL user of one provider account. A link to a
/// remote actor (which nothing in this server creates, but see the test
/// below) is not a login, and the join is what makes a link whose user has
/// since been deleted unreadable here as well as gone from the table.
const FIND_USER_SQL: &str = "SELECT u.* FROM user_identities i
     JOIN users u ON u.id = i.user_id
    WHERE i.provider = $1 AND i.uid = $2 AND u.local";

/// [`link`]: the identity row itself, minted id included.
const LINK_SQL: &str = "INSERT INTO user_identities (id, user_id, provider, uid)
     VALUES ($1, $2, $3, $4)
  RETURNING *";

/// The local account `uid` at `provider` logs in as, `None` when no row
/// links that pair to a local user.
///
/// Activity is not this query's business: the caller's account-readiness
/// check owns it, so a deactivated account is refused as itself instead of
/// reading as "never linked".
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn find_user(pool: &PgPool, provider: &str, uid: &str) -> AppResult<Option<User>> {
    let found = sqlx::query_as::<_, User>(FIND_USER_SQL)
        .bind(provider)
        .bind(uid)
        .fetch_optional(pool)
        .await?;
    Ok(found)
}

/// Links the provider account `uid` at `provider` to `user_id` and returns
/// the stored row.
///
/// The id is a fresh `UUIDv7` (decision 4); `inserted_at` is the database's.
/// A pair that is already linked is rejected by the table's
/// `UNIQUE (provider, uid)`, which arrives as a database failure: moving a
/// provider account from one user to another is not this write's business.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn link(
    pool: &PgPool,
    user_id: Uuid,
    provider: &str,
    uid: &str,
) -> AppResult<UserIdentity> {
    let made = sqlx::query_as::<_, UserIdentity>(LINK_SQL)
        .bind(new_id())
        .bind(user_id)
        .bind(provider)
        .bind(uid)
        .fetch_one(pool)
        .await?;
    debug!(
        provider,
        uid,
        user = %made.user_id,
        "external identity linked"
    );
    Ok(made)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        error::AppError,
        models::user::{self, Flag, NewUser, local_ap_id},
    };
    use sqlx::PgPool;

    /// The provider and id the tests link.
    const PROVIDER: &str = "github";
    /// See [`PROVIDER`].
    const UID: &str = "4242";

    /// A local user named `nickname`, straight through the user model.
    async fn create_user(pool: &PgPool, nickname: &str) -> User {
        let config = Config::for_tests();
        user::insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(&config.base_url, nickname)),
        )
        .await
        .expect("the user fixture inserts")
    }

    /// A mirror of a remote actor named `nickname` on `host`.
    async fn create_remote_user(pool: &PgPool, nickname: &str) -> User {
        let ap_id = format!("https://remote.example/users/{nickname}");
        user::insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@remote.example"),
                ap_id: ap_id.clone(),
                domain: Some("remote.example".to_owned()),
                local: Flag::new(false),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// A fresh table answers `None`: nothing is linked to anything.
    #[tokio::test]
    async fn find_user_of_an_unknown_pair_is_none() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            find_user(&pool, PROVIDER, UID)
                .await
                .expect("the lookup reaches the database")
                .is_none(),
            "a fresh table links nobody"
        );
    }

    /// [`link`] then [`find_user`] round-trips: the stored row carries the
    /// pair it was made from, the lookup answers that user's row.
    #[tokio::test]
    async fn link_and_find_user_round_trip() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let identity = link(&pool, alice.id, PROVIDER, UID)
            .await
            .expect("the link inserts");
        assert_eq!(identity.user_id, alice.id, "the linked user");
        assert_eq!(identity.provider, PROVIDER, "the provider");
        assert_eq!(identity.uid, UID, "the provider account id");
        assert_ne!(identity.id, Uuid::nil(), "the id is minted");

        let found = find_user(&pool, PROVIDER, UID)
            .await
            .expect("the lookup reaches the database")
            .expect("the pair is linked");
        assert_eq!(found.id, alice.id, "the lookup answers the user row");
        assert_eq!(found.nickname, "alice", "and it is a whole user");

        assert!(
            find_user(&pool, "keycloak", UID)
                .await
                .expect("the lookup runs")
                .is_none(),
            "another provider's same account id is a different login"
        );
        assert!(
            find_user(&pool, PROVIDER, "4243")
                .await
                .expect("the lookup runs")
                .is_none(),
            "another account id of the same provider is too"
        );
    }

    /// One pair is one row: the `UNIQUE (provider, uid)` constraint refuses a
    /// second link, so a provider account cannot move between users here.
    #[tokio::test]
    async fn a_linked_pair_cannot_be_linked_again() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let bob = create_user(&pool, "bob").await;
        let first = link(&pool, alice.id, PROVIDER, UID)
            .await
            .expect("the first link inserts");
        assert_eq!(first.user_id, alice.id, "the first link stands");
        let err = link(&pool, bob.id, PROVIDER, UID)
            .await
            .expect_err("the pair is taken");
        assert!(
            matches!(err, AppError::Database(_)),
            "a constraint violation is a database failure, got {err}"
        );
        let found = find_user(&pool, PROVIDER, UID)
            .await
            .expect("the lookup runs")
            .expect("the first link stands");
        assert_eq!(found.id, alice.id, "the refused write changed nothing");
    }

    /// Deleting the user takes the link with it (the cascade), which is how
    /// a closed account stops being reachable by a provider login.
    #[tokio::test]
    async fn deleting_a_user_unlinks_their_identities() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = create_user(&pool, "alice").await;
        let made = link(&pool, alice.id, PROVIDER, UID)
            .await
            .expect("the link inserts");
        assert_eq!(made.user_id, alice.id, "the link names the user");
        let gone = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the delete runs");
        assert_eq!(gone.rows_affected(), 1, "the user row went away");

        assert!(
            find_user(&pool, PROVIDER, UID)
                .await
                .expect("the lookup runs")
                .is_none(),
            "and so does the link"
        );
        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM user_identities")
            .fetch_one(&pool)
            .await
            .expect("the count runs");
        assert_eq!(rows.0, 0, "the cascade left no orphan row");
    }

    /// A link to a remote actor is not a login: [`find_user`] answers the
    /// local accounts only, which is the pair of facts the callback's
    /// "matches a local active user" gate asks for (activity is
    /// `check_account_ready`'s half).
    #[tokio::test]
    async fn a_link_to_a_remote_actor_is_not_a_login() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let remote = create_remote_user(&pool, "octocat").await;
        let made = link(&pool, remote.id, PROVIDER, UID)
            .await
            .expect("the link inserts");
        assert_eq!(made.user_id, remote.id, "the row is stored");
        assert!(
            find_user(&pool, PROVIDER, UID)
                .await
                .expect("the lookup reaches the database")
                .is_none(),
            "the row exists but names no local account"
        );
        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM user_identities")
            .fetch_one(&pool)
            .await
            .expect("the count runs");
        assert_eq!(rows.0, 1, "the stored row is what the lookup refused");
    }
}
