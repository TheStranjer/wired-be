//! Scenarios 1-3: favourite/unfavourite, reblog/undo, and the bookmark
//! with its Pleroma folder, each read back through every surface.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};

use super::support::{
    BOOKMARKS, FAVOURITES, FOLDERS, HOME, STATUSES, UNKNOWN_ID, acted, assert_every, assert_gone,
    entity_id, follows, get, ids_of, post, posted, representations, row_of, user,
};
use crate::common;

// ────────────────────  Scenario (1): favourite  ────────────────────

/// A like steps `favourited` and `favourites_count` on every
/// representation, lands in `GET /api/v1/favourites`, and an unlike walks
/// the whole set back down.
#[tokio::test]
async fn a_favourite_moves_the_count_the_flag_and_the_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    let id = entity_id(&post(&app, &alice_token, "favourite me").await);
    follows(&app, &bob_token, &alice.id.to_string()).await;
    let (on, off) = (json!(true), json!(false));
    let (one, zero) = (json!(1), json!(0));

    assert_every(
        &representations(&app, Some(&bob_token), &id).await,
        &["favourited"],
        &off,
        "the like before it is liked",
    );

    let liked = acted(&app, &bob_token, &format!("{STATUSES}/{id}/favourite")).await;
    assert_eq!(liked["favourited"], on, "the write flags the like");
    assert_eq!(liked["favourites_count"], one, "and counts it once");
    let again = acted(&app, &bob_token, &format!("{STATUSES}/{id}/favourite")).await;
    assert_eq!(
        again["favourites_count"], one,
        "a repeat never counts twice"
    );

    let mine = representations(&app, Some(&bob_token), &id).await;
    assert_every(&mine, &["favourited"], &on, "the liker's reads");
    assert_every(&mine, &["favourites_count"], &one, "the liker's reads");
    let strangers = representations(&app, None, &id).await;
    assert_every(&strangers, &["favourited"], &off, "a stranger's reads");
    assert_every(&strangers, &["favourites_count"], &one, "the public count");

    let listed = get(&app, Some(&bob_token), FAVOURITES).await.json_value();
    assert_eq!(
        ids_of(&listed),
        vec![id.clone()],
        "the favourites list round-trips the like"
    );
    assert_eq!(
        row_of(&listed, &id, "favourites")["favourited"],
        on,
        "listed as the liker's own"
    );

    let unlike = acted(&app, &bob_token, &format!("{STATUSES}/{id}/unfavourite")).await;
    assert_eq!(unlike["favourited"], off, "the withdrawal clears the flag");
    assert_eq!(unlike["favourites_count"], zero, "and steps the count back");
    assert_every(
        &representations(&app, Some(&bob_token), &id).await,
        &["favourites_count"],
        &zero,
        "every read agrees the unlike",
    );
    let emptied = get(&app, Some(&bob_token), FAVOURITES).await.json_value();
    assert!(
        ids_of(&emptied).is_empty(),
        "the favourites list empties with it"
    );
}

// ─────────────────────  Scenario (2): reblog  ─────────────────────

/// A boost nests the original, steps `reblogs_count` and `reblogged` on
/// every representation, and rides the follower's home timeline exactly
/// once; the undo walks it all back and takes the boost row down.
#[tokio::test]
async fn a_reblog_nests_the_original_and_rides_the_follower_home_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (bob, bob_token) = user(&pool, "bob").await;
    let (_carol, carol_token) = user(&pool, "carol").await;
    let id = entity_id(&post(&app, &alice_token, "boost me").await);
    follows(&app, &bob_token, &alice.id.to_string()).await;
    follows(&app, &carol_token, &bob.id.to_string()).await;

    let boost = acted(&app, &bob_token, &format!("{STATUSES}/{id}/reblog")).await;
    let boost_id = entity_id(&boost);
    assert_ne!(boost_id, id, "the answer is the boost, not the post");
    assert_eq!(
        boost["content"], "",
        "the boost row carries no text of its own"
    );
    assert_eq!(boost["reblog"]["id"], id, "the original nests inside");
    assert_eq!(
        boost["reblog"]["reblogs_count"],
        json!(1),
        "and counts the boost"
    );
    assert_eq!(
        boost["reblogged"],
        json!(true),
        "one's own boost flags itself"
    );

    let boosted = representations(&app, Some(&bob_token), &id).await;
    assert_every(
        &boosted,
        &["reblogs_count"],
        &json!(1),
        "the booster's reads",
    );
    assert_every(
        &boosted,
        &["reblogged"],
        &json!(true),
        "the booster's reads",
    );
    let strangers = representations(&app, None, &id).await;
    assert_every(
        &strangers,
        &["reblogs_count"],
        &json!(1),
        "the count every reader sees",
    );
    assert_every(
        &strangers,
        &["reblogged"],
        &json!(false),
        "nobody else boosted it",
    );

    let home = get(&app, Some(&carol_token), HOME).await.json_value();
    assert_eq!(
        ids_of(&home),
        vec![boost_id.clone()],
        "the follower's home carries the boost exactly once"
    );
    assert_eq!(
        row_of(&home, &boost_id, "the follower's home")["reblog"]["id"],
        id,
        "boosting the post it names"
    );

    let undo = acted(&app, &bob_token, &format!("{STATUSES}/{id}/unreblog")).await;
    assert_eq!(undo["id"], id, "the withdrawal answers the original");
    assert_eq!(undo["reblogs_count"], json!(0), "its count steps back");
    assert_eq!(undo["reblogged"], json!(false), "and the flag clears");
    assert_gone(&app, Some(&bob_token), &boost_id, "the withdrawn boost").await;
    let settled = get(&app, Some(&carol_token), HOME).await.json_value();
    assert!(
        ids_of(&settled).is_empty(),
        "the boost left the home with it"
    );
}

// ──────────────────  Scenario (3): bookmark + folder  ──────────────────

/// A Pleroma folder created over HTTP files a bookmark through the
/// bookmark write; the filing renders as `pleroma.bookmark_folder` on
/// every representation and round-trips through `GET /api/v1/bookmarks`
/// and its `folder_id` filter.
#[tokio::test]
async fn a_filed_bookmark_round_trips_the_folder_and_the_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    let id = entity_id(&post(&app, &alice_token, "save this").await);
    follows(&app, &bob_token, &alice.id.to_string()).await;
    let folder_id = create_folder(&app, &bob_token, "Reading list").await;

    let filed = posted(
        &app,
        &bob_token,
        &format!("{STATUSES}/{id}/bookmark"),
        json!({"folder_id": folder_id}),
    )
    .await;
    assert_eq!(
        filed["bookmarked"],
        json!(true),
        "the write flags the bookmark"
    );
    assert_eq!(
        filed["pleroma"]["bookmark_folder"],
        json!(folder_id),
        "and files it in the folder"
    );

    assert_filing(
        &app,
        Some(&bob_token),
        &id,
        &json!(folder_id.clone()),
        "the bookmarker",
    )
    .await;
    assert_filing(&app, None, &id, &Value::Null, "a stranger").await;

    assert_bookmarks(&app, &bob_token, "", std::slice::from_ref(&id)).await;
    let listed = bookmarks_page(&app, &bob_token, "").await;
    assert_eq!(
        row_of(&listed, &id, "bookmarks")["pleroma"]["bookmark_folder"],
        json!(folder_id),
        "listed as filed"
    );
    assert_bookmarks(
        &app,
        &bob_token,
        &format!("folder_id={folder_id}"),
        std::slice::from_ref(&id),
    )
    .await;
    assert_bookmarks(&app, &bob_token, &format!("folder_id={UNKNOWN_ID}"), &[]).await;

    let freed = acted(&app, &bob_token, &format!("{STATUSES}/{id}/unbookmark")).await;
    assert_eq!(
        freed["bookmarked"],
        json!(false),
        "the withdrawal clears the flag"
    );
    assert_eq!(
        freed["pleroma"]["bookmark_folder"],
        Value::Null,
        "and the filing"
    );
    assert_bookmarks(&app, &bob_token, "", &[]).await;
}

/// A Pleroma bookmark folder named `name`, created over HTTP and shown
/// back on the folder list; answers its id.
async fn create_folder(app: &Router, token: &str, name: &str) -> String {
    let folder = posted(app, token, FOLDERS, json!({"name": name})).await;
    assert_eq!(folder["name"], json!(name), "the folder answers its name");
    let folder_id = entity_id(&folder);
    let shelves = get(app, Some(token), FOLDERS).await.json_value();
    assert_eq!(
        ids_of(&shelves),
        vec![folder_id.clone()],
        "the folder list holds it, in creation order"
    );
    folder_id
}

/// The `GET /api/v1/bookmarks` page narrowed by `query` (empty: the whole
/// list), asserted 200 and answered as JSON.
async fn bookmarks_page(app: &Router, token: &str, query: &str) -> Value {
    let path = if query.is_empty() {
        BOOKMARKS.to_owned()
    } else {
        format!("{BOOKMARKS}?{query}")
    };
    let response = get(app, Some(token), &path).await;
    common::assert_answer(&response, StatusCode::OK, "the bookmarks list reads");
    response.json_value()
}

/// The bookmarks page for `query` answers exactly the ids in `want`.
async fn assert_bookmarks(app: &Router, token: &str, query: &str, want: &[String]) {
    assert_eq!(
        ids_of(&bookmarks_page(app, token, query).await),
        want,
        "the bookmarks list answers"
    );
}

/// The bookmark pair every representation of `id` answers as the reader
/// behind `token`: `bookmarked` is the folder's presence and
/// `pleroma.bookmark_folder` is `folder_id` — which is why the two can
/// never disagree, the case named by `who`.
async fn assert_filing(app: &Router, token: Option<&str>, id: &str, folder_id: &Value, who: &str) {
    let seen = representations(app, token, id).await;
    let flag = json!(!folder_id.is_null());
    assert_every(&seen, &["bookmarked"], &flag, who);
    assert_every(&seen, &["pleroma", "bookmark_folder"], folder_id, who);
}
