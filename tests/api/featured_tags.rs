//! Featured hashtags over HTTP (WBE-827): `/api/v1/featured_tags` and its
//! suggestions, `/api/v1/accounts/{id}/featured_tags`, the `feature` and
//! `unfeature` tag routes, the counts the posts keep current, and the
//! `featuredTags` collection the actor document advertises.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::{
    featured_tag,
    status::Visibility,
    user::{User, local_ap_id},
};

use crate::common;

/// The collection root of the own list.
const FEATURED: &str = "/api/v1/featured_tags";
/// The ten-tag limit's 422 body.
const LIMIT: &str = "Validation failed: You have already featured the maximum number of hashtags";

/// A local user named `nickname` and a bearer token granting `scopes`.
async fn user_and_token(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let user = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &user, scopes).await;
    (user, token)
}

/// `POST /api/v1/featured_tags` with `name`.
async fn feature(app: &Router, token: &str, name: &str) -> common::TestResponse {
    common::TestRequest::post(FEATURED)
        .bearer(token)
        .json(&json!({ "name": name }))
        .send(app)
        .await
}

/// `GET` of `path` as `token`.
async fn get(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// The `name`s of a rendered array, in order.
fn names(rows: &Value) -> Vec<&str> {
    rows.as_array()
        .expect("an array")
        .iter()
        .map(|row| row["name"].as_str().expect("every row names its tag"))
        .collect()
}

#[tokio::test]
async fn featuring_a_tag_answers_the_entity_and_lists_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = user_and_token(&pool, "alice", "read write").await;

    let created = feature(&app, &token, "#FedLab").await;
    assert_eq!(created.status, StatusCode::OK, "the tag is featured");
    let entity = created.json_value();
    assert_eq!(entity["name"], "FedLab", "the casing the owner wrote");
    assert_eq!(
        entity["url"],
        format!("https://test.example/@{}/tagged/fedlab", alice.nickname)
    );
    assert_eq!(entity["statuses_count"], "0", "counts are strings");
    assert_eq!(entity["last_status_at"], Value::Null, "never used");
    assert!(entity["id"].as_str().is_some(), "the row id");

    let listed = get(&app, &token, FEATURED).await;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(listed.json_value(), json!([entity]), "the list holds it");
}

#[tokio::test]
async fn duplicates_blank_names_and_the_eleventh_tag_are_422s() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = user_and_token(&pool, "alice", "read write").await;

    let first = feature(&app, &token, "rust").await;
    assert_eq!(first.status, StatusCode::OK);
    let again = feature(&app, &token, "#Rust").await;
    assert_eq!(again.status, StatusCode::UNPROCESSABLE_ENTITY, "twice");
    common::assert_error(
        &again,
        "Validation failed: Name has already been taken",
        "a duplicate",
    );
    for blank in ["", "  ", "#"] {
        let refused = feature(&app, &token, blank).await;
        common::assert_error(
            &refused,
            "Validation failed: Name can't be blank",
            "a blank name",
        );
    }
    let unusable = feature(&app, &token, "two words").await;
    common::assert_error(
        &unusable,
        "Validation failed: Name is invalid",
        "an unusable name",
    );

    for n in 1..10 {
        let created = feature(&app, &token, &format!("tag{n}x")).await;
        assert_eq!(created.status, StatusCode::OK, "tag {n} fits");
    }
    let eleventh = feature(&app, &token, "overflow").await;
    common::assert_error(&eleventh, LIMIT, "the eleventh tag");
    let listed = get(&app, &token, FEATURED).await.json_value();
    assert_eq!(names(&listed).len(), 10, "ten tags, no more");
    assert!(!names(&listed).contains(&"overflow"));
}

#[tokio::test]
async fn deleting_removes_one_row_and_only_the_owners() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, alice_token) = user_and_token(&pool, "alice", "read write").await;
    let (_bob, bob_token) = user_and_token(&pool, "bob", "read write").await;
    let id = feature(&app, &alice_token, "rust").await.json_value()["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let path = format!("{FEATURED}/{id}");

    let foreign = common::TestRequest::delete(&path)
        .bearer(&bob_token)
        .send(&app)
        .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND, "not bob's row");
    let removed = common::TestRequest::delete(&path)
        .bearer(&alice_token)
        .send(&app)
        .await;
    assert_eq!(removed.status, StatusCode::OK);
    assert_eq!(removed.json_value(), json!({}), "an empty object");
    let repeat = common::TestRequest::delete(&path)
        .bearer(&alice_token)
        .send(&app)
        .await;
    assert_eq!(repeat.status, StatusCode::NOT_FOUND, "already gone");
    let malformed = common::TestRequest::delete(&format!("{FEATURED}/nope"))
        .bearer(&alice_token)
        .send(&app)
        .await;
    assert_eq!(malformed.status, StatusCode::NOT_FOUND, "no such id");
    let listed = get(&app, &alice_token, FEATURED).await.json_value();
    assert!(names(&listed).is_empty(), "nothing left");
}

#[tokio::test]
async fn the_routes_demand_a_token_with_the_right_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, read_only) = user_and_token(&pool, "alice", "read").await;

    let anonymous = common::TestRequest::get(FEATURED).send(&app).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    let read = get(&app, &read_only, FEATURED).await;
    assert_eq!(read.status, StatusCode::OK, "read covers the list");
    let write = feature(&app, &read_only, "rust").await;
    assert_eq!(write.status, StatusCode::FORBIDDEN, "but not the write");
    let suggestions = get(&app, &read_only, "/api/v1/featured_tags/suggestions").await;
    assert_eq!(suggestions.status, StatusCode::OK);
}

#[tokio::test]
async fn counts_follow_the_accounts_public_posts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = user_and_token(&pool, "alice", "read write").await;
    let _before = common::create_status(&pool, &alice, "early #rust", Visibility::Public).await;
    let _private = common::create_status(&pool, &alice, "#rust secret", Visibility::Private).await;

    let created = feature(&app, &token, "rust").await.json_value();
    assert_eq!(created["statuses_count"], "1", "past public posts count");

    let _fresh = common::create_status(&pool, &alice, "new #rust post", Visibility::Unlisted).await;
    let listed = get(&app, &token, FEATURED).await.json_value();
    assert_eq!(listed[0]["statuses_count"], "2", "a new post bumps it");
    let today = chrono::Utc::now().date_naive().to_string();
    assert_eq!(listed[0]["last_status_at"], today, "and dates it");
}

#[tokio::test]
async fn suggestions_are_the_recent_unfeatured_tags() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = user_and_token(&pool, "alice", "read write").await;
    let _one = common::create_status(&pool, &alice, "#older", Visibility::Public).await;
    let _two = common::create_status(&pool, &alice, "#newer #taken", Visibility::Public).await;
    let created = feature(&app, &token, "taken").await;
    assert_eq!(created.status, StatusCode::OK);

    let response = get(&app, &token, "/api/v1/featured_tags/suggestions").await;
    assert_eq!(response.status, StatusCode::OK);
    let suggested = response.json_value();
    assert_eq!(names(&suggested), ["newer", "older"], "newest use first");
    assert!(
        suggested[0]["history"].is_array(),
        "suggestions are full Tag entities"
    );
}

#[tokio::test]
async fn an_accounts_list_is_public_for_local_and_remote_accounts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = user_and_token(&pool, "alice", "read write").await;
    let created = feature(&app, &token, "rust").await;
    assert_eq!(created.status, StatusCode::OK);
    let remote = common::create_remote_user(&pool, "wanda", "remote.example").await;
    featured_tag::replace_all(&pool, remote.id, &["fedlab".to_owned(), "#Art".to_owned()])
        .await
        .expect("the remote set stores");

    let local = common::TestRequest::get(&format!("/api/v1/accounts/{}/featured_tags", alice.id))
        .send(&app)
        .await;
    assert_eq!(local.status, StatusCode::OK, "no token needed");
    assert_eq!(names(&local.json_value()), ["rust"]);
    let mirrored =
        common::TestRequest::get(&format!("/api/v1/accounts/{}/featured_tags", remote.id))
            .send(&app)
            .await;
    assert_eq!(mirrored.status, StatusCode::OK);
    assert_eq!(
        names(&mirrored.json_value()),
        ["Art", "fedlab"],
        "the stored collection of the remote account"
    );
    for bad in [uuid::Uuid::now_v7().to_string(), "nope".to_owned()] {
        let missing = common::TestRequest::get(&format!("/api/v1/accounts/{bad}/featured_tags"))
            .send(&app)
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "{bad}");
    }
}

#[tokio::test]
async fn the_tag_routes_feature_and_unfeature_and_show_the_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = user_and_token(&pool, "alice", "read write follow").await;

    let featured = common::TestRequest::post("/api/v1/tags/Rust/feature")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(featured.status, StatusCode::OK);
    assert_eq!(featured.json_value()["featuring"], true);
    assert_eq!(featured.json_value()["name"], "rust");
    let twice = common::TestRequest::post("/api/v1/tags/rust/feature")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(twice.status, StatusCode::OK, "idempotent");
    let shown = get(&app, &token, "/api/v1/tags/rust").await.json_value();
    assert_eq!(shown["featuring"], true, "a viewer sees the flag");
    let anonymous = common::TestRequest::get("/api/v1/tags/rust")
        .send(&app)
        .await;
    assert!(
        anonymous.json_value().get("featuring").is_none(),
        "an anonymous render has none"
    );
    let listed = get(&app, &token, FEATURED).await.json_value();
    assert_eq!(names(&listed), ["rust"], "the same set as the list");

    let off = common::TestRequest::post("/api/v1/tags/rust/unfeature")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(off.status, StatusCode::OK);
    assert_eq!(off.json_value()["featuring"], false);
    let unknown = common::TestRequest::post("/api/v1/tags/neverseen/unfeature")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let invalid = common::TestRequest::post("/api/v1/tags/a%20b/feature")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn the_feature_route_stops_at_ten_tags() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = user_and_token(&pool, "alice", "read write").await;
    for n in 0..10 {
        let created = feature(&app, &token, &format!("tag{n}x")).await;
        assert_eq!(created.status, StatusCode::OK);
    }
    let over = common::TestRequest::post("/api/v1/tags/overflow/feature")
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_error(&over, LIMIT, "the eleventh tag by the tag route");
}

#[tokio::test]
async fn the_actor_advertises_and_serves_its_featured_tags() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = user_and_token(&pool, "alice", "read write").await;
    for name in ["rust", "fedlab"] {
        let created = feature(&app, &token, name).await;
        assert_eq!(created.status, StatusCode::OK);
    }

    let actor = common::TestRequest::get("/users/alice")
        .header("accept", "application/activity+json")
        .send(&app)
        .await
        .json_value();
    assert_eq!(
        actor["featuredTags"],
        "https://test.example/users/alice/collections/tags"
    );
    let collection = common::TestRequest::get("/users/alice/collections/tags")
        .header("accept", "application/activity+json")
        .send(&app)
        .await;
    assert_eq!(collection.status, StatusCode::OK);
    let document = collection.json_value();
    assert_eq!(document["type"], "Collection");
    assert_eq!(document["totalItems"], 2);
    assert_eq!(
        document["items"],
        json!([
            {"type": "Hashtag", "href": "https://test.example/tags/fedlab", "name": "#fedlab"},
            {"type": "Hashtag", "href": "https://test.example/tags/rust", "name": "#rust"},
        ])
    );
    let browser = common::TestRequest::get("/users/alice/collections/tags")
        .send(&app)
        .await;
    assert_eq!(browser.status, StatusCode::NOT_FOUND, "ActivityPub only");
}

#[tokio::test]
async fn changing_the_set_queues_one_profile_update() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let ap_id = local_ap_id("https://test.example", "alice");
    let alice = common::create_federating_user_with(&pool, "alice", |new| new).await;
    let token = common::create_app_and_token(&pool, &alice, "read write").await;

    let created = feature(&app, &token, "rust").await;
    assert_eq!(created.status, StatusCode::OK);
    let jobs: Vec<Value> = sqlx::query_scalar(
        "SELECT args FROM jobs WHERE kind = 'federator.publish' AND unique_key = $1",
    )
    .bind(format!("profile-update:{}", alice.id))
    .fetch_all(&pool)
    .await
    .expect("the publish jobs read");
    assert_eq!(jobs.len(), 1, "the change is announced");
    assert_eq!(jobs[0]["activity"]["type"], "Update");
    assert_eq!(
        jobs[0]["activity"]["object"]["featuredTags"],
        format!("{ap_id}/collections/tags"),
        "the actor document carries the collection address"
    );
}

#[tokio::test]
async fn the_owners_casing_is_kept_and_matching_ignores_case() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = user_and_token(&pool, "alice", "read write").await;

    let created = feature(&app, &token, "FedLabR4").await;
    assert_eq!(created.status, StatusCode::OK);
    let entity = created.json_value();
    assert_eq!(entity["name"], "FedLabR4", "the casing as given");
    assert_eq!(
        entity["url"],
        format!("https://test.example/@{}/tagged/fedlabr4", alice.nickname),
        "the slug stays lowercase"
    );

    let again = feature(&app, &token, "fedlabr4").await;
    assert_eq!(again.status, StatusCode::UNPROCESSABLE_ENTITY);
    common::assert_error(
        &again,
        "Validation failed: Name has already been taken",
        "another casing is the same tag",
    );

    let _post = common::create_status(&pool, &alice, "hello #fedlabr4", Visibility::Public).await;
    let listed = get(&app, &token, FEATURED).await.json_value();
    assert_eq!(names(&listed), ["FedLabR4"], "the list shows the casing");
    assert_eq!(
        listed[0]["statuses_count"], "1",
        "a post under #fedlabr4 counts"
    );

    let collection = common::TestRequest::get("/users/alice/collections/tags")
        .header("accept", "application/activity+json")
        .send(&app)
        .await
        .json_value();
    assert_eq!(
        collection["items"],
        json!([{
            "type": "Hashtag",
            "href": "https://test.example/tags/fedlabr4",
            "name": "#FedLabR4",
        }]),
        "the collection item keeps the casing, its href does not"
    );
}
