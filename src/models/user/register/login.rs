//! Checking a submitted password against the stored hash.
//! [`authenticate`] is the strict gate every ordinary login path wants:
//! local, active and the right password. [`authenticate_any_status`] is
//! the same check without the active-user rule, for flows that report a
//! deactivated account themselves (the OAuth password grant).
//!
//! A lookup that found no row, and an account with no hash to check, are
//! answered with a verification against [`DUMMY_PASSWORD_HASH`] so neither
//! is cheaply distinguishable from a wrong password; a password that
//! matches a legacy (bcrypt / PBKDF2) hash is re-stored as Argon2id on the
//! way out, by [`upgrade_password_hash`].

use sqlx::PgPool;
use tracing::warn;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::user::User,
    util::password::{hash_password, needs_rehash, verify_password},
};

/// A valid Argon2id PHC string of a password nobody will ever type, used
/// only by [`verify_dummy_password`] so a lookup that found no row still
/// pays the cost of one real verification.
const DUMMY_PASSWORD_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$/uRDL24D8QHL5NRLmAficA$Eoza8QsFsJDqfi2oMa05IbXRgPgZ7Kb4t6nxTSc4sIs";

/// The login lookup: local rows at any account status, matched by citext
/// nickname or email against the single bind `$1`. Whether a found account
/// is ready to receive a token is the caller's decision
/// ([`authenticate`] demands an active one; the OAuth password grant runs
/// its own, richer check); a remote mirror is never a candidate.
const LOGIN_LOOKUP_SQL: &str = "SELECT * FROM users
     WHERE local
       AND (nickname = $1::citext OR email = $1::citext)";

/// Verifies `password` for the local, active user whose nickname or email
/// is `nickname_or_email`, returning the row on success.
///
/// A remote mirror, a deactivated account, an unknown identifier, an
/// account without a password hash or simply the wrong password all answer
/// `None`. This is the strict gate every ordinary login path wants; the
/// OAuth password grant instead authenticates with
/// [`authenticate_any_status`] and reports a deactivated account as a
/// distinct `account_is_disabled` error, so it does not route that case
/// through here.
pub async fn authenticate(pool: &PgPool, nickname_or_email: &str, password: &str) -> Option<User> {
    let user = authenticate_any_status(pool, nickname_or_email, password).await?;
    // The credential was correct; now enforce the ordinary active-user rule.
    user.is_active.get().then_some(user)
}

/// Verifies `password` for the local user whose nickname or email is
/// `nickname_or_email`, regardless of whether that account is active,
/// returning the row on success.
///
/// This is [`authenticate`] minus the active-user gate, for callers that
/// must distinguish a wrong password from a deactivated account (the OAuth
/// password grant answers the former with `Invalid credentials` and the
/// latter with `account_is_disabled`). A remote mirror, an unknown
/// identifier, an account without a password hash and the wrong password
/// all answer `None`; unknown identifiers and hashless accounts are checked
/// against a built-in dummy hash so they cost about the same as verifying a
/// wrong password, and a password that matches a legacy hash
/// ([`needs_rehash`]) is transparently re-stored as Argon2id before the row
/// is returned. A deactivated account whose password matches is returned
/// with its `is_active` flag `false`; the caller decides what that means.
pub async fn authenticate_any_status(
    pool: &PgPool,
    nickname_or_email: &str,
    password: &str,
) -> Option<User> {
    let Some(mut user) = find_login_candidate(pool, nickname_or_email).await else {
        verify_dummy_password(password);
        return None;
    };
    let Some(stored) = user.password_hash.as_deref() else {
        verify_dummy_password(password);
        return None;
    };
    if !verify_password(password, stored) {
        return None;
    }
    if needs_rehash(stored) {
        upgrade_password_hash(pool, &mut user, password).await;
    }
    Some(user)
}

/// The login lookup: the local row whose nickname or email matches
/// `identifier` (citext folds case in both), whatever the account's
/// status. A database failure is logged and answered `None`, like an
/// unknown account.
async fn find_login_candidate(pool: &PgPool, identifier: &str) -> Option<User> {
    let found = sqlx::query_as::<_, User>(LOGIN_LOOKUP_SQL)
        .bind(identifier)
        .fetch_optional(pool)
        .await;
    match found {
        Ok(user) => user,
        Err(err) => {
            warn!(%err, "login lookup failed");
            None
        }
    }
}

/// Runs one real Argon2id verification against [`DUMMY_PASSWORD_HASH`].
/// Its result is meaningless — the dummy hash verifies nothing a caller
/// submitted — only its work is wanted, so a login attempt against an
/// unknown account is not cheaply distinguishable from a wrong password.
fn verify_dummy_password(password: &str) {
    if verify_password(password, DUMMY_PASSWORD_HASH) {
        warn!("the dummy verification hash matched a submitted password");
    }
}

/// Upgrades `user`'s verified legacy hash to Argon2id in the row and in the
/// returned struct. A failure here never revokes a successful login; it is
/// only logged, and the user keeps their (working) legacy hash.
async fn upgrade_password_hash(pool: &PgPool, user: &mut User, password: &str) {
    match hash_password(password) {
        Ok(new_hash) => match set_password_hash(pool, user.id, &new_hash).await {
            Ok(()) => user.password_hash = Some(new_hash),
            Err(err) => warn!(%err, "could not store the upgraded password hash"),
        },
        Err(err) => warn!(%err, "could not rehash the verified password"),
    }
}

/// Writes a user's `password_hash` and refreshes `updated_at`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
async fn set_password_hash(pool: &PgPool, id: Uuid, hash: &str) -> AppResult<()> {
    sqlx::query("UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(hash)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::user::{Flag, NewUser, get_by_id, insert, register::test_support::*};

    /// A bcrypt `$2b$` hash of [`PASSWORD`] (the same vector
    /// `src/util/password.rs` verifies), for the rehash test.
    const BCRYPT_2B: &str = "$2b$12$28E9Mpz84hFJrMMxMmid6.2qqJZdjG.YOLhO5OjHkMJ2Lmm4AaGr2";

    /// Sets a user's `is_active` flag directly, for the login tests.
    async fn set_active(pool: &PgPool, id: Uuid, active: bool) {
        let result = sqlx::query("UPDATE users SET is_active = $2 WHERE id = $1")
            .bind(id)
            .bind(Flag::new(active))
            .execute(pool)
            .await
            .expect("the flag update runs");
        assert_eq!(result.rows_affected(), 1, "the row exists to change");
    }

    #[tokio::test]
    async fn authenticate_accepts_anyone_it_should_and_no_one_else() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hash = hash_password(PASSWORD).expect("the fixture hash is made");
        let alice = insert_login(&pool, "alice", "alice@example.org", Some(&hash)).await;

        let by_nickname = authenticate(&pool, "alice", PASSWORD)
            .await
            .expect("the nickname and password log in");
        assert_eq!(by_nickname.id, alice.id, "the row itself");
        assert!(
            authenticate(&pool, "ALICE", PASSWORD).await.is_some(),
            "the nickname lookup is citext"
        );
        assert!(
            authenticate(&pool, "ALICE@EXAMPLE.ORG", PASSWORD)
                .await
                .is_some(),
            "the email lookup is citext too"
        );
        assert!(
            authenticate(&pool, "alice", "wrong password")
                .await
                .is_none(),
            "the wrong password never logs in"
        );
        assert!(
            authenticate(&pool, "nobody", PASSWORD).await.is_none(),
            "an unknown identifier never logs in (dummy-hash path)"
        );
        assert!(
            authenticate(&pool, "", PASSWORD).await.is_none(),
            "an empty identifier never logs in"
        );

        let carol = insert_login(&pool, "carol", "carol@example.org", None).await;
        assert!(
            carol.password_hash.is_none(),
            "the hashless fixture exists to be tested"
        );
        assert!(
            authenticate(&pool, "carol", PASSWORD).await.is_none(),
            "an account without a hash cannot log in by password"
        );

        set_active(&pool, alice.id, false).await;
        assert!(
            authenticate(&pool, "alice", PASSWORD).await.is_none(),
            "a deactivated account never logs in"
        );
        set_active(&pool, alice.id, true).await;
        assert!(
            authenticate(&pool, "alice", PASSWORD).await.is_some(),
            "reactivation lets it in again"
        );

        // A remote mirror with a hash is no candidate either: local-only.
        let bob = insert(
            &pool,
            &NewUser {
                nickname: "bob@remote.example".to_owned(),
                ap_id: "https://remote.example/users/bob".to_owned(),
                email: Some("bob@remote.example".to_owned()),
                password_hash: Some(hash.clone()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts");
        assert!(!bob.local.get(), "the fixture is a remote mirror");
        assert!(
            authenticate(&pool, "bob@remote.example", PASSWORD)
                .await
                .is_none(),
            "remote users never authenticate by password"
        );
    }

    #[tokio::test]
    async fn authenticate_any_status_returns_a_deactivated_user() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hash = hash_password(PASSWORD).expect("the fixture hash is made");
        let alice = insert_login(&pool, "henry", "henry@example.org", Some(&hash)).await;
        set_active(&pool, alice.id, false).await;
        let found = authenticate_any_status(&pool, "henry", PASSWORD)
            .await
            .expect("the password of a deactivated account still verifies");
        assert_eq!(found.id, alice.id, "the row itself comes back");
        assert!(
            !found.is_active.get(),
            "and it still reports the account as inactive"
        );
        assert!(
            authenticate_any_status(&pool, "henry", "wrong password")
                .await
                .is_none(),
            "but the wrong password never logs in either"
        );
        assert!(
            authenticate_any_status(&pool, "nobody-here", PASSWORD)
                .await
                .is_none(),
            "and an unknown identifier finds nothing (dummy-hash path)"
        );
    }

    #[tokio::test]
    async fn authenticate_upgrades_a_legacy_bcrypt_hash() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = insert_login(&pool, "dave", "dave@example.org", Some(BCRYPT_2B)).await;
        assert!(needs_rehash(BCRYPT_2B), "the fixture is a legacy hash");
        let logged_in = authenticate(&pool, "dave", PASSWORD)
            .await
            .expect("the legacy hash logs in");
        assert!(
            !needs_rehash(
                logged_in
                    .password_hash
                    .as_deref()
                    .expect("the returned user carries the upgrade")
            ),
            "the returned row reports the current hash"
        );

        let stored = get_by_id(&pool, user.id)
            .await
            .unwrap()
            .expect("the user exists");
        let new_hash = stored.password_hash.expect("the row was upgraded");
        assert_ne!(new_hash, BCRYPT_2B, "the legacy hash is gone");
        assert!(
            new_hash.starts_with("$argon2id$"),
            "the upgrade is Argon2id"
        );
        assert!(
            verify_password(PASSWORD, &new_hash),
            "the upgraded hash verifies the same password"
        );
        assert!(
            authenticate(&pool, "dave", PASSWORD).await.is_some(),
            "logging in still works afterwards"
        );
        assert!(
            authenticate(&pool, "dave", "wrong password")
                .await
                .is_none(),
            "and still rejects wrong passwords"
        );
    }

    #[tokio::test]
    async fn authenticate_leaves_a_current_hash_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hash = hash_password(PASSWORD).expect("the fixture hash is made");
        let user = insert_login(&pool, "erin", "erin@example.org", Some(&hash)).await;
        let logged_in = authenticate(&pool, "erin", PASSWORD)
            .await
            .expect("the current hash logs in");
        assert_eq!(
            logged_in.password_hash.as_deref(),
            Some(hash.as_str()),
            "an Argon2id hash is not rehashed"
        );
        let stored = get_by_id(&pool, user.id)
            .await
            .unwrap()
            .expect("the user exists");
        assert_eq!(
            stored.password_hash.as_deref(),
            Some(hash.as_str()),
            "and the row keeps it verbatim"
        );
    }

    #[tokio::test]
    async fn authenticate_answers_none_when_the_database_breaks() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hash = hash_password(PASSWORD).expect("the fixture hash is made");
        let frank = insert_login(&pool, "frank", "frank@example.org", Some(&hash)).await;
        assert_eq!(frank.nickname, "frank", "the fixture exists");
        // CASCADE because the OAuth token and authorization tables now
        // hold foreign keys to `users`; the point of the exercise is the
        // same — no readable `users` table for the lookup to hit.
        let dropped = sqlx::query("DROP TABLE users CASCADE")
            .execute(&pool)
            .await
            .expect("the table can be dropped in a throwaway database");
        assert_eq!(dropped.rows_affected(), 0, "DROP reports no rows");
        assert!(
            authenticate(&pool, "frank", PASSWORD).await.is_none(),
            "a broken lookup is not a login"
        );
    }
}
