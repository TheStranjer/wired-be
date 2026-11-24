//! The fixtures the `poll` module's unit tests share: a named local user,
//! status and poll, the raw mark write the cascade reads need, and the
//! one-binding count probe every lifecycle test asks. Test-only.

use sqlx::PgPool;
use uuid::Uuid;

use crate::models::{
    status::{self, NewStatus, Status},
    user::User,
};

use super::{Poll, PollParams, create};

pub(super) use crate::test_fixtures::local_user as create_user;

/// The instance origin the fixtures pretend to serve.
pub(super) const BASE: &str = "https://test.example";

/// Inserts a local status of `author`, because `status_id` is a foreign
/// key every poll names.
pub(super) async fn create_status(pool: &PgPool, author: &User) -> Status {
    status::insert(pool, BASE, &NewStatus::local(author, "a poll question"))
        .await
        .expect("fixture status inserts")
}

/// [`PollParams`] for the given option titles, nothing else set.
pub(super) fn params(titles: &[&str]) -> PollParams {
    let options = titles.iter().map(|title| (*title).to_owned()).collect();
    PollParams {
        options,
        ..PollParams::default()
    }
}

/// [`create`] through a connection of the pool's own, the way a caller
/// outside a transaction reaches it.
pub(super) async fn create_for(pool: &PgPool, status_id: Uuid, params: &PollParams) -> Poll {
    let mut conn = pool.acquire().await.expect("slot connection");
    create(&mut conn, status_id, params)
        .await
        .expect("fixture poll creates")
}

/// Records a mark straight into `poll_votes`: the vote endpoint that
/// normally writes it is a later ticket, and the cascade and the
/// voter's-flag reads need votes to exist.
pub(super) async fn vote(pool: &PgPool, poll_id: Uuid, option_id: Uuid, user_id: Uuid) {
    let inserted = sqlx::query(
        "INSERT INTO poll_votes (poll_id, option_id, user_id, inserted_at)
         VALUES ($1, $2, $3, now())",
    )
    .bind(poll_id)
    .bind(option_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("fixture vote inserts");
    assert_eq!(inserted.rows_affected(), 1, "the mark was written");
}

/// Runs a one-binding `SELECT count(*)` — the shape every cascade test
/// asks ("how many rows of this table still name that id?"). The query is
/// a literal at every call site, which is what lets it reach
/// `sqlx::query_scalar` as `'static` text.
pub(super) async fn count_for(pool: &PgPool, sql: &'static str, id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the count reads")
}
