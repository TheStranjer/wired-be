//! The Mastodon v2 filter CRUD over HTTP (`/api/v2/filters`): the lifecycle
//! of filters, keywords and status entries, the scope gates, Mastodon's
//! validation wording and the 404 every foreign or malformed id answers.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::models::status::Visibility;

use super::support::{ROOT_V2, fixture, id_in, v2_create, v2_delete, v2_get, v2_post, v2_put};
use crate::common;

/// Every key the v2 filter entity carries, sorted.
const FILTER_KEYS: [&str; 7] = [
    "context",
    "expires_at",
    "filter_action",
    "id",
    "keywords",
    "statuses",
    "title",
];

/// The scopes the lifecycle needs.
const SCOPES: &str = "read:filters write:filters";

/// A filter body naming `title`, the `home` context and one keyword.
fn body(title: &str, keyword: &str) -> Value {
    json!({
        "title": title,
        "context": ["home"],
        "filter_action": "warn",
        "keywords_attributes": [{"keyword": keyword, "whole_word": true}],
    })
}

/// The whole lifecycle of a filter: create with a keyword, list, show, a
/// partial `PUT` that leaves the rest alone, and delete.
#[tokio::test]
async fn a_v2_filter_round_trips_through_every_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", SCOPES).await;
    let app = common::test_app(pool);

    let created = v2_create(&app, &token, body("Spoilers", "spoiler")).await;
    assert_eq!(
        common::sorted_keys(&created),
        FILTER_KEYS,
        "the entity keys"
    );
    assert_eq!(created["title"], "Spoilers", "the title");
    assert_eq!(created["context"], json!(["home"]), "the contexts");
    assert_eq!(created["filter_action"], "warn", "the action");
    assert_eq!(created["expires_at"], Value::Null, "no expiry");
    assert_eq!(created["statuses"], json!([]), "no status entries");
    assert_eq!(created["keywords"][0]["keyword"], "spoiler", "the keyword");
    assert_eq!(created["keywords"][0]["whole_word"], true, "whole word");
    let id = id_in(&created);

    let listed = v2_get(&app, &token, ROOT_V2).await.json_value();
    assert_eq!(listed, json!([created.clone()]), "the list holds it");
    let shown = v2_get(&app, &token, &format!("{ROOT_V2}/{id}")).await;
    assert_eq!(shown.json_value(), created, "and the show answers it");

    let changed = v2_put(
        &app,
        &token,
        &format!("{ROOT_V2}/{id}"),
        json!({"title": "Renamed", "filter_action": "blur", "expires_in": 3600}),
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK, "the update answers 200");
    let after = changed.json_value();
    assert_eq!(after["title"], "Renamed", "the title changed");
    assert_eq!(after["filter_action"], "blur", "and the action");
    assert_eq!(after["context"], created["context"], "the contexts stayed");
    assert_eq!(after["keywords"], created["keywords"], "and the keywords");
    assert!(after["expires_at"].is_string(), "the expiry was set");

    let cleared = v2_put(
        &app,
        &token,
        &format!("{ROOT_V2}/{id}"),
        json!({"expires_in": null}),
    )
    .await
    .json_value();
    assert_eq!(cleared["expires_at"], Value::Null, "null clears the expiry");

    let deleted = v2_delete(&app, &token, &format!("{ROOT_V2}/{id}")).await;
    assert_eq!(deleted.json_value(), json!({}), "delete answers `{{}}`");
    let gone = v2_get(&app, &token, &format!("{ROOT_V2}/{id}")).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND, "and the filter is gone");
}

/// `keywords_attributes` on a `PUT` edits, removes and adds keywords in one
/// request, as Mastodon's web client sends them.
#[tokio::test]
async fn keywords_attributes_edit_remove_and_add_in_one_update() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", SCOPES).await;
    let app = common::test_app(pool);
    let created = v2_create(
        &app,
        &token,
        json!({
            "title": "Words",
            "context": ["home"],
            "keywords_attributes": [{"keyword": "old"}, {"keyword": "doomed"}],
        }),
    )
    .await;
    let id = id_in(&created);
    let old = id_in(&created["keywords"][0]);
    let doomed = id_in(&created["keywords"][1]);

    let updated = v2_put(
        &app,
        &token,
        &format!("{ROOT_V2}/{id}"),
        json!({"keywords_attributes": [
            {"id": old, "keyword": "new", "whole_word": false},
            {"id": doomed, "_destroy": true},
            {"keyword": "added"},
        ]}),
    )
    .await
    .json_value();
    let words: Vec<(&Value, &Value)> = updated["keywords"]
        .as_array()
        .expect("keywords are an array")
        .iter()
        .map(|entry| (&entry["keyword"], &entry["whole_word"]))
        .collect();
    assert_eq!(
        words,
        [
            (&json!("new"), &json!(false)),
            (&json!("added"), &json!(true))
        ],
        "one edited, one removed, one added"
    );

    let refused = v2_put(
        &app,
        &token,
        &format!("{ROOT_V2}/{id}"),
        json!({"title": "Never", "keywords_attributes": [{"id": doomed, "_destroy": true}]}),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::NOT_FOUND,
        "a keyword the filter no longer holds is a 404"
    );
    let kept = v2_get(&app, &token, &format!("{ROOT_V2}/{id}")).await;
    assert_eq!(
        kept.json_value()["title"],
        "Words",
        "and the rest of that update was not applied"
    );
}

/// The keyword sub-resource: list, add, show, change and remove.
#[tokio::test]
async fn the_keyword_routes_round_trip() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", SCOPES).await;
    let app = common::test_app(pool);
    let filter = v2_create(&app, &token, body("Words", "first")).await;
    let path = format!("{ROOT_V2}/{}/keywords", id_in(&filter));

    let added = v2_post(
        &app,
        &token,
        &path,
        json!({"keyword": "second", "whole_word": false}),
    )
    .await
    .json_value();
    assert_eq!(
        common::sorted_keys(&added),
        ["id", "keyword", "whole_word"],
        "the keyword entity"
    );
    assert_eq!(added["whole_word"], false, "the flag sent");
    let listed = v2_get(&app, &token, &path).await.json_value();
    assert_eq!(listed.as_array().map(Vec::len), Some(2), "both keywords");

    let keyword_path = format!("{ROOT_V2}/keywords/{}", id_in(&added));
    let shown = v2_get(&app, &token, &keyword_path).await.json_value();
    assert_eq!(shown, added, "the show answers the keyword by its own id");
    let renamed = v2_put(&app, &token, &keyword_path, json!({"keyword": "third"}))
        .await
        .json_value();
    assert_eq!(renamed["keyword"], "third", "the phrase changed");
    assert_eq!(renamed["whole_word"], false, "the flag stayed");
    let removed = v2_delete(&app, &token, &keyword_path).await;
    assert_eq!(removed.json_value(), json!({}), "delete answers `{{}}`");
    let gone = v2_get(&app, &token, &keyword_path).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND, "the keyword is gone");
    let blank = v2_post(&app, &token, &path, json!({"keyword": " "})).await;
    assert_eq!(
        blank.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "blank is a 422"
    );
    assert_eq!(
        blank.error_field().as_deref(),
        Some("Validation failed: Keyword can't be blank"),
        "in Mastodon's wording"
    );
}

/// The status sub-resource: add, list, show and remove, with the 422s for a
/// repeated or unknown status.
#[tokio::test]
async fn the_status_routes_round_trip() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", SCOPES).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let post = common::create_status(&pool, &bob, "hello", Visibility::Public).await;
    let app = common::test_app(pool);
    let filter = v2_create(&app, &token, body("Words", "first")).await;
    let path = format!("{ROOT_V2}/{}/statuses", id_in(&filter));
    let status_id = post.id.to_string();

    let added = v2_post(&app, &token, &path, json!({"status_id": status_id})).await;
    assert_eq!(added.status, StatusCode::OK, "adding answers 200");
    let entry = added.json_value();
    assert_eq!(
        common::sorted_keys(&entry),
        ["id", "status_id"],
        "the status entity"
    );
    assert_eq!(entry["status_id"], json!(status_id), "the status named");
    let listed = v2_get(&app, &token, &path).await.json_value();
    assert_eq!(listed, json!([entry.clone()]), "the list holds it");
    let shown = v2_get(&app, &token, &format!("{ROOT_V2}/{}", id_in(&filter))).await;
    assert_eq!(
        shown.json_value()["statuses"],
        json!([entry.clone()]),
        "and the filter carries it"
    );

    let repeat = v2_post(&app, &token, &path, json!({"status_id": status_id})).await;
    assert_eq!(
        repeat.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "twice is a 422"
    );
    let unknown = v2_post(
        &app,
        &token,
        &path,
        json!({"status_id": uuid::Uuid::now_v7().to_string()}),
    )
    .await;
    assert_eq!(
        unknown.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a status nobody wrote is a 422"
    );

    let entry_path = format!("{ROOT_V2}/statuses/{}", id_in(&entry));
    let by_id = v2_get(&app, &token, &entry_path).await.json_value();
    assert_eq!(by_id, entry, "the entry reads by its own id");
    let removed = v2_delete(&app, &token, &entry_path).await;
    assert_eq!(removed.json_value(), json!({}), "delete answers `{{}}`");
    let gone = v2_get(&app, &token, &entry_path).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND, "the entry is gone");
}

/// Mastodon's validation wording on create, ahead of any storage.
#[tokio::test]
async fn invalid_creates_are_422_with_mastodon_wording() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", SCOPES).await;
    let app = common::test_app(pool);

    for (bad, message) in [
        (
            json!({"context": ["home"]}),
            "Validation failed: Title can't be blank",
        ),
        (
            json!({"title": "t"}),
            "Validation failed: Context is invalid",
        ),
        (
            json!({"title": "t", "context": ["bogus"]}),
            "Validation failed: Context is invalid",
        ),
        (
            json!({"title": "t", "context": ["home"], "filter_action": "mute"}),
            "Validation failed: Filter action is invalid",
        ),
    ] {
        let response = v2_post(&app, &token, ROOT_V2, bad.clone()).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{bad}` is a 422"
        );
        assert_eq!(
            response.error_field().as_deref(),
            Some(message),
            "`{bad}` carries Mastodon's wording"
        );
    }
    let listed = v2_get(&app, &token, ROOT_V2).await.json_value();
    assert_eq!(listed, json!([]), "and nothing was stored");
}

/// Another account's filters, keywords and status entries, and malformed
/// ids, are all the same 404.
#[tokio::test]
async fn foreign_and_malformed_ids_are_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, alice_token) = fixture(&pool, "alice", SCOPES).await;
    let (_bob, bob_token) = fixture(&pool, "bob", SCOPES).await;
    let author = common::create_local_user(&pool, "carol").await;
    let post = common::create_status(&pool, &author, "hello", Visibility::Public).await;
    let app = common::test_app(pool);
    let hers = v2_create(&app, &alice_token, body("Hers", "word")).await;
    let id = id_in(&hers);
    let keyword = id_in(&hers["keywords"][0]);
    let entry = id_in(
        &v2_post(
            &app,
            &alice_token,
            &format!("{ROOT_V2}/{id}/statuses"),
            json!({"status_id": post.id.to_string()}),
        )
        .await
        .json_value(),
    );

    for path in [
        format!("{ROOT_V2}/{id}"),
        format!("{ROOT_V2}/{id}/keywords"),
        format!("{ROOT_V2}/{id}/statuses"),
        format!("{ROOT_V2}/keywords/{keyword}"),
        format!("{ROOT_V2}/statuses/{entry}"),
        format!("{ROOT_V2}/not-an-id"),
        format!("{ROOT_V2}/keywords/not-an-id"),
    ] {
        let read = v2_get(&app, &bob_token, &path).await;
        assert_eq!(read.status, StatusCode::NOT_FOUND, "bob reads `{path}`");
    }
    for path in [
        format!("{ROOT_V2}/{id}"),
        format!("{ROOT_V2}/keywords/{keyword}"),
        format!("{ROOT_V2}/statuses/{entry}"),
        format!("{ROOT_V2}/not-an-id"),
    ] {
        let removed = v2_delete(&app, &bob_token, &path).await;
        assert_eq!(
            removed.status,
            StatusCode::NOT_FOUND,
            "bob deletes `{path}`"
        );
    }
    let renamed = v2_put(
        &app,
        &bob_token,
        &format!("{ROOT_V2}/{id}"),
        json!({"title": "stolen"}),
    )
    .await;
    assert_eq!(renamed.status, StatusCode::NOT_FOUND, "bob renames nothing");
    let intact = v2_get(&app, &alice_token, &format!("{ROOT_V2}/{id}")).await;
    let intact = intact.json_value();
    assert_eq!(intact["title"], hers["title"], "alice's title is untouched");
    assert_eq!(
        intact["keywords"], hers["keywords"],
        "and so are her keywords"
    );
    assert_eq!(
        intact["statuses"].as_array().map(Vec::len),
        Some(1),
        "and the status entry bob tried to delete is still there"
    );
    let listed = v2_get(&app, &bob_token, ROOT_V2).await.json_value();
    assert_eq!(listed, json!([]), "and bob's list is his own");
}

/// The credential and scope gates: no token is a 401, an app-only token and
/// a missing scope are 403s, and `read`/`write` cover the halves.
#[tokio::test]
async fn the_v2_routes_enforce_the_filter_scopes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_reader, read_only) = fixture(&pool, "reader", "read:filters").await;
    let (_writer, write_only) = fixture(&pool, "writer", "write:filters").await;
    let (_broad_reader, broad_read) = fixture(&pool, "broadreader", "read").await;
    let (_broad_writer, broad_write) = fixture(&pool, "broadwriter", "write").await;
    let (_narrow, wrong) = fixture(&pool, "narrow", "read:statuses write:statuses").await;
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let app = common::test_app(pool);

    let anonymous = common::TestRequest::get(ROOT_V2).send(&app).await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "no token is a 401"
    );
    let userless = v2_get(&app, &app_only, ROOT_V2).await;
    assert_eq!(
        userless.status,
        StatusCode::FORBIDDEN,
        "an app-only token is a 403"
    );

    let new = body("Words", "word");
    for (who, token) in [("read:filters", &read_only), ("read", &broad_read)] {
        let read = v2_get(&app, token, ROOT_V2).await;
        assert_eq!(read.status, StatusCode::OK, "`{who}` reads");
        let write = v2_post(&app, token, ROOT_V2, new.clone()).await;
        assert_eq!(write.status, StatusCode::FORBIDDEN, "`{who}` cannot write");
    }
    for (who, token) in [("write:filters", &write_only), ("write", &broad_write)] {
        let read = v2_get(&app, token, ROOT_V2).await;
        assert_eq!(read.status, StatusCode::FORBIDDEN, "`{who}` cannot read");
        let write = v2_post(&app, token, ROOT_V2, new.clone()).await;
        assert_eq!(write.status, StatusCode::OK, "`{who}` writes");
    }
    let read = v2_get(&app, &wrong, ROOT_V2).await;
    assert_eq!(
        read.status,
        StatusCode::FORBIDDEN,
        "another scope cannot read"
    );
    let write = v2_post(&app, &wrong, ROOT_V2, new).await;
    assert_eq!(write.status, StatusCode::FORBIDDEN, "nor write");
}
