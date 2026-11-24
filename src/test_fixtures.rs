//! Shared fixtures for the crate's unit tests: the user rows and JSON helpers
//! that every model's test module used to copy for itself.
//!
//! Compiled for the crate's own tests and for the `integration-tests`
//! feature, like [`crate::test_db`].

use serde_json::Value;

#[cfg(test)]
use sqlx::PgPool;

#[cfg(test)]
use crate::models::user::{User, insert};
use crate::{
    config::TEST_BASE_URL,
    models::user::{NewUser, local_ap_id},
};

/// The host the remote fixtures mirror their actors from.
#[cfg(test)]
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

/// Inserts a mirror of the remote actor `nickname` on `remote.example`: its
/// stored nickname is `nickname@remote.example` (decision 7), its `ap_id`
/// `https://remote.example/users/{nickname}` and its `domain` the host.
///
/// Compiled for the crate's own unit tests only, like [`local_user`].
///
/// # Panics
///
/// Panics if the insert fails, as a fixture's failure fails its test.
#[cfg(test)]
pub async fn remote_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@{REMOTE_HOST}"),
            ap_id: format!("https://{REMOTE_HOST}/users/{nickname}"),
            domain: Some(REMOTE_HOST.to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the remote fixture inserts")
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
    fn json_keys_sorts_the_object_keys() {
        let value = json!({"zeta": 1, "alpha": {"inner": 2}, "mid": null});

        assert_eq!(
            json_keys(&value),
            vec!["alpha", "mid", "zeta"],
            "keys sorted"
        );
    }

    #[test]
    fn json_keys_of_an_empty_object_is_empty() {
        assert!(json_keys(&json!({})).is_empty(), "no keys");
    }

    #[test]
    fn json_keys_of_a_non_object_is_empty() {
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
