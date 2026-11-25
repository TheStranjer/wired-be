//! v2 filters applied to rendered listings: a `warn` filter annotates a
//! matching status with the v2 `FilterResult`, a `hide` filter removes it
//! from timelines and notifications, and contexts, whole-word matching,
//! expiry and single-status entries decide what matches. The v1 routes read
//! the same storage, so each side sees the other's filters.

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;
use wired_be::models::{follow, status::Visibility};

use super::support::{
    BASE, HOME, NOTES, PUBLIC, ROOT_V2, create_json, destroy, filtered_of, fixture, id_in, id_of,
    ids, listed, newest_first, page_of, v2_create, v2_get, v2_post,
};
use crate::common;

/// The scopes the tests read timelines and manage filters with.
const SCOPES: &str = "read:statuses read:notifications read:filters write:filters";

/// A v2 filter body for `keywords` in `contexts` doing `action`.
fn filter_body(action: &str, contexts: &[&str], keywords: &[&str]) -> Value {
    let attributes: Vec<Value> = keywords
        .iter()
        .map(|word| json!({"keyword": word, "whole_word": true}))
        .collect();
    json!({
        "title": "Words",
        "context": contexts,
        "filter_action": action,
        "keywords_attributes": attributes,
    })
}

/// A `warn` filter annotates the matching home status with the v2 result —
/// the filter, the matched keywords and a `null` status match — while the
/// rest of the page keeps its empty array.
#[tokio::test]
async fn a_v2_warn_filter_annotates_the_matching_home_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", SCOPES).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _followed = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows bob");
    let spammy = common::create_status(&pool, &bob, "a spam post", Visibility::Public).await;
    let clean = common::create_status(&pool, &bob, "quiet evening", Visibility::Public).await;
    let app = common::test_app(pool);
    let filter = v2_create(&app, &token, filter_body("warn", &["home"], &["spam"])).await;

    let body = page_of(&app, &token, HOME).await;
    assert_eq!(
        ids(&body),
        newest_first(&[&spammy, &clean]),
        "a warning removes nothing"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&spammy)),
        json!([{
            "filter": filter,
            "keyword_matches": ["spam"],
            "status_matches": Value::Null,
        }]),
        "the match carries the whole v2 filter and what matched"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&clean)),
        json!([]),
        "the other status is not annotated"
    );
}

/// The acceptance path: a `hide` filter made through `POST /api/v2/filters`
/// drops the matching post from home and shows in `GET /api/v1/filters`.
#[tokio::test]
async fn a_v2_hide_filter_drops_the_match_and_appears_in_v1() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", SCOPES).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _followed = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows bob");
    let _spammy = common::create_status(&pool, &bob, "a spam post", Visibility::Public).await;
    let clean = common::create_status(&pool, &bob, "quiet evening", Visibility::Public).await;
    let app = common::test_app(pool);
    let filter = v2_create(&app, &token, filter_body("hide", &["home"], &["spam"])).await;

    let body = page_of(&app, &token, HOME).await;
    assert_eq!(
        ids(&body),
        newest_first(&[&clean]),
        "only the clean post stays"
    );

    let v1 = listed(&app, &token).await;
    assert_eq!(v1.len(), 1, "the v1 list holds the filter");
    assert_eq!(
        v1[0]["id"], filter["keywords"][0]["id"],
        "under the keyword's id"
    );
    assert_eq!(v1[0]["phrase"], "spam", "its phrase");
    assert_eq!(v1[0]["irreversible"], true, "`hide` reads as irreversible");
    assert_eq!(v1[0]["context"], json!(["home"]), "its contexts");
}

/// A `hide` filter in `notifications` drops the notification whose status
/// it matched.
#[tokio::test]
async fn a_v2_hide_filter_drops_matching_notifications() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", SCOPES).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let spammy = common::create_status(&pool, &bob, "a spam post", Visibility::Public).await;
    let _note = common::create_notification(&pool, &alice, &bob, "mention", Some(&spammy)).await;
    let app = common::test_app(pool);

    let before = page_of(&app, &token, NOTES).await;
    assert_eq!(before.as_array().map(Vec::len), Some(1), "one notification");
    let _filter = v2_create(
        &app,
        &token,
        filter_body("hide", &["notifications"], &["spam"]),
    )
    .await;
    let after = page_of(&app, &token, NOTES).await;
    assert_eq!(after, json!([]), "the filter takes it out of the listing");
}

/// A filter applies only in the contexts it names: `public`-only leaves
/// home alone and annotates the public timeline.
#[tokio::test]
async fn a_v2_filter_applies_only_in_its_contexts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", SCOPES).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _followed = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows bob");
    let spammy = common::create_status(&pool, &bob, "a spam post", Visibility::Public).await;
    let app = common::test_app(pool);
    let _filter = v2_create(&app, &token, filter_body("warn", &["public"], &["spam"])).await;

    let home = page_of(&app, &token, HOME).await;
    assert_eq!(
        filtered_of(&home, &id_of(&spammy)),
        json!([]),
        "home is not a named context"
    );
    let public = page_of(&app, &token, PUBLIC).await;
    assert_eq!(
        filtered_of(&public, &id_of(&spammy))
            .as_array()
            .map(Vec::len),
        Some(1),
        "the public timeline is"
    );
}

/// Whole-word matching is per keyword: a whole-word keyword skips a glued
/// occurrence, a substring keyword does not, and each match lists only the
/// keywords that matched.
#[tokio::test]
async fn keywords_match_whole_words_or_substrings_each_on_their_own() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", SCOPES).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _followed = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows bob");
    let glued = common::create_status(&pool, &bob, "the foood", Visibility::Public).await;
    let inside = common::create_status(&pool, &bob, "xbarx", Visibility::Public).await;
    let bounded = common::create_status(&pool, &bob, "a foo here", Visibility::Public).await;
    let app = common::test_app(pool);
    let _filter = v2_create(
        &app,
        &token,
        json!({
            "title": "Mixed",
            "context": ["home"],
            "keywords_attributes": [
                {"keyword": "foo", "whole_word": true},
                {"keyword": "bar", "whole_word": false},
            ],
        }),
    )
    .await;

    let body = page_of(&app, &token, HOME).await;
    let matched = |post| filtered_of(&body, &id_of(post))[0]["keyword_matches"].clone();
    assert_eq!(
        filtered_of(&body, &id_of(&glued)),
        json!([]),
        "no whole word"
    );
    assert_eq!(matched(&inside), json!(["bar"]), "the substring keyword");
    assert_eq!(matched(&bounded), json!(["foo"]), "the whole-word keyword");
}

/// Expiry: a filter made with `expires_in` applies while live; once its
/// expiry is back-dated it stops applying and leaves the v1 list, but the
/// v2 list still shows it, as Mastodon's does.
#[tokio::test]
async fn an_expired_v2_filter_stops_applying() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", SCOPES).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _followed = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows bob");
    let spammy = common::create_status(&pool, &bob, "a spam post", Visibility::Public).await;
    let app = common::test_app(pool.clone());
    let mut body = filter_body("hide", &["home"], &["spam"]);
    body["expires_in"] = json!(3600);
    let filter = v2_create(&app, &token, body).await;
    assert!(filter["expires_at"].is_string(), "the expiry is stamped");

    let live = page_of(&app, &token, HOME).await;
    assert!(ids(&live).is_empty(), "while live it hides the post");

    let _backdated = sqlx::query("UPDATE custom_filters SET expires_at = now() - interval '1 day'")
        .execute(&pool)
        .await
        .expect("the expiry is back-dated");
    let expired = page_of(&app, &token, HOME).await;
    assert_eq!(
        ids(&expired),
        vec![id_of(&spammy)],
        "once expired the post is back"
    );
    assert!(
        listed(&app, &token).await.is_empty(),
        "the v1 list drops it"
    );
    let v2 = v2_get(&app, &token, ROOT_V2).await.json_value();
    assert_eq!(v2.as_array().map(Vec::len), Some(1), "the v2 list keeps it");
}

/// A status entry hides exactly that status, with no keyword involved.
#[tokio::test]
async fn a_status_entry_hides_that_status_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", SCOPES).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _followed = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows bob");
    let target = common::create_status(&pool, &bob, "one", Visibility::Public).await;
    let other = common::create_status(&pool, &bob, "two", Visibility::Public).await;
    let app = common::test_app(pool);
    let filter = v2_create(
        &app,
        &token,
        json!({"title": "One", "context": ["home"], "filter_action": "hide"}),
    )
    .await;

    let added = v2_post(
        &app,
        &token,
        &format!("{ROOT_V2}/{}/statuses", id_in(&filter)),
        json!({"status_id": id_of(&target)}),
    )
    .await;
    assert_eq!(added.status, StatusCode::OK, "the entry is added");
    let body = page_of(&app, &token, HOME).await;
    assert_eq!(ids(&body), newest_first(&[&other]), "only the other stays");
}

/// v1 and v2 share storage: a v1 filter is a v2 filter with one keyword,
/// a v2 filter with several keywords is several v1 filters, and deleting
/// one v1 filter takes one keyword.
#[tokio::test]
async fn v1_and_v2_see_each_others_filters() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", SCOPES).await;
    let app = common::test_app(pool);

    let v1 = create_json(
        &app,
        &token,
        json!({"phrase": "foo", "context": ["home"], "irreversible": true}),
    )
    .await
    .json_value();
    let v2 = v2_get(&app, &token, ROOT_V2).await.json_value();
    assert_eq!(v2[0]["title"], "foo", "the phrase titles the v2 filter");
    assert_eq!(v2[0]["filter_action"], "hide", "irreversible is `hide`");
    assert_eq!(
        v2[0]["keywords"][0]["id"], v1["id"],
        "the keyword has the v1 id"
    );

    let group = v2_create(
        &app,
        &token,
        filter_body("warn", &["public"], &["one", "two"]),
    )
    .await;
    let phrases: Vec<String> = listed(&app, &token)
        .await
        .iter()
        .map(|entry| entry["phrase"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(phrases, ["two", "one", "foo"], "one v1 filter per keyword");

    let first = id_in(&group["keywords"][0]);
    let removed = destroy(&app, &token, first.parse::<Uuid>().expect("an id")).await;
    assert_eq!(removed.status, StatusCode::OK, "v1 delete works");
    let shown = v2_get(&app, &token, &format!("{ROOT_V2}/{}", id_in(&group))).await;
    assert_eq!(
        shown.json_value()["keywords"].as_array().map(Vec::len),
        Some(1),
        "and took one keyword of the group"
    );
}
