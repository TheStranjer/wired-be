//! The fixtures the `follow` module's unit tests share: the users and
//! relationships a test starts with, and the little readers and orderings
//! that check what a write moved. Test-only.

use sqlx::{Error as SqlxError, PgPool};
use uuid::Uuid;

use crate::{
    config::TEST_BASE_URL,
    models::{
        block,
        user::{Flag, NewUser, User, get_by_id, insert, local_ap_id},
    },
    pagination::{MAX_LIMIT, PageParams},
    util::id::{new_id, parse_id},
};

use super::{Follow, FollowPageEntry, get};

/// The `ActivityPub` path segment a local follower's `ap_id` is built
/// under, as [`follow`](super::follow) must write it (decision 6).
pub(super) const ACTIVITY_PREFIX: &str = "/activities/";

/// A local [`NewUser`] named `nickname` on the test instance.
pub(super) fn local_new(nickname: &str) -> NewUser {
    NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
}

/// Inserts a local, unlocked user named `nickname`.
pub(super) async fn local_user(pool: &PgPool, nickname: &str) -> User {
    insert(pool, &local_new(nickname))
        .await
        .expect("the local fixture inserts")
}

/// Inserts a local user named `nickname` whose account is locked, so
/// follows of it wait for an answer.
pub(super) async fn locked_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser {
            locked: Flag::new(true),
            ..local_new(nickname)
        },
    )
    .await
    .expect("the locked fixture inserts")
}

/// Inserts a mirror of `nickname` on `remote.example` whose fetch reported
/// seven followers, so a test can see whether a counter was touched.
pub(super) async fn remote_user(pool: &PgPool, nickname: &str) -> User {
    let ap_id = format!("https://remote.example/users/{nickname}");
    insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@remote.example"),
            ap_id,
            domain: Some("remote.example".to_owned()),
            followers_count: 7,
            ..NewUser::default()
        },
    )
    .await
    .expect("the remote fixture inserts")
}

/// The row of user `id` as it stands now, for reading counters back.
pub(super) async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
    get_by_id(pool, id).await.unwrap().expect("the user exists")
}

/// The stored relationship of the pair, which the test expects to exist.
pub(super) async fn relation(pool: &PgPool, follower: &User, target: &User) -> Follow {
    get(pool, follower.id, target.id)
        .await
        .unwrap()
        .expect("the relationship exists")
}

/// Writes a `rejected` row by hand: [`reject`](super::reject) deletes the row
/// rather than storing that state, so the reopen and unfollow paths have to
/// set it up the way a mirrored remote rejection would have.
pub(super) async fn rejected_relation(pool: &PgPool, follower: &User, target: &User) -> Follow {
    sqlx::query_as::<_, Follow>(
        "INSERT INTO follows (id, follower_id, followee_id, state)
         VALUES ($1, $2, $3, 'rejected')
         RETURNING *",
    )
    .bind(new_id())
    .bind(follower.id)
    .bind(target.id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Records `blocker` blocking `target` through the table's own module:
/// the fixture the follow guard below reads.
pub(super) async fn block_pair(pool: &PgPool, blocker: &User, target: &User) {
    let stored = block::block(pool, TEST_BASE_URL, blocker, target)
        .await
        .expect("the fixture block stores");
    assert_eq!(
        stored.blocked_id, target.id,
        "the row is the pair that was asked for"
    );
}

/// The raw `state` text of one row, to check the enum's database mapping.
pub(super) async fn state_text(pool: &PgPool, follow_id: Uuid) -> String {
    let row: (String,) = sqlx::query_as("SELECT state FROM follows WHERE id = $1")
        .bind(follow_id)
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// The number of relationship rows in the table.
pub(super) async fn follow_count(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM follows")
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// Page params built from a query string, the way the extractor makes them.
pub(super) fn page(query: &str) -> PageParams {
    PageParams::from_query(Some(query), MAX_LIMIT).unwrap()
}

/// The ids of `users`, in the order they came back.
pub(super) fn ids(users: &[User]) -> Vec<Uuid> {
    users.iter().map(|user| user.id).collect()
}

/// The ids of the users behind `entries`, in the order they came back.
pub(super) fn entry_ids(entries: &[FollowPageEntry]) -> Vec<Uuid> {
    entries.iter().map(|entry| entry.user.id).collect()
}

/// The relationship row ids of `entries`, in the order they came back.
pub(super) fn follow_ids(entries: &[FollowPageEntry]) -> Vec<Uuid> {
    entries.iter().map(|entry| entry.follow_id).collect()
}

/// The `{TEST_BASE_URL}/activities/{uuid}` id [`follow`](super::follow)
/// minted for a local follower, checked for its shape and reduced to the id
/// part.
pub(super) fn activity_id(follow: &Follow) -> String {
    let prefix = format!("{TEST_BASE_URL}{ACTIVITY_PREFIX}");
    let ap_id = follow
        .ap_id
        .clone()
        .expect("a local follower's row carries a minted activity id");
    assert!(
        ap_id.starts_with(&prefix),
        "the activity id is this instance's, got {ap_id}"
    );
    let tail = ap_id
        .strip_prefix(&prefix)
        .expect("the prefix was just checked");
    assert!(
        parse_id(tail).is_some(),
        "the tail of the activity id is an id: {tail}"
    );
    tail.to_owned()
}

/// The SQLSTATE a database failure carries, or an empty string when the
/// failure is not the database reporting a constraint.
pub(super) fn sqlstate_of(err: SqlxError) -> String {
    if let SqlxError::Database(db_err) = err {
        return db_err.code().unwrap_or_default().into_owned();
    }
    String::new()
}

/// `rows` by relationship id, oldest first. `UUIDv7` decides the order the
/// lists come back in, and it is only millisecond precise, so an
/// expectation is sorted from what was stored rather than from the order
/// the writes happened in.
pub(super) fn by_age(rows: &[Follow]) -> Vec<&Follow> {
    let mut sorted = rows.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|follow| follow.id);
    sorted
}

/// The followees of `rows`, oldest relationship first.
pub(super) fn followed_oldest_first(rows: &[Follow]) -> Vec<Uuid> {
    by_age(rows)
        .iter()
        .map(|follow| follow.followee_id)
        .collect()
}

/// The followers of `rows`, newest relationship first.
pub(super) fn followers_newest_first(rows: &[Follow]) -> Vec<Uuid> {
    by_age(rows)
        .iter()
        .rev()
        .map(|follow| follow.follower_id)
        .collect()
}

/// The followees of `rows`, newest relationship first.
pub(super) fn followed_newest_first(rows: &[Follow]) -> Vec<Uuid> {
    by_age(rows)
        .iter()
        .rev()
        .map(|follow| follow.followee_id)
        .collect()
}
