//! Fixtures the block model's unit tests share.

use sqlx::{Error as SqlxError, PgPool};
use uuid::Uuid;

use super::{Block, read::get, write::block, write::unblock};
use crate::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, FollowState},
        user::{Flag, NewUser, User, get_by_id, insert, local_ap_id},
    },
    pagination::{MAX_LIMIT, PageParams},
    util::id::parse_id,
};

/// The `ActivityPub` path segment a local blocker's `ap_id` is built
/// under, as [`block`] must write it (decision 6).
pub(super) const ACTIVITY_PREFIX: &str = "/activities/";

/// Inserts a local, unlocked user named `nickname`.
pub(super) async fn local_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
    )
    .await
    .expect("the local fixture inserts")
}

/// Inserts a local user named `nickname` whose account is locked, so a
/// follow of it waits for an answer.
pub(super) async fn locked_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser {
            locked: Flag::new(true),
            ..NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
        },
    )
    .await
    .expect("the locked fixture inserts")
}

/// Inserts a mirror of `nickname` on `remote.example`: the account this
/// instance does not own, whose `Block` activity id is not ours to mint.
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

/// The row of user `id` as it stands now, for reading counters back.
pub(super) async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
    get_by_id(pool, id).await.unwrap().expect("the user exists")
}

/// Blocks `target` for `blocker` through the write under test and checks
/// the row it wrote: the fixture every read below starts from.
pub(super) async fn seed_block(pool: &PgPool, blocker: &User, target: &User) {
    let row = block(pool, TEST_BASE_URL, blocker, target)
        .await
        .expect("the fixture block stores");
    assert_eq!(
        row.blocked_id, target.id,
        "the row is the pair that was asked for"
    );
}

/// Lifts `blocker`'s block on `target`, asserting one was there to lift.
pub(super) async fn seed_unblock(pool: &PgPool, blocker: &User, target: &User) {
    let removed = unblock(pool, blocker, target)
        .await
        .expect("the fixture unblock runs");
    assert!(removed.is_some(), "the pair's block was there to lift");
}

/// Records `follower`'s follow of the open local `target`, asserting the
/// answer was granted on the spot: the fixture the severing checks.
pub(super) async fn seed_follow(pool: &PgPool, follower: &User, target: &User) {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the fixture follow records");
    assert_eq!(
        row.state,
        FollowState::Accepted,
        "an unlocked local account answers at once"
    );
}

/// The stored block of the pair, which the test expects to exist.
pub(super) async fn stored_block(pool: &PgPool, blocker: &User, target: &User) -> Block {
    get(pool, blocker.id, target.id)
        .await
        .unwrap()
        .expect("the block exists")
}

/// Every row of the table, newest block first — the order a page walks,
/// read straight from storage so same-millisecond `UUIDv7`s cannot make an
/// expectation depend on the order the fixtures were written in.
pub(super) async fn stored_rows(pool: &PgPool) -> Vec<(Uuid, Uuid, Uuid)> {
    sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
        "SELECT id, blocker_id, blocked_id FROM blocks ORDER BY id DESC",
    )
    .fetch_all(pool)
    .await
    .expect("the rows read")
}

/// The number of rows in the table.
pub(super) async fn block_count(pool: &PgPool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM blocks")
        .fetch_one(pool)
        .await
        .unwrap();
    row.0
}

/// Page params built from a query string, the way the extractor makes them.
pub(super) fn page(query: &str) -> PageParams {
    PageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture page parses")
}

/// The `{TEST_BASE_URL}/activities/{uuid}` id [`block`] minted for a local
/// blocker, checked for its shape and reduced to the id part.
pub(super) fn activity_id(row: &Block) -> String {
    let prefix = format!("{TEST_BASE_URL}{ACTIVITY_PREFIX}");
    let ap_id = row
        .ap_id
        .clone()
        .expect("a local blocker's row carries a minted activity id");
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

/// The blocked ids of `entries`, in the order they came back.
pub(super) fn blocked_ids_of(entries: &[(Block, User)]) -> Vec<Uuid> {
    entries.iter().map(|(_, user)| user.id).collect()
}

/// The block row ids of `entries`, in the order they came back.
pub(super) fn row_ids_of(entries: &[(Block, User)]) -> Vec<Uuid> {
    entries.iter().map(|(row, _)| row.id).collect()
}
