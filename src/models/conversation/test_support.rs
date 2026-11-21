//! The fixtures the `conversation` module's unit tests share: the migrated
//! slot, the local and remote accounts, the direct post and reply that open
//! a thread, the create hook's bump, and the little readers that check what
//! a write stored. Test-only.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    config::TEST_BASE_URL,
    models::{
        status::{self, NewStatus, Status, Visibility},
        user::{self, NewUser, User},
    },
    pagination::PageParams,
    test_db::TestDb,
};

use super::{Participation, participate_for_status};

/// A migrated, emptied database slot.
pub(super) async fn slot() -> (TestDb, PgPool) {
    let db = crate::test_db::test_db()
        .await
        .expect("a migrated test database slot");
    let pool = db.pool().clone();
    (db, pool)
}

/// A fresh local account named `nickname`.
pub(super) async fn local(pool: &PgPool, nickname: &str) -> User {
    user::insert(
        pool,
        &NewUser::local(nickname, &format!("https://test.example/users/{nickname}")),
    )
    .await
    .expect("the local fixture inserts")
}

/// A mirror of the remote actor `carol@remote.example`.
pub(super) async fn remote(pool: &PgPool, nickname: &str) -> User {
    user::insert(
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

/// `author`'s `direct` status mentioning everyone in `mentioned`,
/// through the model — the row the create's hook would then see.
pub(super) async fn direct(
    pool: &PgPool,
    author: &User,
    mentioned: &[&User],
    text: &str,
) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility: Visibility::Direct,
            mentioned_user_ids: mentioned.iter().map(|who| who.id).collect(),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("the direct fixture inserts")
}

/// A `direct` reply by `author` to `parent`, mentioning `mentioned`:
/// the thread shape the create's reply arm stores.
pub(super) async fn direct_reply(
    pool: &PgPool,
    author: &User,
    parent: &Status,
    mentioned: &[&User],
    text: &str,
) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility: Visibility::Direct,
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(parent.user_id),
            conversation_id: Some(parent.conversation_id),
            mentioned_user_ids: mentioned.iter().map(|who| who.id).collect(),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("the direct reply fixture inserts")
}

/// Runs the create hook's whole write for `status`.
pub(super) async fn bump(pool: &PgPool, status: &Status) {
    participate_for_status(pool, status)
        .await
        .expect("participating for the fixture status runs");
}

/// The stored thread id of `status`, read back off the row.
pub(super) async fn linked_thread(pool: &PgPool, status: &Status) -> Uuid {
    let row = status::get_by_id(pool, status.id)
        .await
        .expect("the row reads")
        .expect("the fixture status is live");
    row.direct_conversation_id
        .expect("the fixture status got its thread")
}

/// The number of `conversations` rows.
pub(super) async fn thread_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversations")
        .fetch_one(pool)
        .await
        .expect("the count reads")
}

/// The stored participation of `owner` in `conversation_id`, for an
/// assertion.
pub(super) async fn participation(
    pool: &PgPool,
    owner: Uuid,
    conversation_id: Uuid,
) -> Option<Participation> {
    sqlx::query_as::<_, Participation>(
        "SELECT * FROM conversation_participations \
         WHERE user_id = $1 AND conversation_id = $2",
    )
    .bind(owner)
    .bind(conversation_id)
    .fetch_optional(pool)
    .await
    .expect("the participation reads")
}

/// The stored recipient ids of one participation.
pub(super) async fn stored_recipients(pool: &PgPool, participation_id: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT user_id FROM conversation_participation_recipients \
          WHERE participation_id = $1 ORDER BY user_id",
    )
    .bind(participation_id)
    .fetch_all(pool)
    .await
    .expect("the recipients read")
}

/// A page asking for `limit` rows with no cursor.
pub(super) fn page(limit: i64) -> PageParams {
    PageParams {
        max_id: None,
        min_id: None,
        since_id: None,
        limit,
        offset: None,
    }
}
