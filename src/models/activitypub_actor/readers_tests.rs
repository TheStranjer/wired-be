//! Tests of the readers: [`with_plumbing`], [`with_plumbing_many`] and the
//! collection address helpers.

use sqlx::PgPool;

use super::*;
use crate::{
    models::user::{NewUser, insert},
    test_fixtures::{local_new, remote_user},
};

/// The row of `user_id` rewritten with `inbox`, `followers_url` and
/// `featured_url`, so a test can tell the row from the `users` columns.
async fn rewrite_row(pool: &PgPool, user_id: Uuid, inbox: &str) -> ActivityPubActor {
    let write = ActivityPubActorWrite {
        user_id,
        status: ActorStatus::Active,
        inbox: Some(inbox.to_owned()),
        shared_inbox: Some(format!("{inbox}/shared")),
        outbox: Some(format!("{inbox}/outbox")),
        followers_url: Some(format!("{inbox}/followers")),
        following_url: Some(format!("{inbox}/following")),
        featured_url: Some(format!("{inbox}/featured")),
        public_key: Some("ROW-PUBLIC".to_owned()),
        private_key: Some("ROW-PRIVATE".to_owned()),
        public_key_id: None,
        last_fetched_at: None,
    };
    upsert(pool, &write).await.expect("the row rewrites")
}

#[tokio::test]
async fn with_plumbing_reads_the_moved_columns_from_the_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let user = remote_user(&pool, "far").await;
    let row = rewrite_row(&pool, user.id, "https://row.example/far").await;

    let read = with_plumbing(&pool, user.clone())
        .await
        .expect("the row reads");

    assert_eq!(read.inbox, row.inbox, "inbox");
    assert_eq!(read.shared_inbox, row.shared_inbox, "shared inbox");
    assert_eq!(read.outbox, row.outbox, "outbox");
    assert_eq!(read.followers_url, row.followers_url, "followers");
    assert_eq!(read.following_url, row.following_url, "following");
    assert_eq!(read.featured_url, row.featured_url, "featured");
    assert_eq!(read.public_key.as_deref(), Some("ROW-PUBLIC"), "key");
    assert_eq!(read.private_key.as_deref(), Some("ROW-PRIVATE"), "secret");
    assert_eq!(read.last_fetched_at, row.last_fetched_at, "fetch time");
    assert_eq!(read.id, user.id, "the rest of the account is untouched");
    assert_eq!(read.ap_id, user.ap_id, "including its actor id");
}

#[tokio::test]
async fn with_plumbing_of_an_account_without_a_row_clears_the_columns() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let user = insert(
        &pool,
        &NewUser {
            inbox: Some("https://test.example/users/alice/inbox".to_owned()),
            public_key: Some("USERS-PUBLIC".to_owned()),
            ..local_new("alice")
        },
    )
    .await
    .expect("the local fixture inserts");
    assert!(user.inbox.is_some(), "users still holds an inbox");
    let removed = sqlx::query("DELETE FROM activitypub_actors WHERE user_id = $1")
        .bind(user.id)
        .execute(&pool)
        .await
        .expect("the row deletes")
        .rows_affected();
    assert_eq!(removed, 1, "the fixture came with a row");

    let read = with_plumbing(&pool, user).await.expect("the read works");

    assert!(read.inbox.is_none(), "no row, no inbox");
    assert!(read.public_key.is_none(), "no row, no key");
    assert!(read.followers_url.is_none(), "no row, no collections");
}

#[tokio::test]
async fn with_plumbing_many_applies_each_row_in_the_order_given() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let first = remote_user(&pool, "first").await;
    let second = remote_user(&pool, "second").await;
    let bare = remote_user(&pool, "bare").await;
    let _first = rewrite_row(&pool, first.id, "https://row.example/first").await;
    let _second = rewrite_row(&pool, second.id, "https://row.example/second").await;
    let removed = sqlx::query("DELETE FROM activitypub_actors WHERE user_id = $1")
        .bind(bare.id)
        .execute(&pool)
        .await
        .expect("the row deletes")
        .rows_affected();
    assert_eq!(removed, 1, "the bare account's row goes");

    let read = with_plumbing_many(&pool, vec![second.clone(), bare.clone(), first.clone()])
        .await
        .expect("the rows read");

    let ids: Vec<Uuid> = read.iter().map(|user| user.id).collect();
    assert_eq!(ids, [second.id, bare.id, first.id], "the order is kept");
    assert_eq!(read[0].inbox.as_deref(), Some("https://row.example/second"));
    assert!(read[1].inbox.is_none(), "the account without a row");
    assert_eq!(read[2].inbox.as_deref(), Some("https://row.example/first"));
}

#[tokio::test]
async fn the_collection_addresses_are_the_rows_with_the_built_one_behind() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let user = remote_user(&pool, "far").await;
    let row = rewrite_row(&pool, user.id, "https://row.example/far").await;
    let ap_id = "https://remote.example/users/far";

    assert_eq!(
        followers_address(Some(&row), ap_id),
        "https://row.example/far/followers",
        "the stored followers collection"
    );
    assert_eq!(
        featured_address(Some(&row), ap_id),
        "https://row.example/far/featured",
        "the stored featured collection"
    );
    assert_eq!(
        followers_address(None, ap_id),
        format!("{ap_id}/followers"),
        "no row: the built address"
    );
    assert_eq!(
        featured_address(None, ap_id),
        format!("{ap_id}/collections/featured"),
        "no row: the built address"
    );
    let bare = ActivityPubActor {
        followers_url: None,
        featured_url: None,
        ..row
    };
    assert_eq!(
        followers_address(Some(&bare), ap_id),
        format!("{ap_id}/followers"),
        "a row without the column: the built address"
    );
    assert_eq!(
        featured_address(Some(&bare), ap_id),
        format!("{ap_id}/collections/featured"),
        "a row without the column: the built address"
    );
}
