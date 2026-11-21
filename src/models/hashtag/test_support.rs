//! The fixtures the `hashtag` module's tests share: the written-name list a
//! call is made with, the storage readers that check what a write left
//! behind, and the author-and-post fixture every link and feed test starts
//! from. Test-only.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    config::TEST_BASE_URL,
    models::{
        status::{self, NewStatus, Status},
        user::{NewUser, insert as insert_user, local_ap_id},
    },
};

use super::Hashtag;

/// The instance origin the fixtures pretend to serve.
pub(super) const BASE: &str = TEST_BASE_URL;

/// The names of `hashtags`, in the order they came back.
pub(super) fn names(hashtags: &[Hashtag]) -> Vec<String> {
    hashtags
        .iter()
        .map(|hashtag| hashtag.name.clone())
        .collect()
}

/// The stored tag rows as `(name, id)`, ordered by name, read straight
/// from storage so assertions do not depend on fixture return values.
pub(super) async fn stored_tags(pool: &PgPool) -> Vec<(String, Uuid)> {
    sqlx::query_as::<_, (String, Uuid)>("SELECT name, id FROM hashtags ORDER BY name")
        .fetch_all(pool)
        .await
        .expect("the stored tags read")
}

/// A `Vec<String>` of the given written names, for the &[String] APIs.
pub(super) fn tags(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// A local user named `nickname` and one public status of theirs.
pub(super) async fn post(pool: &PgPool, nickname: &str, text: &str) -> Status {
    let author = insert_user(
        pool,
        &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
    )
    .await
    .expect("fixture user inserts");
    status::insert(pool, BASE, &NewStatus::local(&author, text))
        .await
        .expect("fixture status inserts")
}
