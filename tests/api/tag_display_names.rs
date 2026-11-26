//! The Tag entity keeps the casing of the hashtag's first use (WBE-879).
//!
//! `GET /api/v1/tags/{name}`, the v2 hashtag search, `followed_tags` and the
//! featured-tag suggestions render the display name as `name`, as Mastodon's
//! `TagSerializer#name` does; the `url` stays lowercase, a later use in
//! another casing changes nothing, and a Status entity's `tags[].name` stays
//! lowercase.

#![cfg(test)]

use axum::http::StatusCode;
use serde_json::Value;
use wired_be::models::{hashtag, hashtag_follow, status::Visibility};

use crate::common;

/// The instance origin `test_config` serves as.
const BASE: &str = "https://test.example";

/// `GET path` as `bearer`, or anonymously.
async fn get(app: &axum::Router, bearer: Option<&str>, path: &str) -> common::TestResponse {
    let request = common::TestRequest::get(path);
    match bearer {
        Some(token) => request.bearer(token).send(app).await,
        None => request.send(app).await,
    }
}

/// The first use's casing is the tag's `name` and a later use keeps it.
#[tokio::test]
async fn the_tag_endpoint_shows_the_first_use_casing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let _first = common::create_status(&pool, &alice, "hello #R5Smoke", Visibility::Public).await;
    let _later = common::create_status(&pool, &alice, "again #r5smoke", Visibility::Public).await;

    let shown = get(&app, None, "/api/v1/tags/r5smoke").await;
    common::assert_answer(&shown, StatusCode::OK, "the tag lookup");
    let body = shown.json_value();
    assert_eq!(body["name"], "R5Smoke", "the first use's casing");
    assert_eq!(
        body["url"],
        format!("{BASE}/tags/r5smoke"),
        "the url stays lowercase"
    );

    let mixed = get(&app, None, "/api/v1/tags/R5SMOKE").await;
    assert_eq!(
        mixed.json_value()["name"],
        "R5Smoke",
        "matching is case-insensitive and the casing is the stored one"
    );
}

/// A tag first used lowercase keeps showing lowercase after a cased use.
#[tokio::test]
async fn a_later_cased_use_does_not_change_the_name() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let _first = common::create_status(&pool, &alice, "#r5smoke", Visibility::Public).await;
    let _later = common::create_status(&pool, &alice, "#R5Smoke", Visibility::Public).await;

    let shown = get(&app, None, "/api/v1/tags/R5Smoke").await;
    assert_eq!(shown.json_value()["name"], "r5smoke", "the first use wins");
}

/// The v2 hashtag search renders the same display name.
#[tokio::test]
async fn the_hashtag_search_shows_the_first_use_casing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let _first = common::create_status(&pool, &alice, "#R5Smoke", Visibility::Public).await;

    let found = get(&app, None, "/api/v2/search?q=%23R5Smoke&type=hashtags").await;
    common::assert_answer(&found, StatusCode::OK, "the v2 hashtag search");
    let body = found.json_value();
    assert_eq!(body["hashtags"][0]["name"], "R5Smoke", "the display name");
    assert_eq!(
        body["hashtags"][0]["url"],
        format!("{BASE}/tags/r5smoke"),
        "the url stays lowercase"
    );
}

/// `followed_tags` renders the display name too.
#[tokio::test]
async fn followed_tags_show_the_first_use_casing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let _first = common::create_status(&pool, &alice, "#R5Smoke", Visibility::Public).await;
    let tag = hashtag::get_by_name(&pool, "r5smoke")
        .await
        .expect("the lookup runs")
        .expect("the status created the tag");
    let new = hashtag_follow::follow(&pool, alice.id, tag.id)
        .await
        .expect("the fixture follow writes");
    assert_eq!(new, 1, "the fixture follow is new");
    let token = common::create_app_and_token(&pool, &alice, "read:follows").await;

    let listed = get(&app, Some(&token), "/api/v1/followed_tags").await;
    common::assert_answer(&listed, StatusCode::OK, "the followed tags");
    let tags = listed.json::<Vec<Value>>();
    assert_eq!(tags[0]["name"], "R5Smoke", "the display name");
    assert_eq!(
        tags[0]["url"],
        format!("{BASE}/tags/r5smoke"),
        "the url stays lowercase"
    );
}

/// The featured-tag suggestions render the display name.
#[tokio::test]
async fn featured_tag_suggestions_show_the_first_use_casing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let _first = common::create_status(&pool, &alice, "#R5Smoke", Visibility::Public).await;
    let token = common::create_app_and_token(&pool, &alice, "read:accounts").await;

    let suggested = get(&app, Some(&token), "/api/v1/featured_tags/suggestions").await;
    common::assert_answer(&suggested, StatusCode::OK, "the suggestions");
    let tags = suggested.json::<Vec<Value>>();
    assert_eq!(tags[0]["name"], "R5Smoke", "the display name");
}

/// A Status entity's `tags[].name` stays lowercase.
#[tokio::test]
async fn status_tags_stay_lowercase() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let posted = common::create_status(&pool, &alice, "#R5Smoke", Visibility::Public).await;

    let shown = get(&app, None, &format!("/api/v1/statuses/{}", posted.id)).await;
    common::assert_answer(&shown, StatusCode::OK, "the status");
    assert_eq!(
        shown.json_value()["tags"][0]["name"],
        "r5smoke",
        "the status render is lowercase, as Mastodon's ShallowTagSerializer"
    );
}

/// `POST` of an absolute path as `bearer`.
async fn post(app: &axum::Router, bearer: &str, path: &str) -> common::TestResponse {
    common::TestRequest::post(path)
        .bearer(bearer)
        .send(app)
        .await
}

/// Following a new mixed-case tag stores the written casing (WBE-885).
#[tokio::test]
async fn following_a_new_tag_keeps_the_written_casing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:follows write:follows").await;

    let followed = post(&app, &token, "/api/v1/tags/R6FollowTag/follow").await;
    common::assert_answer(&followed, StatusCode::OK, "the follow");
    let body = followed.json_value();
    assert_eq!(body["name"], "R6FollowTag", "the follow answers the casing");
    assert_eq!(
        body["url"],
        format!("{BASE}/tags/r6followtag"),
        "the url stays lowercase"
    );

    let listed = get(&app, Some(&token), "/api/v1/followed_tags").await;
    common::assert_answer(&listed, StatusCode::OK, "the followed tags");
    assert_eq!(
        listed.json::<Vec<Value>>()[0]["name"],
        "R6FollowTag",
        "followed_tags lists the casing"
    );

    let shown = get(&app, None, "/api/v1/tags/r6followtag").await;
    common::assert_answer(&shown, StatusCode::OK, "the lowercase lookup");
    assert_eq!(
        shown.json_value()["name"],
        "R6FollowTag",
        "and so does show"
    );
}

/// Following `#foo` when `Foo` exists keeps `Foo`.
#[tokio::test]
async fn following_an_existing_tag_keeps_its_casing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let _first = common::create_status(&pool, &alice, "#Foo", Visibility::Public).await;
    let token = common::create_app_and_token(&pool, &alice, "read:follows write:follows").await;

    let followed = post(&app, &token, "/api/v1/tags/foo/follow").await;
    common::assert_answer(&followed, StatusCode::OK, "the follow");
    assert_eq!(followed.json_value()["name"], "Foo", "the first use wins");
}

/// An unusable name is still the 422, and nothing is stored.
#[tokio::test]
async fn following_an_invalid_name_is_still_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:follows write:follows").await;

    for name in ["123", "a%2Fb", "%23"] {
        let refused = post(&app, &token, &format!("/api/v1/tags/{name}/follow")).await;
        common::assert_answer(&refused, StatusCode::UNPROCESSABLE_ENTITY, "the bad name");
    }
}

/// Featuring a new mixed-case tag keeps the casing too.
#[tokio::test]
async fn featuring_a_new_tag_keeps_the_written_casing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;

    let featured = post(&app, &token, "/api/v1/tags/R6FeatureTag/feature").await;
    common::assert_answer(&featured, StatusCode::OK, "the feature");
    assert_eq!(featured.json_value()["name"], "R6FeatureTag", "the casing");
}
