//! The instance actor: the local `Application` account this process signs
//! its federation fetches with and attributes server-level activities to.
//!
//! The reference is Pleroma's service actor, created on demand by
//! `Pleroma.User.get_or_create_service_actor_by_ap_id`
//! (`lib/pleroma/user.ex`) and served at the URI its
//! `ActivityPubController.internal_fetch` action renders
//! (`lib/pleroma/web/activity_pub/activity_pub_controller.ex`). Like
//! Pleroma's, this actor has no password and can never log in; unlike
//! Pleroma (which keeps the key in runtime config), its RSA pair lives in
//! its `activitypub_actors` row beside every other local actor's.
//!
//! [`ensure_instance_actor`] is the single entry point: it is idempotent,
//! called once at startup by `wired-be`'s `main` and lazily by the routes
//! and fetch paths that need the actor, so a fresh instance provisions it
//! without any manual step and a lost creation race against a concurrent
//! first use simply reads back the winner's row.

use sqlx::{Error as SqlxError, PgPool};

use crate::{
    config::Config,
    error::{AppError, AppResult},
    models::{
        activitypub_actor::{self, ActivityPubActorWrite},
        user::{Flag, INSTANCE_ACTOR_NICKNAME, NewUser, User, get_by_nickname, insert_row},
    },
    util::keys::{KeyPair, generate_keypair_async},
};

/// Path the instance actor document is served at and its actor id hangs
/// off `BASE_URL`: `/internal/fetch` (Pleroma's `internal_fetch` route).
pub const INSTANCE_ACTOR_PATH: &str = "/internal/fetch";

/// The `ActivityPub` `type` of the instance actor: an `Application`, the
/// kind of actor owned by the software rather than a person (Pleroma's
/// service actors are `Application`s).
const APPLICATION_ACTOR_TYPE: &str = "Application";

/// SQLSTATE of a Postgres unique violation.
const SQLSTATE_UNIQUE_VIOLATION: &str = "23505";

/// The `ActivityPub` actor id of the instance actor on the instance served
/// at `base_url`: `{base_url}/internal/fetch`.
#[must_use]
pub fn instance_actor_ap_id(base_url: &str) -> String {
    format!("{base_url}{INSTANCE_ACTOR_PATH}")
}

/// Returns the instance actor, creating it on first use.
///
/// The stored row answers unchanged — the actor is provisioned once and
/// never rewritten, so its key stays valid with the peers that cached its
/// document. Creation generates the RSA pair on the blocking pool (see
/// [`generate_keypair_async`]) and writes the row the way the ticket
/// prescribes: `actor_type = Application`, `locked`, undiscoverable, named
/// after `INSTANCE_NAME`, with no password.
///
/// # Errors
///
/// Any database or key-generation failure arrives as
/// [`AppError::Database`](crate::error::AppError) or
/// [`AppError::Internal`](crate::error::AppError).
pub async fn ensure_instance_actor(pool: &PgPool, config: &Config) -> AppResult<User> {
    if let Some(existing) = get_by_nickname(pool, INSTANCE_ACTOR_NICKNAME).await? {
        return Ok(existing);
    }
    create(pool, config).await
}

/// Inserts the instance actor row. A unique violation means a concurrent
/// first use won the race, and its row is the answer; any other error (or
/// the impossible case where the winner is somehow unreadable) stands.
async fn create(pool: &PgPool, config: &Config) -> AppResult<User> {
    let keys = generate_keypair_async().await?;
    match insert_with_identity(pool, config, keys).await {
        Ok(user) => Ok(user),
        Err(err) if is_unique_violation(&err) => get_by_nickname(pool, INSTANCE_ACTOR_NICKNAME)
            .await?
            .ok_or(err),
        Err(err) => Err(err),
    }
}

/// Writes the instance actor's `users` row and its `activitypub_actors` row, which carries the
/// endpoints and the key pair `keys`, in one transaction.
///
/// One transaction, so a concurrent first use never reads back a user whose identity is not there yet.
async fn insert_with_identity(pool: &PgPool, config: &Config, keys: KeyPair) -> AppResult<User> {
    let ap_id = instance_actor_ap_id(&config.base_url);
    let new_user = NewUser {
        name: config.instance_name.clone(),
        actor_type: APPLICATION_ACTOR_TYPE.to_owned(),
        locked: Flag::new(true),
        discoverable: Flag::new(false),
        ..NewUser::local(INSTANCE_ACTOR_NICKNAME, &ap_id)
    };
    let mut tx = pool.begin().await?;
    let user = insert_row(&mut tx, &new_user).await?;
    let identity = ActivityPubActorWrite::for_local_account(&user, &config.base_url, keys);
    activitypub_actor::upsert(&mut *tx, &identity)
        .await
        .map(drop)?;
    tx.commit().await?;
    Ok(user)
}

/// Whether `err` is a Postgres unique violation (SQLSTATE `23505`) — the
/// shape a lost race against the instance actor's unique `nickname` or
/// `ap_id` arrives as.
fn is_unique_violation(err: &AppError) -> bool {
    let AppError::Database(SqlxError::Database(db_err)) = err else {
        return false;
    };
    db_err.code().as_deref() == Some(SQLSTATE_UNIQUE_VIOLATION)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{NewUser, get_by_ap_id, insert, register::authenticate},
    };

    /// The `activitypub_actors` row of `user`, which creation wrote.
    async fn identity_of(pool: &PgPool, user: &User) -> activitypub_actor::ActivityPubActor {
        activitypub_actor::get(pool, user.id)
            .await
            .unwrap()
            .expect("the instance actor has an identity row")
    }

    /// Row count of `users` — the creation-race assertions watch it.
    async fn user_count(pool: &PgPool) -> i64 {
        let (count,) = sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM users")
            .fetch_one(pool)
            .await
            .unwrap();
        count
    }

    #[test]
    fn the_actor_id_hangs_off_the_base_url() {
        assert_eq!(
            instance_actor_ap_id("https://social.example"),
            "https://social.example/internal/fetch",
            "Pleroma's internal fetch URI"
        );
        assert_eq!(
            instance_actor_ap_id("http://localhost:4000"),
            "http://localhost:4000/internal/fetch",
            "a port carries through"
        );
    }

    #[tokio::test]
    async fn the_instance_actor_is_created_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let config = Config::for_tests();
        let first = ensure_instance_actor(&pool, &config).await.unwrap();
        let second = ensure_instance_actor(&pool, &config).await.unwrap();

        assert_eq!(first.id, second.id, "the second call reads the row");
        assert_eq!(
            identity_of(&pool, &second).await.public_key,
            identity_of(&pool, &first).await.public_key,
            "the stored key is never rewritten"
        );
        assert_eq!(user_count(&pool).await, 1, "exactly one row exists");
    }

    #[tokio::test]
    async fn the_created_row_is_the_locked_application() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let config = Config::for_tests();
        let actor = ensure_instance_actor(&pool, &config).await.unwrap();

        assert_eq!(
            actor.nickname, INSTANCE_ACTOR_NICKNAME,
            "the reserved nickname"
        );
        assert_eq!(
            actor.ap_id,
            format!("{TEST_BASE_URL}/internal/fetch"),
            "the actor id is the internal fetch URI"
        );
        assert!(actor.local.get(), "the instance actor is a local account");
        assert_eq!(actor.actor_type, "Application", "the ticket's actor type");
        assert!(actor.locked.get(), "nobody follows it without approval");
        assert!(
            !actor.discoverable.get(),
            "it is not offered to anyone for discovery"
        );
        assert_eq!(
            actor.name, config.instance_name,
            "its display name is the instance name"
        );
        assert!(actor.password_hash.is_none(), "it has no password");
        assert!(actor.email.is_none(), "and no email");
        let identity = identity_of(&pool, &actor).await;
        assert!(
            identity
                .public_key
                .as_deref()
                .is_some_and(|pem| pem.contains("BEGIN PUBLIC KEY")),
            "the generated public key is stored"
        );
        assert!(
            identity
                .private_key
                .as_deref()
                .is_some_and(|pem| pem.contains("BEGIN PRIVATE KEY")),
            "the generated private key is stored"
        );
        assert_eq!(
            identity.inbox.as_deref(),
            Some(format!("{TEST_BASE_URL}/internal/fetch/inbox").as_str()),
            "the inbox endpoint the later route will serve"
        );
        assert_eq!(
            identity.shared_inbox.as_deref(),
            Some(format!("{TEST_BASE_URL}/inbox").as_str()),
            "the instance-wide shared inbox"
        );

        let by_ap_id = get_by_ap_id(&pool, &actor.ap_id)
            .await
            .unwrap()
            .expect("the actor id finds the row");
        assert_eq!(by_ap_id.id, actor.id, "both lookups read one row");
    }

    #[tokio::test]
    async fn the_instance_actor_can_never_log_in() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let config = Config::for_tests();
        let _actor = ensure_instance_actor(&pool, &config).await.unwrap();

        let authenticated = authenticate(&pool, INSTANCE_ACTOR_NICKNAME, "anything").await;
        assert!(
            authenticated.is_none(),
            "a row without a password hash authenticates nobody"
        );
    }

    #[tokio::test]
    async fn a_preexisting_row_answers_untouched() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let stored = insert(
            &pool,
            &NewUser::local(
                INSTANCE_ACTOR_NICKNAME,
                &instance_actor_ap_id(TEST_BASE_URL),
            ),
        )
        .await
        .unwrap();
        let config = Config::for_tests();

        let actor = ensure_instance_actor(&pool, &config).await.unwrap();

        assert_eq!(actor.id, stored.id, "ensure never replaces an existing row");
        assert!(
            activitypub_actor::get(&pool, actor.id)
                .await
                .unwrap()
                .is_none(),
            "and never adds an identity or keys to it"
        );
    }
}
