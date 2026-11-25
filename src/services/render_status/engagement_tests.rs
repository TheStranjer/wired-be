//! Unit tests for the engagement parts of a page render: the emoji
//! reaction roll-ups and the viewer's own favourite, reblog and bookmark
//! flags, each answered by one batched read.

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    ViewerMode, render_statuses,
    test_support::{BASE, app_state, post, user},
};
use crate::{
    entities::status::Status,
    models::{
        bookmark, bookmark_folder,
        emoji_reaction::{self, NewReaction},
        favourite,
        status::{self, NewStatus, Status as StatusRow, Visibility, insert},
        user::User,
    },
};

/// A unicode emoji the reaction fixtures use.
const THUMB: &str = "👍";
/// A custom emoji's stored spelling, and the image recorded with it.
const CAT: &str = ":cat:";
/// [`CAT`]'s image URL.
const CAT_URL: &str = "https://test.example/emoji/cats/cat.png";

/// The `pleroma.emoji_reactions` array of a rendered entity.
fn reactions(entity: &Status) -> Value {
    serde_json::to_value(entity).expect("a rendered status serializes")["pleroma"]
        ["emoji_reactions"]
        .clone()
}

/// Reacts to `status` as `reactor` with `emoji`, whose custom image is
/// `url`. The model stores what it is handed; the endpoint is what
/// validates the emoji, so a batch test needs no `custom_emojis` row.
async fn react(pool: &PgPool, reactor: &User, status: &StatusRow, emoji: &str, url: Option<&str>) {
    let stored = emoji_reaction::react(
        pool,
        BASE,
        &NewReaction {
            user_id: reactor.id,
            status_id: status.id,
            emoji,
            url,
        },
    )
    .await
    .expect("the fixture reaction stores");
    assert_eq!(stored.emoji, emoji, "the fixture row names its emoji");
}

/// Favourites `status_id` as `liker`, asserting the write created its
/// row — the state the flag test starts from.
async fn favourite_it(pool: &PgPool, liker: &User, status_id: Uuid) {
    let created = favourite::favourite(pool, BASE, liker.id, status_id)
        .await
        .expect("the fixture favourite stores");
    assert!(created.is_some(), "the fixture pair had not favourited yet");
}

/// Bookmarks `status_id` as `who`, unfiled — the state the bookmark
/// flag test starts from.
async fn bookmark_it(pool: &PgPool, who: &User, status_id: Uuid) {
    let _filed = bookmark::add(pool, who.id, status_id, None)
        .await
        .expect("the fixture bookmark stores");
}

/// Files a fresh bookmark of `status_id` by `who` in `folder` — the
/// state the `pleroma.bookmark_folder` test starts from.
async fn bookmark_in(pool: &PgPool, who: &User, status_id: Uuid, folder: Uuid) {
    let filed = bookmark::add(pool, who.id, status_id, Some(folder))
        .await
        .expect("the fixture bookmark files");
    assert_eq!(filed.folder_id, Some(folder), "the row filed as asked");
}

/// The reactions ticket's loader inside the fixed query set: one page of
/// three rows — two posts and a reblog of one of them — renders every
/// roll-up from the same call, `me` follows the viewer, and the nested
/// original of a reblog carries its own reactions.
#[tokio::test]
async fn the_reactions_roll_up_in_the_render_batch() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let first = post(&pool, &alice, "first").await;
    let second = post(&pool, &alice, "second").await;
    let reblog = insert(
        &pool,
        BASE,
        &NewStatus {
            reblog_of_id: Some(second.id),
            ..NewStatus::local(&bob, "")
        },
    )
    .await
    .expect("a reblog of a stored post inserts");
    react(&pool, &alice, &first, THUMB, None).await;
    react(&pool, &bob, &first, CAT, Some(CAT_URL)).await;
    react(&pool, &bob, &second, THUMB, None).await;

    let page = [first.clone(), reblog.clone()];
    let for_bob = render_statuses(&state, ViewerMode::user(&bob), &page)
        .await
        .expect("the page renders for bob");
    let reblog_json = serde_json::to_value(&for_bob[1]).expect("the reblog serializes");
    assert!(
        !reblog_json["reblog"].is_null(),
        "the reblog row nests the original it reblogs"
    );
    assert_eq!(
        reblog_json["reblog"]["pleroma"]["emoji_reactions"],
        json!([{
            "name": THUMB, "count": 1, "me": true,
            "url": null, "account_ids": [bob.id.to_string()],
        }]),
        "the nested original's own roll-up, from the same batch"
    );
    assert_eq!(
        reactions(&for_bob[0]),
        json!([
            {
                "name": THUMB,
                "count": 1,
                "me": false,
                "url": null,
                "account_ids": [alice.id.to_string()],
            },
            {
                "name": "cat",
                "count": 1,
                "me": true,
                "url": CAT_URL,
                "account_ids": [bob.id.to_string()],
            },
        ]),
        "both of the first post's emojis, each flagged for the viewer \
         who reacted with one of them"
    );

    let anonymous = render_statuses(&state, ViewerMode::ANONYMOUS, std::slice::from_ref(&first))
        .await
        .expect("the post renders anonymously");
    assert_eq!(
        serde_json::to_value(&anonymous[0]).expect("it serializes")["pleroma"]["emoji_reactions"]
            .as_array()
            .map(|entries| entries.iter().all(|entry| entry["me"] == json!(false))),
        Some(true),
        "an anonymous render is nobody's `me`"
    );
}

/// The favourites ticket's loader inside the fixed query set: one
/// batched read answers the `favourited` flag of a whole page — plain
/// rows, reblog rows (whose like lives on the original) and the nested
/// originals alike — and an anonymous page is nobody's favourite.
#[tokio::test]
async fn the_favourited_flag_follows_one_batched_read() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let liked = post(&pool, &bob, "liked").await;
    let ignored = post(&pool, &bob, "ignored").await;
    let original = post(&pool, &bob, "original").await;
    let reblog = insert(
        &pool,
        BASE,
        &NewStatus {
            reblog_of_id: Some(original.id),
            ..NewStatus::local(&alice, "")
        },
    )
    .await
    .expect("a reblog of a stored post inserts");
    favourite_it(&pool, &alice, liked.id).await;
    favourite_it(&pool, &alice, original.id).await;

    let flag = |entity: &Status| {
        serde_json::to_value(entity).expect("a rendered status serializes")["favourited"].clone()
    };
    let page = [liked.clone(), ignored.clone(), reblog.clone()];
    let for_alice = render_statuses(&state, ViewerMode::user(&alice), &page)
        .await
        .expect("the page renders for alice");
    assert_eq!(flag(&for_alice[0]), json!(true), "alice's own like shows");
    assert_eq!(
        flag(&for_alice[1]),
        json!(false),
        "and the unliked post does not"
    );
    let reblog_json = serde_json::to_value(&for_alice[2]).expect("the reblog serializes");
    assert_eq!(
        reblog_json["favourited"],
        json!(true),
        "the reblog row is flagged through its original, where the \
         like lives"
    );
    assert_eq!(
        reblog_json["reblog"]["favourited"],
        json!(true),
        "and the nested original carries its own flag"
    );

    let for_bob = render_statuses(&state, ViewerMode::user(&bob), std::slice::from_ref(&liked))
        .await
        .expect("the post renders for bob");
    assert_eq!(
        flag(&for_bob[0]),
        json!(false),
        "alice's like is nobody else's flag"
    );
    let anonymous = render_statuses(&state, ViewerMode::ANONYMOUS, &page)
        .await
        .expect("the page renders anonymously");
    assert!(
        anonymous.iter().all(|entity| flag(entity) == json!(false)),
        "an anonymous page is nobody's favourite"
    );
}

/// The reblogs ticket's loader inside the fixed query set: one batched
/// read answers the `reblogged` flag of a whole page — the original the
/// viewer boosted, a plain post they left alone, and, keyed through the
/// original exactly like the favourites flag, the viewer's own reblog
/// row and the original nested in it. An anonymous page is nobody's
/// boost.
#[tokio::test]
async fn the_reblogged_flag_follows_one_batched_read() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let boosted = post(&pool, &bob, "boosted").await;
    let ignored = post(&pool, &bob, "ignored").await;
    let (reblog, created) =
        status::create_reblog(&pool, BASE, &alice, &boosted, Visibility::Public)
            .await
            .expect("alice boosts bob's post");
    assert!(created, "the boost is written by the reblog model");

    let flag = |entity: &Status| {
        serde_json::to_value(entity).expect("a rendered status serializes")["reblogged"].clone()
    };
    let page = [boosted.clone(), ignored.clone(), reblog.clone()];
    let for_alice = render_statuses(&state, ViewerMode::user(&alice), &page)
        .await
        .expect("the page renders for alice");
    assert_eq!(
        flag(&for_alice[0]),
        json!(true),
        "alice's own boost flags the original"
    );
    assert_eq!(
        flag(&for_alice[1]),
        json!(false),
        "and the unboosted post does not"
    );
    let reblog_json = serde_json::to_value(&for_alice[2]).expect("the reblog serializes");
    assert_eq!(
        reblog_json["reblogged"],
        json!(true),
        "the reblog row is flagged through its original, where the \
         boost lives — the shape POST …/reblog answers with"
    );
    assert_eq!(
        reblog_json["reblog"]["reblogged"],
        json!(true),
        "and the nested original carries its own flag"
    );

    let for_bob = render_statuses(
        &state,
        ViewerMode::user(&bob),
        std::slice::from_ref(&boosted),
    )
    .await
    .expect("the post renders for bob");
    assert_eq!(
        flag(&for_bob[0]),
        json!(false),
        "alice's boost is nobody else's flag"
    );
    let anonymous = render_statuses(&state, ViewerMode::ANONYMOUS, &page)
        .await
        .expect("the page renders anonymously");
    assert!(
        anonymous.iter().all(|entity| flag(entity) == json!(false)),
        "an anonymous page is nobody's boost"
    );
}

/// The bookmarks ticket's loader inside the fixed query set: one
/// batched read answers the `bookmarked` flag of a whole page — and,
/// unlike the favourites flag, a reblog row is flagged from its *own*
/// id, the row its client bookmarked, while the nested original stays
/// unflagged unless it was itself bookmarked. An anonymous page is
/// nobody's bookmark.
#[tokio::test]
async fn the_bookmarked_flag_follows_one_batched_read() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let saved = post(&pool, &bob, "saved").await;
    let ignored = post(&pool, &bob, "ignored").await;
    let original = post(&pool, &bob, "original").await;
    let reblog = insert(
        &pool,
        BASE,
        &NewStatus {
            reblog_of_id: Some(original.id),
            ..NewStatus::local(&alice, "")
        },
    )
    .await
    .expect("a reblog of a stored post inserts");
    bookmark_it(&pool, &alice, saved.id).await;
    bookmark_it(&pool, &alice, reblog.id).await;

    let flag = |entity: &Status| {
        serde_json::to_value(entity).expect("a rendered status serializes")["bookmarked"].clone()
    };
    let page = [saved.clone(), ignored.clone(), reblog.clone()];
    let for_alice = render_statuses(&state, ViewerMode::user(&alice), &page)
        .await
        .expect("the page renders for alice");
    assert_eq!(flag(&for_alice[0]), json!(true), "alice's bookmark shows");
    assert_eq!(
        flag(&for_alice[1]),
        json!(false),
        "and the unbookmarked post does not"
    );
    let reblog_json = serde_json::to_value(&for_alice[2]).expect("the reblog serializes");
    assert_eq!(
        reblog_json["bookmarked"],
        json!(true),
        "the reblog row is flagged by its own id — a bookmark lands \
         where the client named it"
    );
    assert_eq!(
        reblog_json["reblog"]["bookmarked"],
        json!(false),
        "and the nested original, never bookmarked, is not"
    );

    let for_bob = render_statuses(&state, ViewerMode::user(&bob), std::slice::from_ref(&saved))
        .await
        .expect("the post renders for bob");
    assert_eq!(
        flag(&for_bob[0]),
        json!(false),
        "alice's bookmark is nobody else's flag"
    );
    let anonymous = render_statuses(&state, ViewerMode::ANONYMOUS, &page)
        .await
        .expect("the page renders anonymously");
    assert!(
        anonymous.iter().all(|entity| flag(entity) == json!(false)),
        "an anonymous page is nobody's bookmark"
    );
}

/// The folders ticket's half of the bookmarks read inside the fixed
/// query set: one batched statement answers both the `bookmarked` flag
/// and `pleroma.bookmark_folder` — a filed bookmark renders its folder
/// id, an unfiled one and a foreign viewer render `null`, and, like the
/// flag, the folder is keyed to the row's *own* id, so the original
/// nested in a boost carries only its own bookmark's folder.
#[tokio::test]
async fn the_bookmark_folder_rides_the_batched_bookmark_read() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let folder = bookmark_folder::create(&pool, alice.id, "shelved", None)
        .await
        .expect("the fixture folder creates");
    let filed = post(&pool, &bob, "filed").await;
    let unfiled = post(&pool, &bob, "unfiled").await;
    bookmark_in(&pool, &alice, filed.id, folder.id).await;
    bookmark_it(&pool, &alice, unfiled.id).await;

    let folder_of = |entity: &Status| {
        serde_json::to_value(entity).expect("a rendered status serializes")["pleroma"]
            ["bookmark_folder"]
            .clone()
    };
    let page = [filed.clone(), unfiled.clone()];
    let for_alice = render_statuses(&state, ViewerMode::user(&alice), &page)
        .await
        .expect("the page renders for alice");
    assert_eq!(
        folder_of(&for_alice[0]),
        json!(folder.id.to_string()),
        "the filed bookmark renders its folder id"
    );
    assert_eq!(
        folder_of(&for_alice[1]),
        Value::Null,
        "the unfiled bookmark renders null while staying bookmarked"
    );
    assert_eq!(
        serde_json::to_value(&for_alice[1]).expect("it serializes")["bookmarked"],
        json!(true),
        "and the flag and the folder come from the one read"
    );

    let for_bob = render_statuses(&state, ViewerMode::user(&bob), std::slice::from_ref(&filed))
        .await
        .expect("the post renders for bob");
    assert_eq!(
        folder_of(&for_bob[0]),
        Value::Null,
        "alice's folder is nobody else's key"
    );
    let anonymous = render_statuses(&state, ViewerMode::ANONYMOUS, &page)
        .await
        .expect("the page renders anonymously");
    assert!(
        anonymous
            .iter()
            .all(|entity| folder_of(entity) == Value::Null),
        "an anonymous page carries nobody's folder"
    );
}
