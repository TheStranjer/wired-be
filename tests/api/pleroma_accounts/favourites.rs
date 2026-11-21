//! The favourites route over HTTP: the open account's likes, the hidden
//! favourites, what the viewer may see and the paging on like row ids.

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        follow::{self, FollowState},
        status::{self, Visibility},
    },
};

use super::support::{
    HIDDEN_FAVOURITES, expected_like_link, favourites, favourites_path, like, liked_pairs,
    listed_status_ids, open_account, post, public_post_by, viewer, wanted_status_ids,
};
use crate::common;

/// The open account's favourites answer as `Status` entities, newest
/// favourite first, with the `Link` header keyed on the favourite row ids —
/// and the per-viewer flags render for whoever reads the list: `false` for
/// a stranger who likes none of these, `true` on the target's own read.
#[tokio::test]
async fn the_favourites_list_answers_the_open_account_s_likes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (bob, bob_token) = open_account(&pool, "bob", "read:favourites").await;
    let (_alice, alice_token) = viewer(&pool, "alice", "read:favourites").await;
    let author = common::create_local_user(&pool, "author").await;
    let mut statuses = Vec::new();
    for n in 0..3 {
        statuses.push(post(&pool, &author, Visibility::Public, &format!("post {n}")).await);
    }
    for status in &statuses {
        like(&pool, &bob, status.id).await;
    }
    let all = liked_pairs(&pool, &bob).await;
    let path = favourites_path(&bob.id.to_string());

    let listed = favourites(&app, Some(&alice_token), &bob.id.to_string(), "").await;
    common::assert_answer(&listed, StatusCode::OK, "an open list reads for a stranger");
    assert_eq!(
        listed.header("content-type"),
        Some("application/json"),
        "the entities are JSON"
    );
    let body = listed.json_value();
    let want: Vec<String> = all
        .iter()
        .map(|(_, status_id)| status_id.to_string())
        .collect();
    assert_eq!(listed_status_ids(&body), want, "newest favourite first");
    assert_eq!(
        body[0]["account"]["id"],
        author.id.to_string(),
        "the entries are full status entities with their author"
    );
    assert!(
        body.as_array()
            .expect("the array")
            .iter()
            .all(|entry| entry["favourited"] == Value::Bool(false)),
        "the flags answer the viewer, who favourited none of these"
    );
    assert_eq!(
        listed.header("link"),
        Some(expected_like_link(&path, &all[0].0, &all[2].0).as_str()),
        "the Link cursors are like row ids, not status ids"
    );

    let own = favourites(&app, Some(&bob_token), &bob.id.to_string(), "").await;
    common::assert_answer(&own, StatusCode::OK, "the target reads their own list");
    assert!(
        own.json_value()
            .as_array()
            .expect("the array")
            .iter()
            .all(|entry| entry["favourited"] == Value::Bool(true)),
        "every entry is the target's own favourite"
    );
}

/// `hide_favorites = true` — the default for every new account — is the
/// ticket's exact 403 for every viewer but the target: another account,
/// the anonymous request and an app-only token are all refused, while the
/// target's own read answers the list, and an account that turned the flag
/// off answers everybody.
#[tokio::test]
async fn hidden_favorites_refuse_everybody_but_the_target() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let carol = common::create_local_user(&pool, "carol").await;
    assert!(
        carol.hide_favorites.get(),
        "a new account hides its favourites by default"
    );
    let carol_token = common::create_app_and_token(&pool, &carol, "read:favourites").await;
    let (_alice, alice_token) = viewer(&pool, "alice", "read").await;
    let app_only = common::create_app_only_token(&pool, "read").await;
    let status = public_post_by(&pool, "author").await;
    like(&pool, &carol, status.id).await;
    let hidden = carol.id.to_string();

    for token in [Some(alice_token.as_str()), None, Some(app_only.as_str())] {
        let refused = favourites(&app, token, &hidden, "").await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            "hidden favourites refuse this viewer",
        );
        assert_eq!(refused.text(), HIDDEN_FAVOURITES, "the ticket's exact body");
    }

    let own = favourites(&app, Some(&carol_token), &hidden, "").await;
    common::assert_answer(
        &own,
        StatusCode::OK,
        "the target reads their own hidden list",
    );
    assert_eq!(
        listed_status_ids(&own.json_value()),
        vec![status.id.to_string()],
        "and sees the like the stranger was refused"
    );

    let (bob, _) = open_account(&pool, "bob", "read").await;
    assert!(
        !bob.hide_favorites.get(),
        "the fixture shares its favourites"
    );
    like(&pool, &bob, status.id).await;
    let shared = favourites(&app, Some(&alice_token), &bob.id.to_string(), "").await;
    common::assert_answer(
        &shared,
        StatusCode::OK,
        "the same viewer reads an open list",
    );
    assert_eq!(
        listed_status_ids(&shared.json_value()),
        vec![status.id.to_string()],
        "and gets the page"
    );
}

/// The listed statuses are narrowed to what the viewer may see: the
/// target's likes of a private post leave the stranger's page and reappear
/// when the stranger follows the author, a tombstoned post leaves for
/// everybody, and the `Link` edges are the likes that came back visible.
#[tokio::test]
async fn the_favourites_list_shows_only_what_the_viewer_may_see() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (bob, _) = open_account(&pool, "bob", "read").await;
    let (alice, alice_token) = viewer(&pool, "alice", "read:favourites").await;
    let author = common::create_local_user(&pool, "author").await;
    let open = post(&pool, &author, Visibility::Public, "open").await;
    let quiet = post(&pool, &author, Visibility::Unlisted, "quiet").await;
    let secret = post(&pool, &author, Visibility::Private, "followers only").await;
    let gone = post(&pool, &author, Visibility::Public, "about to go").await;
    for status in [&open, &quiet, &secret, &gone] {
        like(&pool, &bob, status.id).await;
    }
    let tombstone = status::soft_delete(&pool, gone.id)
        .await
        .expect("the delete runs")
        .expect("the post was live");
    assert!(tombstone.deleted_at.is_some(), "gone is a tombstone");
    let all = liked_pairs(&pool, &bob).await;
    let path = favourites_path(&bob.id.to_string());

    let listed = favourites(&app, Some(&alice_token), &bob.id.to_string(), "").await;
    common::assert_answer(&listed, StatusCode::OK, "the narrowed page answers");
    let visible = [open.id, quiet.id];
    let body = listed.json_value();
    assert_eq!(
        listed_status_ids(&body),
        wanted_status_ids(&all, &visible),
        "the private post and the tombstone are off a stranger's page"
    );
    let shown: Vec<Uuid> = all
        .iter()
        .filter(|(_, status_id)| visible.contains(status_id))
        .map(|(like_id, _)| *like_id)
        .collect();
    assert_eq!(
        listed.header("link"),
        Some(expected_like_link(&path, &shown[0], &shown[1]).as_str()),
        "only the visible likes name the page edges"
    );

    let following = follow::follow(&pool, TEST_BASE_URL, &alice, &author)
        .await
        .expect("alice follows the author");
    assert_eq!(
        following.state,
        FollowState::Accepted,
        "the unlocked author answers on the spot"
    );
    let after = favourites(&app, Some(&alice_token), &bob.id.to_string(), "").await;
    let visible_now = [open.id, quiet.id, secret.id];
    assert_eq!(
        listed_status_ids(&after.json_value()),
        wanted_status_ids(&all, &visible_now),
        "the follower's page carries the private favourite too"
    );

    let anonymous = favourites(&app, None, &bob.id.to_string(), "").await;
    assert_eq!(
        listed_status_ids(&anonymous.json_value()),
        wanted_status_ids(&all, &visible),
        "and the anonymous viewer keeps the pre-follow narrowing"
    );
}

/// The page walks the favourite row ids: a sized first page, the `max_id`
/// continuation, the `min_id` page answered newest-first again, and the
/// empty ends of the walk without a `Link` header.
#[tokio::test]
async fn the_favourites_list_pages_on_like_row_ids() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (bob, _) = open_account(&pool, "bob", "read").await;
    let (_alice, token) = viewer(&pool, "alice", "read:favourites").await;
    for n in 0..3 {
        let status = public_post_by(&pool, &format!("author{n}")).await;
        like(&pool, &bob, status.id).await;
    }
    let all = liked_pairs(&pool, &bob).await;
    let target = bob.id.to_string();
    let path = favourites_path(&target);

    let first = favourites(&app, Some(&token), &target, "?limit=2").await;
    common::assert_answer(&first, StatusCode::OK, "a sized page answers");
    assert_eq!(
        listed_status_ids(&first.json_value()),
        vec![all[0].1.to_string(), all[1].1.to_string()],
        "the two newest likes"
    );
    assert_eq!(
        first.header("link"),
        Some(expected_like_link(&path, &all[0].0, &all[1].0).as_str()),
        "the cursors carry the page's own like ids"
    );

    let older = favourites(
        &app,
        Some(&token),
        &target,
        &format!("?max_id={}", all[1].0),
    )
    .await;
    common::assert_answer(&older, StatusCode::OK, "the continuation answers");
    assert_eq!(
        listed_status_ids(&older.json_value()),
        vec![all[2].1.to_string()],
        "max_id walks to the older like, keyed on the like id"
    );
    assert_eq!(
        older.header("link"),
        Some(expected_like_link(&path, &all[2].0, &all[2].0).as_str()),
        "a one-entry page edges itself"
    );

    let newer = favourites(
        &app,
        Some(&token),
        &target,
        &format!("?min_id={}", all[2].0),
    )
    .await;
    assert_eq!(
        listed_status_ids(&newer.json_value()),
        vec![all[0].1.to_string(), all[1].1.to_string()],
        "min_id answers the newer page, still newest-first"
    );

    let polled = favourites(
        &app,
        Some(&token),
        &target,
        &format!("?since_id={}", all[0].0),
    )
    .await;
    common::assert_answer(&polled, StatusCode::OK, "polling past the head answers");
    assert_eq!(polled.json_value(), json!([]), "with the empty array");
    assert_eq!(polled.header("link"), None, "and no Link header");

    let past = favourites(
        &app,
        Some(&token),
        &target,
        &format!("?max_id={}", all[2].0),
    )
    .await;
    assert_eq!(
        past.json_value(),
        json!([]),
        "walking off the end is the empty page"
    );
    assert_eq!(past.header("link"), None, "which links nowhere");
}
