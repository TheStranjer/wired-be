//! The follows a move creates for the origin's local followers, over
//! `POST /api/pleroma/move_account`: the `Follow` delivered to a remote
//! target, the request a locked local target receives, the notification of
//! an unlocked one, and the blocked follower that stays behind.

use serde_json::json;
use sqlx::PgPool;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        block,
        follow::FollowState,
        user::{Flag, User},
    },
};

use super::support::{PASSWORD, aliased_target, assert_stays, fixture, follow_user, move_account};
use super::support::{relation, stored};
use crate::common::{self, delivery_jobs};

/// `(type, from_account_id)` of every notification of `user`, oldest first.
async fn told(pool: &PgPool, user: &User) -> Vec<(String, uuid::Uuid)> {
    sqlx::query_as::<_, (String, uuid::Uuid)>(
        "SELECT type, from_account_id FROM notifications WHERE user_id = $1 ORDER BY id",
    )
    .bind(user.id)
    .fetch_all(pool)
    .await
    .expect("the notifications read")
}

/// A remote account on `remote.example` that lists `origin` among its
/// aliases.
async fn remote_target(pool: &PgPool, origin: &User) -> User {
    let remote = common::create_remote_user(pool, "tina", "remote.example").await;
    let _updated = sqlx::query("UPDATE users SET also_known_as = $2 WHERE id = $1")
        .bind(remote.id)
        .bind(vec![origin.ap_id.clone()])
        .execute(pool)
        .await
        .expect("the alias stamps in");
    stored(pool, remote.id).await
}

/// Every local follower of a moved account gets a `Follow` delivered to
/// the remote target's inbox; the row is pending under the delivered id.
#[tokio::test]
async fn a_move_to_a_remote_account_delivers_a_follow_per_follower() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (origin, token) = fixture(&pool, "alice", "write:accounts").await;
    let target = remote_target(&pool, &origin).await;
    let target_inbox = common::actor_of(&pool, &target)
        .await
        .inbox
        .expect("an inbox");
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    for follower in [&carol, &dave] {
        follow_user(&pool, follower, &origin).await;
    }

    let app = common::test_app(pool.clone());
    let response = move_account(&app, &token, PASSWORD, "tina@remote.example").await;
    assert_eq!(response.json_value(), json!({"status": "success"}));

    let jobs = delivery_jobs(&pool).await;
    let follows = jobs
        .iter()
        .filter(|job| job["body"]["type"] == json!("Follow"))
        .collect::<Vec<_>>();
    assert_eq!(follows.len(), 2, "one Follow per local follower");
    for follower in [&carol, &dave] {
        let row = relation(&pool, follower, &target).await;
        assert_eq!(
            row.state,
            FollowState::Pending,
            "the target's answer is awaited"
        );
        let job = follows
            .iter()
            .find(|job| job["body"]["actor"] == json!(follower.ap_id))
            .expect("the follower's Follow was queued");
        assert_eq!(job["body"]["object"], json!(target.ap_id));
        assert_eq!(job["inbox"], json!(target_inbox));
        assert_eq!(
            Some(job["body"]["id"].as_str().expect("an id")),
            row.ap_id.as_deref(),
            "the stored row carries the delivered activity id"
        );
    }
}

/// A locked local target holds the follows as requests and is told of
/// each one.
#[tokio::test]
async fn a_locked_local_target_receives_follow_requests() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (origin, token) = fixture(&pool, "alice", "write:accounts").await;
    let origin_ap_id = origin.ap_id.clone();
    let target = common::create_local_user_with(&pool, "bob", |mut new| {
        new.also_known_as = vec![origin_ap_id];
        new.locked = Flag::new(true);
        new
    })
    .await;
    let carol = common::create_local_user(&pool, "carol").await;
    follow_user(&pool, &carol, &origin).await;

    let app = common::test_app(pool.clone());
    let response = move_account(&app, &token, PASSWORD, "bob").await;
    assert_eq!(response.json_value(), json!({"status": "success"}));

    assert_eq!(
        relation(&pool, &carol, &target).await.state,
        FollowState::Pending
    );
    assert_eq!(
        told(&pool, &target).await,
        vec![("follow_request".to_owned(), carol.id)],
        "the target is asked, by the follower"
    );
}

/// An unlocked local target accepts on the spot and gets a `follow`
/// notification.
#[tokio::test]
async fn an_unlocked_local_target_is_notified_of_the_follow() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (origin, token) = fixture(&pool, "alice", "write:accounts").await;
    let target = aliased_target(&pool, "bob", &origin).await;
    let carol = common::create_local_user(&pool, "carol").await;
    follow_user(&pool, &carol, &origin).await;

    let app = common::test_app(pool.clone());
    let response = move_account(&app, &token, PASSWORD, "bob").await;
    assert_eq!(response.json_value(), json!({"status": "success"}));

    assert_eq!(
        relation(&pool, &carol, &target).await.state,
        FollowState::Accepted
    );
    assert_eq!(
        told(&pool, &target).await,
        vec![("follow".to_owned(), carol.id)],
        "the target hears of the new follower"
    );
}

/// A follower the target blocked is skipped: it keeps following the origin
/// and nothing is delivered for it.
#[tokio::test]
async fn a_follower_blocked_by_the_target_stays_with_the_origin() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (origin, token) = fixture(&pool, "alice", "write:accounts").await;
    let target = remote_target(&pool, &origin).await;
    let carol = common::create_local_user(&pool, "carol").await;
    follow_user(&pool, &carol, &origin).await;
    let _blocked = block::block(&pool, TEST_BASE_URL, &target, &carol)
        .await
        .expect("the block is recorded");

    let app = common::test_app(pool.clone());
    let response = move_account(&app, &token, PASSWORD, "tina@remote.example").await;
    assert_eq!(response.json_value(), json!({"status": "success"}));

    assert_stays(&pool, &carol, &origin, &target).await;
    assert!(
        delivery_jobs(&pool)
            .await
            .iter()
            .all(|job| job["body"]["type"] != json!("Follow")),
        "no Follow is sent for a blocked follower"
    );
}
