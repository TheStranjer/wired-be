//! The fixtures the `report` module's unit tests share: the accounts, posts
//! and filings a test starts with, and the little readers that check what a
//! write stored. Test-only.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    config::TEST_BASE_URL,
    models::{
        status::{self, NewStatus},
        user::{NewUser, User, insert, local_ap_id},
    },
    pagination::{MAX_LIMIT, PageParams},
    util::id::assert_local_activity_id,
};

use super::{Category, NewReport, Report, create, get};

/// Inserts a local, unlocked user named `nickname`.
pub(super) async fn local_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
    )
    .await
    .expect("the local fixture inserts")
}

/// Inserts a mirror of `nickname` on `remote.example`: the account this
/// instance does not own, whose `Flag` activity id is not ours to mint.
pub(super) async fn remote_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@remote.example"),
            ap_id: format!("https://remote.example/users/{nickname}"),
            domain: Some("remote.example".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the remote fixture inserts")
}

/// A public post by `author`, for the reports that cite something.
pub(super) async fn post(pool: &PgPool, author: &User, text: &str) -> status::Status {
    status::insert(pool, TEST_BASE_URL, &NewStatus::local(author, text))
        .await
        .expect("the status fixture inserts")
}

/// The report content a filing has no more of than it must: no comment,
/// no category, no flags, nothing cited.
pub(super) fn bare() -> NewReport {
    NewReport {
        comment: String::new(),
        category: Category::Other,
        forwarded: false,
        rule_ids: Vec::new(),
        status_ids: Vec::new(),
    }
}

/// Files `reporter`'s report about `target` and checks the row came back.
pub(super) async fn seed(pool: &PgPool, reporter: &User, target: &User) -> Report {
    create(pool, TEST_BASE_URL, reporter, target, &bare())
        .await
        .expect("the fixture report files")
}

/// The stored report behind `id`, which the test expects to exist.
pub(super) async fn stored(pool: &PgPool, id: Uuid) -> Report {
    get(pool, id).await.unwrap().expect("the report exists")
}

/// Every row of the table, newest filing first — the order a page walks,
/// read straight from storage so same-millisecond `UUIDv7`s cannot make an
/// expectation depend on the order the fixtures were written in.
pub(super) async fn stored_rows(pool: &PgPool) -> Vec<Report> {
    sqlx::query_as::<_, Report>("SELECT * FROM reports ORDER BY id DESC")
        .fetch_all(pool)
        .await
        .expect("the rows read")
}

/// The ids of `rows`, in order.
pub(super) fn ids_of(rows: &[Report]) -> Vec<Uuid> {
    rows.iter().map(|row| row.id).collect()
}

/// The number of rows of `reports`, which only ever holds fixture rows in
/// these tests.
pub(super) async fn report_count(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM reports")
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// The number of rows of `report_statuses`: the citations filed.
pub(super) async fn citation_count(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM report_statuses")
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// Page params built from a query string, the way the extractor makes them.
pub(super) fn page(query: &str) -> PageParams {
    PageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture page parses")
}

/// The `{TEST_BASE_URL}/activities/{uuid}` id [`create`] minted for a
/// local reporter, checked for its shape and reduced to the id part.
pub(super) fn activity_id(row: &Report) -> String {
    assert_local_activity_id(row.ap_id.as_deref())
}
