//! Shared fixtures for the crate's unit tests: the user rows and JSON helpers
//! that every model's test module used to copy for itself.
//!
//! Compiled for the crate's own tests and for the `integration-tests`
//! feature, like [`crate::test_db`].

use serde_json::Value;

#[cfg(test)]
use sqlx::PgPool;
#[cfg(test)]
use uuid::Uuid;

#[cfg(test)]
use crate::models::{
    activitypub_actor::{self, ActivityPubActor},
    user::{User, get_by_id, insert},
};
use crate::{
    config::TEST_BASE_URL,
    models::user::{NewUser, local_ap_id},
};

/// The host the remote fixtures mirror their actors from.
const REMOTE_HOST: &str = "remote.example";

/// A local [`NewUser`] named `nickname` on the test instance
/// ([`TEST_BASE_URL`]), for fixtures that adjust a field before inserting.
#[must_use]
pub fn local_new(nickname: &str) -> NewUser {
    NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
}

/// Inserts a local user named `nickname` on the test instance.
///
/// Only the crate's own unit tests compile it: the `expect` that fails a test
/// on a broken insert is denied in the library build the
/// `integration-tests` feature produces.
///
/// # Panics
///
/// Panics if the insert fails, as a fixture's failure fails its test.
#[cfg(test)]
pub async fn local_user(pool: &PgPool, nickname: &str) -> User {
    insert(pool, &local_new(nickname))
        .await
        .expect("the local fixture inserts")
}

/// A [`NewUser`] mirroring the remote actor `nickname` on `remote.example`.
///
/// Its stored nickname is `nickname@remote.example` (decision 7), its `ap_id`
/// `https://remote.example/users/{nickname}` and its `domain` the host, for
/// fixtures that adjust a field before inserting.
#[must_use]
pub fn remote_new(nickname: &str) -> NewUser {
    NewUser {
        nickname: format!("{nickname}@{REMOTE_HOST}"),
        ap_id: format!("https://{REMOTE_HOST}/users/{nickname}"),
        domain: Some(REMOTE_HOST.to_owned()),
        ..NewUser::default()
    }
}

/// Inserts a mirror of the remote actor `nickname` on `remote.example`, as
/// [`remote_new`] shapes it.
///
/// Compiled for the crate's own unit tests only, like [`local_user`].
///
/// # Panics
///
/// Panics if the insert fails, as a fixture's failure fails its test.
#[cfg(test)]
pub async fn remote_user(pool: &PgPool, nickname: &str) -> User {
    insert(pool, &remote_new(nickname))
        .await
        .expect("the remote fixture inserts")
}

/// Asserts an account's actor row holds the moved columns of its `users` row.
///
/// Returns the actor row. The check behind every test of the temporary dual write (see
/// [`crate::models::activitypub_actor`]): `inbox`, `shared_inbox`, `outbox`,
/// `followers_url`, `following_url`, `featured_url`, `public_key`,
/// `private_key`, `public_key_id` and `last_fetched_at` compare equal.
///
/// # Panics
///
/// Panics when either row is missing or any column differs.
#[cfg(test)]
pub async fn assert_actor_in_step(pool: &PgPool, user_id: Uuid) -> ActivityPubActor {
    let user = get_by_id(pool, user_id)
        .await
        .expect("the user reads")
        .expect("the user exists");
    let key_id =
        sqlx::query_scalar::<_, Option<String>>("SELECT public_key_id FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(pool)
            .await
            .expect("the key id reads");
    let actor = activitypub_actor::get(pool, user_id)
        .await
        .expect("the actor row reads")
        .expect("the account has an actor row");
    assert_eq!(actor.inbox, user.inbox, "inbox");
    assert_eq!(actor.shared_inbox, user.shared_inbox, "shared_inbox");
    assert_eq!(actor.outbox, user.outbox, "outbox");
    assert_eq!(actor.followers_url, user.followers_url, "followers_url");
    assert_eq!(actor.following_url, user.following_url, "following_url");
    assert_eq!(actor.featured_url, user.featured_url, "featured_url");
    assert_eq!(actor.public_key, user.public_key, "public_key");
    assert_eq!(actor.private_key, user.private_key, "private_key");
    assert_eq!(actor.public_key_id, key_id, "public_key_id");
    assert_eq!(
        actor.last_fetched_at, user.last_fetched_at,
        "last_fetched_at"
    );
    actor
}

/// Copies the moved columns of a `users` row onto its `activitypub_actors` row.
///
/// For a fixture that sets those columns with a raw `UPDATE users`, so the
/// two tables stay in step the way the writers keep them.
///
/// # Panics
///
/// Panics when the account has no actor row to update.
#[cfg(test)]
pub async fn resync_actor(pool: &PgPool, user_id: Uuid) {
    let copied = sqlx::query(
        "UPDATE activitypub_actors AS a
            SET inbox = u.inbox, shared_inbox = u.shared_inbox, outbox = u.outbox,
                followers_url = u.followers_url, following_url = u.following_url,
                featured_url = u.featured_url, public_key = u.public_key,
                private_key = u.private_key, public_key_id = u.public_key_id,
                last_fetched_at = u.last_fetched_at
           FROM users AS u
          WHERE u.id = a.user_id AND u.id = $1",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .expect("the actor row updates")
    .rows_affected();
    assert_eq!(copied, 1, "the account has one actor row to update");
}

/// The keys of the JSON object `value`, sorted: the exact key-set
/// assertion the shape tests pair with. A value that is not an object has
/// no keys.
#[must_use]
pub fn json_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .into_iter()
        .flat_map(|object| object.keys().map(String::as_str))
        .collect();
    keys.sort_unstable();
    keys
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn local_user_inserts_a_local_row_at_the_test_origin() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();

        let user = local_user(pool, "alice").await;

        assert!(user.local.get(), "the row is local");
        assert_eq!(user.nickname, "alice", "nickname");
        assert_eq!(user.ap_id, "https://test.example/users/alice", "ap_id");
        assert!(user.domain.is_none(), "a local row has no domain");
    }

    #[tokio::test]
    async fn remote_user_inserts_a_mirror_with_its_domain() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();

        let user = remote_user(pool, "bob").await;

        assert!(!user.local.get(), "the row is remote");
        assert_eq!(user.nickname, "bob@remote.example", "nickname");
        assert_eq!(user.ap_id, "https://remote.example/users/bob", "ap_id");
        assert_eq!(user.domain.as_deref(), Some("remote.example"), "domain");
    }

    #[test]
    fn local_new_is_a_local_account_on_the_test_origin() {
        let new = local_new("carol");

        assert!(new.local.get(), "marked local");
        assert_eq!(new.nickname, "carol", "nickname");
        assert_eq!(new.ap_id, "https://test.example/users/carol", "ap_id");
    }

    #[test]
    fn remote_new_is_a_mirror_on_the_remote_host() {
        let new = remote_new("dora");

        assert!(!new.local.get(), "marked remote");
        assert_eq!(new.nickname, "dora@remote.example", "nickname");
        assert_eq!(new.ap_id, "https://remote.example/users/dora", "ap_id");
        assert_eq!(new.domain.as_deref(), Some("remote.example"), "domain");
    }

    #[test]
    fn object_keys_come_back_sorted() {
        let value = json!({"zeta": 1, "alpha": {"inner": 2}, "mid": null});

        assert_eq!(
            json_keys(&value),
            vec!["alpha", "mid", "zeta"],
            "keys sorted"
        );
    }

    #[test]
    fn an_empty_object_has_no_keys() {
        assert!(json_keys(&json!({})).is_empty(), "no keys");
    }

    #[test]
    fn a_non_object_has_no_keys() {
        assert!(json_keys(&json!([1, 2])).is_empty(), "an array has no keys");
    }

    #[tokio::test]
    #[should_panic(expected = "the local fixture inserts")]
    async fn a_failed_insert_fails_the_test_with_its_label() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let _first = local_user(pool, "dup").await;

        let _second = local_user(pool, "dup").await;
    }
}
