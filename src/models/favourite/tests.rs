//! Tests for the favourites model: the fixtures every group shares, and the
//! groups as child modules.

mod lifecycle;
mod pages;
mod writes;

use sqlx::PgPool;

use super::*;
use crate::{
    config::TEST_BASE_URL,
    error::AppError,
    models::{
        status::{NewStatus, insert, soft_delete},
        user::{NewUser, User, insert as insert_user, local_ap_id},
    },
    pagination::MAX_LIMIT,
};

/// The instance origin the fixtures favourite on.
const BASE: &str = TEST_BASE_URL;
/// The activity-id prefix [`favourite`] mints.
const ACTIVITY_PREFIX: &str = "/activities/";
/// How many favourites the page tests seed.
const PAGE_SEED: usize = 3;

/// A local user named `nickname`.
async fn user(pool: &PgPool, nickname: &str) -> User {
    insert_user(
        pool,
        &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
    )
    .await
    .expect("the fixture user inserts")
}

/// A public post by `author`, as its row.
async fn post(pool: &PgPool, author: &User) -> Status {
    insert(pool, BASE, &NewStatus::local(author, "hello"))
        .await
        .expect("the fixture post inserts")
}

/// A public post by a fresh local author named `nickname`, as its id.
async fn post_by(pool: &PgPool, nickname: &str) -> Uuid {
    post(pool, &user(pool, nickname).await).await.id
}

/// Favourites `status_id` as `who`, asserting the write created a row
/// (a test that needs the no-op arm calls [`favourite`] itself).
async fn favourite_as(pool: &PgPool, who: &User, status_id: Uuid) {
    let created = favourite(pool, BASE, who.id, status_id)
        .await
        .expect("the fixture favourite stores");
    assert!(created.is_some(), "the fixture pair had not favourited yet");
}

/// Every stored favourite as `(id, user_id, status_id)`, newest id
/// first — the order the pages walk — so an assertion says what the
/// table holds rather than what a loader chose to show.
async fn stored(pool: &PgPool) -> Vec<(Uuid, Uuid, Uuid)> {
    sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
        "SELECT id, user_id, status_id FROM favourites ORDER BY id DESC",
    )
    .fetch_all(pool)
    .await
    .expect("the table reads")
}

/// The stored `favourites_count` of status `status_id`.
async fn count(pool: &PgPool, status_id: Uuid) -> i32 {
    let row: (i32,) = sqlx::query_as("SELECT favourites_count FROM statuses WHERE id = $1")
        .bind(status_id)
        .fetch_one(pool)
        .await
        .expect("the counter reads");
    row.0
}

/// Page params built from a query string, the way the extractor makes
/// them.
fn page(query: &str) -> PageParams {
    PageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture page parses")
}

/// The ids of `entries`.
fn entry_ids(entries: &[FavouritePageEntry]) -> Vec<Uuid> {
    entries.iter().map(|entry| entry.favourite_id).collect()
}

/// The favourite ids of `pairs`, in order.
fn pair_ids(pairs: &[(Favourite, Status)]) -> Vec<Uuid> {
    pairs.iter().map(|(favourite, _)| favourite.id).collect()
}
