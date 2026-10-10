//! Integration tests for `POST /api/v1/apps`: application registration
//! answers 200 with the Mastodon `Application` entity plus credentials
//! for JSON and form bodies, rejects missing or empty required fields
//! and unknown scopes with 422, and the stored row matches what the
//! response says. No authentication is required (decision 9).

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::oauth_app;

/// POSTs `body` as JSON to the registration endpoint.
async fn post_json(app: &Router, body: &Value) -> common::TestResponse {
    common::TestRequest::post("/api/v1/apps")
        .json(body)
        .send(app)
        .await
}

/// The `error` message of a JSON error response.
fn error_text(response: &common::TestResponse) -> String {
    response.json_value()["error"]
        .as_str()
        .expect("errors carry a string error field")
        .to_owned()
}

/// A full JSON registration: every documented field echoes back, the
/// credentials have Pleroma's shape, and the stored row is the response.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn json_registration_returns_the_application_entity(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let response = post_json(
        &app,
        &json!({
            "client_name": "My Client",
            "redirect_uris": ["https://client.example/cb", " https://other.example/cb "],
            "scopes": "read write",
            "website": "https://client.example",
        }),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "200 on success");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the entity is JSON"
    );
    let body = response.json_value();
    let id = body["id"]
        .as_str()
        .and_then(|text| Uuid::parse_str(text).ok())
        .expect("id is the hyphenated UUIDv7");
    assert!(id.get_version_num() == 7, "the id is a UUIDv7 (decision 4)");
    assert_eq!(body["name"], "My Client", "client_name becomes name");
    assert_eq!(
        body["website"], "https://client.example",
        "website round trips"
    );
    assert_eq!(
        body["redirect_uri"], "https://client.example/cb\nhttps://other.example/cb",
        "a list is trimmed and joined with newlines, then echoed as stored"
    );
    let client_id = body["client_id"].as_str().expect("client_id is a string");
    let client_secret = body["client_secret"]
        .as_str()
        .expect("client_secret is a string");
    assert_eq!(client_id.len(), 43, "client_id has 43 characters");
    assert_eq!(client_secret.len(), 43, "client_secret has 43 characters");
    assert_ne!(client_id, client_secret, "the two tokens differ");
    assert!(
        client_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "client_id is URL-safe"
    );
    assert_eq!(
        body["client_secret_expires_at"], 0,
        "these secrets never expire"
    );
    assert!(
        body["vapid_key"].is_null(),
        "vapid_key stays null until the push ticket"
    );
    assert_eq!(body["scopes"], json!(["read", "write"]), "a scope list");

    let stored = oauth_app::get_by_client_id(&pool, client_id)
        .await
        .expect("the lookup reaches the database")
        .expect("the registration stored a row");
    assert_eq!(stored.id, id, "the response id is the stored primary key");
    assert_eq!(stored.client_name, "My Client", "the stored name");
    assert_eq!(
        stored.redirect_uris,
        body["redirect_uri"].as_str().expect("a string"),
        "the stored redirect_uris is exactly the response's redirect_uri"
    );
    assert_eq!(
        stored.scopes, "read write",
        "the stored scopes is the space-joined list"
    );
    assert_eq!(
        stored.website.as_deref(),
        Some("https://client.example"),
        "the stored website"
    );
    assert_eq!(
        stored.client_secret, client_secret,
        "the stored secret is the response's secret"
    );
    assert!(!stored.trusted, "registrations start untrusted");
    assert!(stored.user_id.is_none(), "no owner without authentication");
}

/// Form bodies register the same way: `redirect_uris[]` keys become the
/// joined storage string, a lone value is stored verbatim, and `+`
/// separated scopes parse.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn form_registration_stores_and_echoes_the_same_values(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let list = common::TestRequest::post("/api/v1/apps")
        .form(&[
            ("client_name", "Form Client"),
            ("redirect_uris[]", "https://a.example/cb"),
            ("redirect_uris[]", "https://b.example/cb"),
        ])
        .send(&app)
        .await;
    assert_eq!(list.status, StatusCode::OK, "a form body registers");
    let list_body = list.json_value();
    assert_eq!(
        list_body["redirect_uri"], "https://a.example/cb\nhttps://b.example/cb",
        "list form keys join with newlines"
    );
    assert_eq!(
        list_body["scopes"],
        json!(["read"]),
        "no scopes sent, the default applies"
    );

    let single = common::TestRequest::post("/api/v1/apps")
        .form(&[
            ("client_name", "Form Client"),
            ("redirect_uris", "https://a.example/cb https://b.example/cb"),
            ("scopes", "read+follow"),
        ])
        .send(&app)
        .await;
    assert_eq!(
        single.status,
        StatusCode::OK,
        "a string form field registers"
    );
    let single_body = single.json_value();
    assert_eq!(
        single_body["redirect_uri"], "https://a.example/cb https://b.example/cb",
        "a single string is stored verbatim"
    );
    assert_eq!(
        single_body["scopes"],
        json!(["read", "follow"]),
        "the plus-separated form value parses"
    );

    let stored = oauth_app::get_by_client_id(
        &pool,
        single_body["client_id"]
            .as_str()
            .expect("client_id is a string"),
    )
    .await
    .expect("the lookup reaches the database")
    .expect("the registration stored a row");
    assert_eq!(
        stored.scopes, "read follow",
        "the stored scopes is the response list joined by spaces"
    );
    assert_eq!(
        stored.redirect_uris,
        single_body["redirect_uri"].as_str().expect("a string"),
        "the stored redirect_uris matches the response"
    );
}

/// A minimal body (just the two required fields) gets the documented
/// defaults.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn defaults_apply_for_a_minimal_registration(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let response = post_json(
        &app,
        &json!({"client_name": "Bare", "redirect_uris": "https://bare.example/cb"}),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the minimal body registers"
    );
    let body = response.json_value();
    assert_eq!(body["scopes"], json!(["read"]), "scopes default to read");
    assert!(body["website"].is_null(), "website defaults to null");

    let stored = oauth_app::get_by_client_id(
        &pool,
        body["client_id"].as_str().expect("client_id is a string"),
    )
    .await
    .expect("the lookup reaches the database")
    .expect("the registration stored a row");
    assert_eq!(stored.scopes, "read", "the default is what gets stored");
    assert!(stored.website.is_none(), "no website row content");
    assert!(stored.user_id.is_none(), "no owner");
    assert!(!stored.trusted, "untrusted by default");
}

/// A body without `client_name` is a 422 that names the field.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn missing_client_name_is_unprocessable(pool: PgPool) {
    let app = common::test_app(pool);
    let response = post_json(&app, &json!({"redirect_uris": "https://x.example/cb"})).await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "missing client_name is a 422"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "errors answer with JSON"
    );
    assert!(
        error_text(&response).contains("client_name"),
        "the message names the missing field"
    );
}

/// `redirect_uris` must be present and carry something: absent, an empty
/// string, an empty list and an all-blank list are all 422s.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn missing_or_empty_redirect_uris_are_unprocessable(pool: PgPool) {
    let app = common::test_app(pool.clone());
    for body in [
        json!({"client_name": "C"}),
        json!({"client_name": "C", "redirect_uris": ""}),
        json!({"client_name": "C", "redirect_uris": "   "}),
        json!({"client_name": "C", "redirect_uris": []}),
        json!({"client_name": "C", "redirect_uris": [" ", ""]}),
    ] {
        let response = post_json(&app, &body).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body} must be a 422"
        );
    }
    let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_apps")
        .fetch_one(&pool)
        .await
        .expect("the count reaches the database");
    assert_eq!(rows.0, 0, "no rejection stored a row");
}

/// An unknown scope is a 422 naming the offender, and nothing is stored.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn an_unknown_scope_is_unprocessable(pool: PgPool) {
    let app = common::test_app(pool.clone());
    let response = post_json(
        &app,
        &json!({
            "client_name": "C",
            "redirect_uris": "https://x.example/cb",
            "scopes": "read bogus",
        }),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unknown scope is a 422"
    );
    assert!(
        error_text(&response).contains("bogus"),
        "the message names the bad scope"
    );
    let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_apps")
        .fetch_one(&pool)
        .await
        .expect("the count reaches the database");
    assert_eq!(rows.0, 0, "the rejected registration stored nothing");
}

/// Two registrations of the same client name get distinct ids and
/// distinct credential pairs.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn two_registrations_get_distinct_credentials(pool: PgPool) {
    let app = common::test_app(pool);
    let first = post_json(
        &app,
        &json!({"client_name": "Twin", "redirect_uris": "https://twin.example/cb"}),
    )
    .await;
    let second = post_json(
        &app,
        &json!({"client_name": "Twin", "redirect_uris": "https://twin.example/cb"}),
    )
    .await;
    assert_eq!(second.status, StatusCode::OK, "the second registers too");
    let (a, b) = (first.json_value(), second.json_value());
    assert_ne!(a["id"], b["id"], "the ids differ");
    assert_ne!(a["client_id"], b["client_id"], "the client ids differ");
    assert_ne!(
        a["client_secret"], b["client_secret"],
        "the client secrets differ"
    );
}
