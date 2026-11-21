//! The fixtures the `list` module's unit tests share: the accounts and lists a
//! test starts with, the membership write it seeds through, and the little
//! readers that count what went in. Test-only.

use sqlx::PgPool;

use crate::{
    config::TEST_BASE_URL,
    models::user::{NewUser, User, insert, local_ap_id},
};

use super::{List, NewList, add_members, create};

/// Inserts a local user named `nickname`.
pub(super) async fn user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
    )
    .await
    .expect("the local fixture inserts")
}

/// A list titled `title` with the defaults otherwise.
pub(super) fn titled(title: &str) -> NewList {
    NewList {
        title: title.to_owned(),
        ..NewList::default()
    }
}

/// Inserts a list titled `title` for `user`, dropping the row.
pub(super) async fn seed(pool: &PgPool, owner: &User, title: &str) -> List {
    create(pool, owner.id, &titled(title))
        .await
        .expect("the fixture list creates")
}

/// Adds `member` to `list` through [`add_members`], the model's own write
/// (the membership endpoints' `WHERE` clauses and follow rule are their
/// ticket's business, and [`crate::api::lists`] tests them over HTTP).
pub(super) async fn add_member(pool: &PgPool, list: &List, member: &User) {
    let added = add_members(pool, list.id, &[member.id])
        .await
        .expect("the fixture membership adds");
    assert_eq!(added, 1, "exactly one membership went in");
}

/// The titles of `rows`, in row order — what a list test compares
/// against.
pub(super) fn titles(rows: &[List]) -> Vec<&str> {
    rows.iter().map(|row| row.title.as_str()).collect()
}

/// How many `lists` rows the database holds.
pub(super) async fn count_lists(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM lists")
        .fetch_one(pool)
        .await
        .expect("counting lists succeeds")
}

/// How many `list_members` rows the database holds.
pub(super) async fn count_members(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM list_members")
        .fetch_one(pool)
        .await
        .expect("counting memberships succeeds")
}
