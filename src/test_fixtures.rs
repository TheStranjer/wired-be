//! Shared fixtures for the crate's unit tests: the user rows and JSON helpers
//! that every model's test module used to copy for itself.
//!
//! Compiled for the crate's own tests and for the `integration-tests`
//! feature, like [`crate::test_db`].

use serde_json::Value;

#[cfg(test)]
use chrono::Utc;
#[cfg(test)]
use sqlx::PgPool;
#[cfg(test)]
use uuid::Uuid;

use crate::{
    config::TEST_BASE_URL,
    models::user::{NewUser, local_ap_id},
};
#[cfg(test)]
use crate::{
    models::{
        activitypub_actor::{self, ActivityPubActor, ActivityPubActorWrite},
        user::{User, insert},
    },
    util::keys::KeyPair,
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

/// Writes `write` as the account's `activitypub_actors` row, for a test that needs the identity.
///
/// Only the crate's own unit tests compile it, like [`local_user`].
///
/// # Panics
///
/// Panics if the write fails, as a fixture's failure fails its test.
#[cfg(test)]
pub async fn write_actor(pool: &PgPool, write: &ActivityPubActorWrite) -> ActivityPubActor {
    activitypub_actor::upsert(pool, write)
        .await
        .expect("the actor row writes")
}

/// Gives the account an `activitypub_actors` row stamped as fetched a moment ago.
///
/// The state a lookup serves from the database without the network.
///
/// Only the crate's own unit tests compile it, like [`local_user`].
///
/// # Panics
///
/// Panics if the write fails, as a fixture's failure fails its test.
#[cfg(test)]
pub async fn fetched_just_now(pool: &PgPool, user_id: Uuid) -> ActivityPubActor {
    write_actor(
        pool,
        &ActivityPubActorWrite {
            last_fetched_at: Some(Utc::now()),
            ..ActivityPubActorWrite::bare(user_id)
        },
    )
    .await
}

/// Inserts a mirror of the remote actor `nickname` with the inboxes and key a fetched actor has.
///
/// The `activitypub_actors` row names the personal inbox
/// `https://remote.example/users/{nickname}/inbox`, the shared inbox
/// `https://remote.example/inbox` and the public key `PUB`.
///
/// # Panics
///
/// Panics if either insert fails, as a fixture's failure fails its test.
#[cfg(test)]
pub async fn federating_remote(pool: &PgPool, nickname: &str) -> User {
    let user = remote_user(pool, nickname).await;
    let _row = write_actor(
        pool,
        &ActivityPubActorWrite {
            inbox: Some(format!("https://{REMOTE_HOST}/users/{nickname}/inbox")),
            shared_inbox: Some(format!("https://{REMOTE_HOST}/inbox")),
            public_key: Some("PUB".to_owned()),
            ..ActivityPubActorWrite::bare(user.id)
        },
    )
    .await;
    user
}

/// Inserts a local user named `nickname` with the signing keys and endpoints a federating account has.
///
/// The `activitypub_actors` row carries a placeholder key pair and the endpoints a registration derives
/// (see [`ActivityPubActorWrite::for_local_account`]).
///
/// # Panics
///
/// Panics if either insert fails, as a fixture's failure fails its test.
#[cfg(test)]
pub async fn federating_user(pool: &PgPool, nickname: &str) -> User {
    let user = local_user(pool, nickname).await;
    let keys = KeyPair {
        private_pem: "PEM".to_owned(),
        public_pem: "PUB".to_owned(),
    };
    let _row = write_actor(
        pool,
        &ActivityPubActorWrite::for_local_account(&user, TEST_BASE_URL, keys),
    )
    .await;
    user
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
