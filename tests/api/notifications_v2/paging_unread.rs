//! The listing pages whole groups, keyed on `most_recent_notification_id`,
//! through the `max_id` / `since_id` / `min_id` cursors and the malformed
//! 400; and `unread_count` counts unseen undismissed rows past the caller's
//! `notifications` marker, with its limit and filters.

use axum::http::StatusCode;
use serde_json::Value;
use wired_be::models::status::Visibility;

use super::support::{
    LIST, READ, WRITE, favourite, fixture, group_keys, groups, list, mark_seen, set_marker, unread,
};
use crate::common;

/// The listing pages whole groups, keyed on `most_recent_notification_id`:
/// `max_id` walks older, `since_id` polls newer, a `min_id` page reads
/// forward and answers flipped, and a malformed cursor is the shared 400.
#[tokio::test]
async fn group_pages_walk_the_most_recent_ids() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let actor = common::create_local_user(&pool, "actor").await;
    let oldest = common::create_status(&pool, &erin, "oldest", Visibility::Public).await;
    let middle = common::create_status(&pool, &erin, "middle", Visibility::Public).await;
    let newest = common::create_status(&pool, &erin, "newest", Visibility::Public).await;
    let first = favourite(&pool, &alice, &actor, &oldest).await;
    let second = favourite(&pool, &alice, &actor, &middle).await;
    let _third = favourite(&pool, &alice, &actor, &newest).await;
    let app = common::test_app(pool);

    let page = list(&app, &token, "?limit=2").await;
    assert_eq!(
        group_keys(&page),
        vec![
            format!("favourite-{}", newest.id),
            format!("favourite-{}", middle.id),
        ],
        "two groups, newest first"
    );

    let next = list(&app, &token, &format!("?limit=2&max_id={}", second.id)).await;
    assert_eq!(
        group_keys(&next),
        vec![format!("favourite-{}", oldest.id)],
        "max_id on a group's newest id steps to the older groups"
    );

    let polled = list(&app, &token, &format!("?since_id={}", first.id)).await;
    assert_eq!(
        group_keys(&polled),
        vec![
            format!("favourite-{}", newest.id),
            format!("favourite-{}", middle.id),
        ],
        "since_id polls the groups newer than the cursor, newest first"
    );

    let forward = list(&app, &token, &format!("?limit=1&min_id={}", first.id)).await;
    assert_eq!(
        group_keys(&forward),
        vec![format!("favourite-{}", middle.id)],
        "a min_id page reads forward and answers newest-first"
    );

    let empty = list(&app, &token, &format!("?max_id={}", first.id)).await;
    assert_eq!(
        groups(&empty),
        Vec::<&Value>::new(),
        "walking past the oldest group answers nothing, with no Link header"
    );

    let bad = common::TestRequest::get(&format!("{LIST}?max_id=not-an-id"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&bad, StatusCode::BAD_REQUEST, "a malformed cursor");
    common::assert_error(&bad, "Invalid id", "a malformed cursor");
}

/// `unread_count` counts unseen undismissed notifications past the caller's
/// `notifications` marker: every row without one, only newer rows with it,
/// seen and dismissed rows never, `limit` capping the answer (default 100,
/// at most 1000) and the filters narrowing it.
#[tokio::test]
async fn unread_count_follows_the_marker_seen_and_the_cap() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", READ).await;
    let read_token = common::create_app_and_token(&pool, &alice, READ).await;
    let write_token = common::create_app_and_token(&pool, &alice, WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let first = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let second = common::create_notification(&pool, &alice, &carol, "follow", None).await;
    let _third = common::create_notification(&pool, &alice, &bob, "mention", None).await;
    let app = common::test_app(pool.clone());

    assert_eq!(
        unread(&app, &read_token, "").await,
        3,
        "with no marker every unseen row counts"
    );
    assert_eq!(
        unread(&app, &read_token, "?limit=1").await,
        1,
        "the limit caps the count"
    );
    assert_eq!(
        unread(&app, &read_token, "?limit=0").await,
        3,
        "an unusable limit falls back to the default of 100, above these rows"
    );
    assert_eq!(
        unread(&app, &read_token, "?types[]=follow").await,
        2,
        "types counts only its own rows"
    );
    assert_eq!(
        unread(&app, &read_token, &format!("?account_id={}", carol.id)).await,
        1,
        "account_id counts only that actor's rows"
    );

    set_marker(&pool, &alice, &second.id.to_string()).await;
    assert_eq!(
        unread(&app, &read_token, "").await,
        1,
        "the marker itself is read; only newer rows count"
    );
    set_marker(&pool, &alice, &first.id.to_string()).await;
    assert_eq!(
        unread(&app, &read_token, "?types[]=follow").await,
        1,
        "the marker and the filter compose"
    );
    set_marker(&pool, &alice, "n-1").await;
    assert_eq!(
        unread(&app, &read_token, "").await,
        3,
        "a marker that names no id is no position"
    );

    mark_seen(&pool, first.id).await;
    assert_eq!(
        unread(&app, &read_token, "").await,
        2,
        "a seen row drops out"
    );

    let dismissed = common::TestRequest::post(&format!("{LIST}/ungrouped-{}/dismiss", second.id))
        .bearer(&write_token)
        .send(&app)
        .await;
    common::assert_answer(&dismissed, StatusCode::OK, "dismissing one row");
    assert_eq!(
        unread(&app, &read_token, "").await,
        1,
        "and a dismissed row drops out too"
    );
}
