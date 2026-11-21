//! The Mastodon v1 filter CRUD over HTTP (`/api/v1/filters`, WBE-98): the
//! lifecycle walk, the form bodies with `context[]` array keys, the whole
//! replacement on `PUT`, the expiry's effect on the list, Mastodon's
//! validation wording and the ownership and missing-id 404s.

use axum::http::StatusCode;
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use uuid::Uuid;
use wired_be::{models::filter, util::time::parse_time};

use super::support::{
    EXPECTED_KEYS, ROOT, create_body, create_form, create_json, destroy, fixture, json_keys, list,
    listed, phrases, seed, show, update,
};
use crate::common;

/// An account with nothing stored answers 200 with the empty JSON array.
#[tokio::test]
async fn empty_database_answers_the_empty_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "read:filters").await;
    let app = common::test_app(pool);

    let response = list(&app, &token).await;
    assert_eq!(response.status, StatusCode::OK, "200 for a reader");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the list is JSON"
    );
    assert_eq!(response.text(), "[]", "no filters, the empty array");
}

/// The whole lifecycle in order: `POST` answers the entity, the list and
/// the show find it, `PUT` replaces every column, `DELETE` answers `{}`
/// and takes it away.
#[tokio::test]
async fn crud_round_trip() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "read:filters write:filters").await;
    let app = common::test_app(pool);

    let created = create_json(&app, &token, create_body()).await;
    assert_eq!(created.status, StatusCode::OK, "creation answers 200");
    let filter = created.json_value();
    assert_eq!(json_keys(&filter), EXPECTED_KEYS, "exactly the entity keys");
    let id = filter["id"].as_str().expect("the id is text").to_owned();
    assert!(
        Uuid::parse_str(&id).is_ok(),
        "and a `UUIDv7` string at that: {id}"
    );
    assert_eq!(filter["phrase"], "Spoilers", "the phrase verbatim");
    assert_eq!(filter["context"], json!(["home"]), "the contexts sent");
    assert_eq!(filter["whole_word"], true, "the default flags");
    assert_eq!(filter["irreversible"], false, "both of them");
    assert_eq!(filter["expires_at"], Value::Null, "and no expiry");

    assert_eq!(phrases(&listed(&app, &token).await), ["Spoilers"], "listed");
    let shown = show(
        &app,
        &token,
        Uuid::parse_str(&id).expect("the id parses back"),
    )
    .await;
    assert_eq!(shown.status, StatusCode::OK, "and served by id");
    assert_eq!(shown.json_value(), filter, "the same entity either way");

    let replaced = update(
        &app,
        &token,
        Uuid::parse_str(&id).expect("the id parses back"),
        json!({
            "phrase": "Endings",
            "context": ["public", "thread"],
            "whole_word": false,
            "irreversible": true,
        }),
    )
    .await;
    assert_eq!(replaced.status, StatusCode::OK, "the update answers 200");
    let after = replaced.json_value();
    assert_eq!(after["id"], filter["id"], "the same id");
    assert_eq!(after["phrase"], "Endings", "the replaced phrase");
    assert_eq!(
        after["context"],
        json!(["public", "thread"]),
        "the replaced contexts, in order"
    );
    assert_eq!(after["whole_word"], false, "the replaced flags");
    assert_eq!(after["irreversible"], true, "both");

    let gone = destroy(
        &app,
        &token,
        Uuid::parse_str(&id).expect("the id parses back"),
    )
    .await;
    assert_eq!(gone.status, StatusCode::OK, "the delete answers 200");
    assert_eq!(gone.json_value(), json!({}), "with the empty object");
    assert!(
        listed(&app, &token).await.is_empty(),
        "and afterwards the list is empty"
    );
    let missing = show(
        &app,
        &token,
        Uuid::parse_str(&id).expect("the id parses back"),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "and the row is gone");
}

/// A form body with `context[]` array keys creates the same filter a JSON
/// body would, string booleans and `expires_in` included.
#[tokio::test]
async fn form_bodies_with_array_keys_are_accepted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "write:filters").await;
    let app = common::test_app(pool);

    let created = create_form(
        &app,
        &token,
        &[
            ("phrase", "Announcements"),
            ("context[]", "home"),
            ("context[]", "notifications"),
            ("whole_word", "false"),
            ("irreversible", "1"),
            ("expires_in", "3600"),
        ],
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "the form creates");
    let filter = created.json_value();
    assert_eq!(
        filter["context"],
        json!(["home", "notifications"]),
        "the array keys became the list"
    );
    assert_eq!(filter["whole_word"], false, "the string boolean");
    assert_eq!(filter["irreversible"], true, "and the numeric one");
    let expires_at = filter["expires_at"]
        .as_str()
        .expect("a positive expires_in stamps an expiry");
    let when = parse_time(expires_at).expect("the expiry is a Mastodon time");
    let delta = when - Utc::now();
    assert!(
        delta > Duration::seconds(3590) && delta < Duration::seconds(3610),
        "about an hour ahead, got {delta}"
    );
}

/// A PUT replaces the whole row: an absent `expires_in` clears the old
/// expiry, absent flags take their defaults.
#[tokio::test]
async fn update_replaces_defaults_instead_of_keeping_stored_values() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:filters write:filters").await;
    let stored = seed(&pool, &alice, "soon", Some(Utc::now() + Duration::days(2))).await;
    let app = common::test_app(pool);

    let replaced = update(
        &app,
        &token,
        stored.id,
        json!({"phrase": "forever", "context": ["account"]}),
    )
    .await;
    assert_eq!(replaced.status, StatusCode::OK, "the update answers 200");
    let filter = replaced.json_value();
    assert_eq!(filter["expires_at"], Value::Null, "the expiry is cleared");
    assert_eq!(
        filter["whole_word"], true,
        "the flag returns to its default"
    );
    assert_eq!(filter["irreversible"], false, "and so does this one");
}

/// An expired filter leaves the list and stays readable by id.
#[tokio::test]
async fn expired_filters_are_hidden_from_the_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read:filters").await;
    let expired = seed(
        &pool,
        &alice,
        "expired",
        Some(Utc::now() - Duration::days(1)),
    )
    .await;
    let _live: filter::Filter =
        seed(&pool, &alice, "alive", Some(Utc::now() + Duration::days(1))).await;
    let _eternal: filter::Filter = seed(&pool, &alice, "eternal", None).await;
    let app = common::test_app(pool);

    let body = listed(&app, &token).await;
    assert_eq!(
        phrases(&body),
        ["eternal", "alive"],
        "the unexpired ones, newest first; the expired one is out"
    );

    let response = show(&app, &token, expired.id).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "while an expired filter is still a row its owner can read"
    );
    assert_eq!(response.json_value()["phrase"], "expired", "by id");
}

/// The bodies the filter endpoints refuse, each with the case the assertion
/// names it by and the wording Mastodon's changeset answers.
fn invalid_bodies() -> Vec<(&'static str, Value, &'static str)> {
    vec![
        (
            "a missing phrase",
            json!({"context": ["home"]}),
            "Validation failed: Phrase can't be blank",
        ),
        (
            "an empty phrase",
            json!({"phrase": "", "context": ["home"]}),
            "Validation failed: Phrase can't be blank",
        ),
        (
            "a blank phrase",
            json!({"phrase": "   ", "context": ["home"]}),
            "Validation failed: Phrase can't be blank",
        ),
        (
            "a 201-character phrase",
            json!({"phrase": "x".repeat(201), "context": ["home"]}),
            "Validation failed: Phrase is too long (maximum is 200 characters)",
        ),
        (
            "a missing context",
            json!({"phrase": "ok"}),
            "Validation failed: Context is invalid",
        ),
        (
            "an empty context",
            json!({"phrase": "ok", "context": []}),
            "Validation failed: Context is invalid",
        ),
        (
            "an unknown context",
            json!({"phrase": "ok", "context": ["bogus"]}),
            "Validation failed: Context is invalid",
        ),
        (
            "one bad value among good ones",
            json!({"phrase": "ok", "context": ["home", "Bogus"]}),
            "Validation failed: Context is invalid",
        ),
    ]
}

/// The `{"error":"Validation failed: …"}` bodies the ticket names, and
/// their precedence: validation runs before any lookup, so even a PUT to
/// a nonexistent id answers 422 for a bad body.
#[tokio::test]
async fn invalid_parameters_answer_mastodons_validation_wording() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "write:filters").await;
    let app = common::test_app(pool.clone());

    for (what, body, message) in invalid_bodies() {
        let response = create_json(&app, &token, body).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{what}` is a 422"
        );
        assert_eq!(
            response.json_value()["error"],
            message,
            "with Mastodon's wording for `{what}`"
        );
    }

    let wrong_type = create_form(
        &app,
        &token,
        &[
            ("phrase", "x"),
            ("context[]", "home"),
            ("whole_word", "maybe"),
        ],
    )
    .await;
    assert_eq!(
        wrong_type.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a flag that is not a boolean is also a 422"
    );
    assert!(
        wrong_type.json_value()["error"]
            .as_str()
            .unwrap_or_default()
            .contains("expected a boolean"),
        "and the flex message says what it wanted: {}",
        wrong_type.text()
    );

    let on_missing = update(&app, &token, Uuid::now_v7(), json!({"context": ["home"]})).await;
    assert_eq!(
        on_missing.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation precedes the lookup, as Pleroma's plug does"
    );
    assert_eq!(
        on_missing.json_value()["error"],
        "Validation failed: Phrase can't be blank",
        "even for an id that names nothing"
    );
}

/// A foreign id and a malformed one are the same 404 on every route, and
/// the owner's row is untouched by another account's attempts.
#[tokio::test]
async fn ownership_and_missing_ids_answer_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:filters write:filters").await;
    let (_bob, bob_token) = fixture(&pool, "bob", "read:filters write:filters").await;
    let hers = seed(&pool, &alice, "hers", None).await;
    let app = common::test_app(pool.clone());

    let shown = show(&app, &bob_token, hers.id).await;
    assert_eq!(shown.status, StatusCode::NOT_FOUND, "bob cannot read it");
    let replaced = update(
        &app,
        &bob_token,
        hers.id,
        json!({"phrase": "stolen", "context": ["home"]}),
    )
    .await;
    assert_eq!(
        replaced.status,
        StatusCode::NOT_FOUND,
        "bob cannot rewrite it"
    );
    let deleted = destroy(&app, &bob_token, hers.id).await;
    assert_eq!(
        deleted.status,
        StatusCode::NOT_FOUND,
        "bob cannot delete it"
    );
    assert!(
        listed(&app, &bob_token).await.is_empty(),
        "and nothing reached bob's list"
    );

    let kept = show(&app, &alice_token, hers.id).await;
    assert_eq!(kept.status, StatusCode::OK, "alice still owns her filter");
    assert_eq!(kept.json_value()["phrase"], "hers", "unchanged");

    for (what, target) in [
        ("never minted", Uuid::now_v7().to_string()),
        ("not an id", "not-a-uuid".to_owned()),
    ] {
        let shown_missing = common::TestRequest::get(&format!("{ROOT}/{target}"))
            .bearer(&alice_token)
            .send(&app)
            .await;
        assert_eq!(
            shown_missing.status,
            StatusCode::NOT_FOUND,
            "{what} is a 404 on show"
        );
        assert_eq!(
            shown_missing.json_value()["error"],
            "Record not found",
            "with Pleroma's wording for {what}"
        );
        let updated = common::TestRequest::put(&format!("{ROOT}/{target}"))
            .bearer(&alice_token)
            .json(&create_body())
            .send(&app)
            .await;
        assert_eq!(
            updated.status,
            StatusCode::NOT_FOUND,
            "{what} is a 404 on update"
        );
        let deleted_missing = common::TestRequest::delete(&format!("{ROOT}/{target}"))
            .bearer(&alice_token)
            .send(&app)
            .await;
        assert_eq!(
            deleted_missing.status,
            StatusCode::NOT_FOUND,
            "{what} is a 404 on delete"
        );
    }
    assert_eq!(
        phrases(&listed(&app, &alice_token).await),
        ["hers"],
        "and alice's row survived every attempt on it"
    );
}
