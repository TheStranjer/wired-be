//! The `delete_per_vote_poll_notifications` migration (WBE-807): the rows a
//! ballot used to raise for a poll's author go, the end-of-poll rows stay.

use sqlx::PgPool;
use uuid::Uuid;

use crate::models::{
    status::{self, NewStatus},
    user::{self, NewUser, User},
};

/// The migration's own SQL, replayed against a slot that already holds the
/// migrated schema.
const MIGRATION_SQL: &str =
    include_str!("../../../migrations/20261125022559_delete_per_vote_poll_notifications.sql");

/// A local user named `nickname`.
async fn account(pool: &PgPool, nickname: &str) -> User {
    user::insert(
        pool,
        &NewUser::local(
            nickname,
            &user::local_ap_id(crate::config::TEST_BASE_URL, nickname),
        ),
    )
    .await
    .expect("the user fixture inserts")
}

/// A stored notification of `kind` for `user_id`, caused by `from_id`, about
/// `status_id`; its id.
async fn notify(pool: &PgPool, user_id: Uuid, from_id: Uuid, kind: &str, status_id: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    let _inserted = sqlx::query(
        "INSERT INTO notifications (id, user_id, from_account_id, type, status_id)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(user_id)
    .bind(from_id)
    .bind(kind)
    .bind(status_id)
    .execute(pool)
    .await
    .expect("the notification fixture inserts");
    id
}

/// The ids of every stored notification, sorted.
async fn stored(pool: &PgPool) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM notifications")
        .fetch_all(pool)
        .await
        .expect("the notifications read");
    ids.sort();
    ids
}

/// The per-vote `poll` rows (the author notified, the voter as actor) go,
/// whether or not the poll has ended; the author's own closing, a voter's
/// closing and every other type stay.
#[tokio::test]
async fn the_migration_removes_per_vote_rows_and_keeps_end_of_poll_rows() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = account(&pool, "ppn-alice").await;
    let bob = account(&pool, "ppn-bob").await;
    let carol = account(&pool, "ppn-carol").await;
    let posted = status::insert(
        &pool,
        crate::config::TEST_BASE_URL,
        &NewStatus::local(&alice, "tea or coffee?"),
    )
    .await
    .expect("the status fixture inserts");

    let _per_vote_bob = notify(&pool, alice.id, bob.id, "poll", posted.id).await;
    let _per_vote_carol = notify(&pool, alice.id, carol.id, "poll", posted.id).await;
    let author_closing = notify(&pool, alice.id, alice.id, "poll", posted.id).await;
    let voter_closing = notify(&pool, bob.id, alice.id, "poll", posted.id).await;
    let favourite = notify(&pool, alice.id, bob.id, "favourite", posted.id).await;

    let _migrated = sqlx::raw_sql(MIGRATION_SQL)
        .execute(&pool)
        .await
        .expect("the migration runs");

    let mut kept = vec![author_closing, voter_closing, favourite];
    kept.sort();
    assert_eq!(
        stored(&pool).await,
        kept,
        "only the two per-vote rows were deleted"
    );
}
