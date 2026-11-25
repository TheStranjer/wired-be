//! Tests of the collection address helpers.

use sqlx::PgPool;

use super::*;
use crate::test_fixtures::remote_user;

/// The row of `user_id` written with `inbox` and the collections derived from it.
async fn rewrite_row(pool: &PgPool, user_id: Uuid, inbox: &str) -> ActivityPubActor {
    let write = ActivityPubActorWrite {
        inbox: Some(inbox.to_owned()),
        shared_inbox: Some(format!("{inbox}/shared")),
        outbox: Some(format!("{inbox}/outbox")),
        followers_url: Some(format!("{inbox}/followers")),
        following_url: Some(format!("{inbox}/following")),
        featured_url: Some(format!("{inbox}/featured")),
        public_key: Some("ROW-PUBLIC".to_owned()),
        private_key: Some("ROW-PRIVATE".to_owned()),
        ..ActivityPubActorWrite::bare(user_id)
    };
    upsert(pool, &write).await.expect("the row writes")
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
