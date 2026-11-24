//! Integration tests for `/api/v1/apps`: `POST /api/v1/apps` answers 200
//! with the Mastodon `Application` entity plus credentials for JSON and
//! form bodies, rejects missing or empty required fields and unknown
//! scopes with 422, and the stored row matches what the response says.
//! No authentication is required (decision 9).
//! `GET /api/v1/apps/verify_credentials` answers with the same entity
//! minus its secrets for any valid bearer token — user-bound or app-only
//! — and with a 401 when the request carries no bearer token
//! ("Invalid credentials") or the token is unknown, expired or owned by
//! a deactivated account ("The access token is invalid").

#![cfg(test)]

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    oauth_app,
    oauth_token::{self, NewToken},
};

use crate::common;

/// POSTs `body` as JSON to the registration endpoint.
async fn post_json(app: &Router, body: &Value) -> common::TestResponse {
    common::TestRequest::post("/api/v1/apps")
        .json(body)
        .send(app)
        .await
}

/// The instance VAPID key the entity's `vapid_key` carries, read straight
/// from the service that also seeds the pair on first use.
async fn instance_vapid_key(pool: &PgPool) -> String {
    wired_be::services::vapid::vapid_public_key(pool)
        .await
        .expect("the instance VAPID key is readable")
}

/// The secret fields only the registration response carries.
const SECRET_FIELDS: [&str; 3] = ["client_id", "client_secret", "client_secret_expires_at"];

/// The row a registration stored, read back by its own `client_id`: every
/// column says what the response said.
async fn assert_stored_row(pool: &PgPool, client_id: &str, id: Uuid, body: &Value) {
    let stored = oauth_app::get_by_client_id(pool, client_id)
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
        stored.client_secret,
        body["client_secret"].as_str().expect("a secret"),
        "the stored secret is the response's secret"
    );
    assert!(!stored.trusted, "registrations start untrusted");
    assert!(stored.user_id.is_none(), "no owner without authentication");
}

/// A full JSON registration: every documented field echoes back, the
/// credentials have Pleroma's shape, and the stored row is the response.
#[tokio::test]
async fn json_registration_returns_the_application_entity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
    assert_eq!(
        body["vapid_key"],
        instance_vapid_key(&pool).await,
        "vapid_key is the instance VAPID public key"
    );
    assert_eq!(body["scopes"], json!(["read", "write"]), "a scope list");

    assert_stored_row(&pool, client_id, id, &body).await;
}

/// Form bodies register the same way: `redirect_uris[]` keys become the
/// joined storage string, a lone value is stored verbatim, and `+`
/// separated scopes parse.
#[tokio::test]
async fn form_registration_stores_and_echoes_the_same_values() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn defaults_apply_for_a_minimal_registration() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
#[tokio::test]
async fn missing_client_name_is_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
        response.error_text().contains("client_name"),
        "the message names the missing field"
    );
}

/// `redirect_uris` must be present and carry something: absent, an empty
/// string, an empty list and an all-blank list are all 422s.
#[tokio::test]
async fn missing_or_empty_redirect_uris_are_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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

/// Empty and over-long names, over-long and non-http(s) websites, and
/// fragment-bearing, relative and `javascript:` redirect URIs are 422s
/// that store nothing.
#[tokio::test]
async fn invalid_name_website_and_redirect_uris_are_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let ok_uri = "https://x.example/cb";
    let long_website = format!("https://a.example/{}", "a".repeat(1_990));
    for body in [
        json!({"client_name": "", "redirect_uris": ok_uri}),
        json!({"client_name": "   ", "redirect_uris": ok_uri}),
        json!({"client_name": "a".repeat(61), "redirect_uris": ok_uri}),
        json!({"client_name": "C", "redirect_uris": ok_uri, "website": long_website}),
        json!({"client_name": "C", "redirect_uris": ok_uri, "website": "ftp://a.example"}),
        json!({"client_name": "C", "redirect_uris": "https://a.example/cb#frag"}),
        json!({"client_name": "C", "redirect_uris": "/relative"}),
        json!({"client_name": "C", "redirect_uris": "javascript:alert(1)"}),
        json!({"client_name": "C", "redirect_uris": [ok_uri, "data:text/html,x"]}),
        json!({"client_name": "C", "redirect_uris": format!("https://a.example/{}", "a".repeat(2_000))}),
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

/// A 60-character name, a custom-scheme redirect, the OOB URN and two
/// space-separated https URIs all register.
#[tokio::test]
async fn valid_name_and_redirect_uri_shapes_register() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());
    for uris in [
        "myapp://callback",
        "urn:ietf:wg:oauth:2.0:oob",
        "https://a.example/cb https://b.example/cb",
    ] {
        let response = post_json(
            &app,
            &json!({"client_name": "a".repeat(60), "redirect_uris": uris, "website": "http://a.example"}),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "`{uris}` registers");
    }
}

/// An unknown scope is a 422 naming the offender, and nothing is stored.
#[tokio::test]
async fn an_unknown_scope_is_unprocessable() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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
        response.error_text().contains("bogus"),
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
#[tokio::test]
async fn two_registrations_get_distinct_credentials() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
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

/// A user-bound bearer token reaches `verify_credentials`: 200 with the
/// application the token was issued for, and none of the secrets.
#[tokio::test]
async fn verify_credentials_accepts_a_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let router = common::test_app(pool.clone());
    let user = common::create_local_user(&pool, "appverifier").await;
    let app_row = common::create_app(&pool, "read write").await;
    let token = oauth_token::create_token(
        &pool,
        &NewToken {
            user_id: Some(user.id),
            app_id: app_row.id,
            scopes: "read".to_owned(),
            ttl: None,
        },
    )
    .await
    .expect("token fixture inserts");

    let response = common::TestRequest::get("/api/v1/apps/verify_credentials")
        .bearer(&token.token)
        .send(&router)
        .await;
    assert_eq!(response.status, StatusCode::OK, "200 for a user token");
    let body = response.json_value();
    assert_eq!(
        body["id"],
        app_row.id.to_string(),
        "the entity names the issuing app"
    );
    assert_eq!(body["name"], "Test Client", "client_name becomes name");
    assert!(body["website"].is_null(), "an unset website is null");
    assert_eq!(
        body["redirect_uri"], "https://client.example/callback",
        "the stored redirect_uris"
    );
    assert_eq!(
        body["vapid_key"],
        instance_vapid_key(&pool).await,
        "verify_credentials carries the instance VAPID key too"
    );
    assert_eq!(
        body["scopes"],
        json!(["read", "write"]),
        "the app's registered scopes as an array"
    );
    assert_no_secrets(&body);
}

/// An app-only (`client_credentials`-shaped) token — minted straight
/// through the token model with no user — satisfies `verify_credentials`
/// too.
#[tokio::test]
async fn verify_credentials_accepts_an_app_only_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let router = common::test_app(pool.clone());
    let app_row = common::create_app(&pool, "read").await;
    let token = oauth_token::create_token(
        &pool,
        &NewToken {
            user_id: None,
            app_id: app_row.id,
            scopes: "read".to_owned(),
            ttl: None,
        },
    )
    .await
    .expect("the app-only token mints straight through the model");

    let response = common::TestRequest::get("/api/v1/apps/verify_credentials")
        .bearer(&token.token)
        .send(&router)
        .await;
    assert_eq!(response.status, StatusCode::OK, "200 for an app-only token");
    let body = response.json_value();
    assert_eq!(body["id"], app_row.id.to_string(), "the same entity");
    assert_no_secrets(&body);
}

/// The `verify_credentials` entity is exactly the registration entity
/// with the credential fields removed: register over HTTP, mint a token
/// for the registered app, and compare the two bodies key by key.
#[tokio::test]
async fn verify_credentials_is_the_registration_response_minus_the_secrets() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let router = common::test_app(pool.clone());
    let registration = post_json(
        &router,
        &json!({
            "client_name": "Mirror",
            "redirect_uris": "https://mirror.example/cb",
            "scopes": "read write follow",
            "website": "https://mirror.example",
        }),
    )
    .await;
    assert_eq!(registration.status, StatusCode::OK, "registered");
    let registered = registration.json_value();

    let app_row = oauth_app::get_by_client_id(
        &pool,
        registered["client_id"].as_str().expect("a client id"),
    )
    .await
    .expect("the lookup reaches the database")
    .expect("the registration stored a row");
    let token = oauth_token::create_token(
        &pool,
        &NewToken {
            user_id: None,
            app_id: app_row.id,
            scopes: "read".to_owned(),
            ttl: None,
        },
    )
    .await
    .expect("token fixture inserts");

    let response = common::TestRequest::get("/api/v1/apps/verify_credentials")
        .bearer(&token.token)
        .send(&router)
        .await;
    assert_eq!(response.status, StatusCode::OK, "200");
    let verified = response.json_value();

    let mut expected = registered
        .as_object()
        .expect("the registration body is an object")
        .clone();
    for secret in SECRET_FIELDS {
        let shown = expected.remove(secret);
        assert!(shown.is_some(), "the registration carries `{secret}`");
    }
    assert_eq!(
        verified,
        Value::Object(expected),
        "the verified body is the registration body without its secrets"
    );
}

/// No token at all is a 401 with Pleroma's anonymous-challenge wording
/// ("Invalid credentials", `EnsureAuthenticatedPlug`), still carrying the
/// bearer challenge header.
#[tokio::test]
async fn verify_credentials_without_a_token_is_unauthorized() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let router = common::test_app(pool);
    let response = common::TestRequest::get("/api/v1/apps/verify_credentials")
        .send(&router)
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED, "401");
    assert_eq!(
        response.header("www-authenticate"),
        Some("Bearer"),
        "the bearer challenge"
    );
    assert_eq!(
        response.text(),
        r#"{"error":"Invalid credentials"}"#,
        "the exact 401 body"
    );
}

/// An unknown token and one past its expiry are the same 401.
#[tokio::test]
async fn verify_credentials_rejects_unknown_and_expired_tokens() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let router = common::test_app(pool.clone());
    let app_row = common::create_app(&pool, "read").await;
    let lapsed = oauth_token::create_token(
        &pool,
        &NewToken {
            user_id: None,
            app_id: app_row.id,
            scopes: "read".to_owned(),
            ttl: None,
        },
    )
    .await
    .expect("token fixture inserts");
    common::expire_token(&pool, lapsed.id).await;

    let unknown = common::TestRequest::get("/api/v1/apps/verify_credentials")
        .bearer("never-minted-token")
        .send(&router)
        .await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED, "401");
    assert_eq!(
        unknown.text(),
        r#"{"error":"The access token is invalid"}"#,
        "the exact body for the unknown token"
    );

    let lapsed_response = common::TestRequest::get("/api/v1/apps/verify_credentials")
        .bearer(&lapsed.token)
        .send(&router)
        .await;
    assert_eq!(
        lapsed_response.status,
        StatusCode::UNAUTHORIZED,
        "an expired token is a 401"
    );
    assert_eq!(
        lapsed_response.text(),
        r#"{"error":"The access token is invalid"}"#,
        "the exact body for an expired token"
    );
}

/// A token whose owner's account is deactivated is refused too, matching
/// the extractors' rules.
#[tokio::test]
async fn verify_credentials_rejects_a_deactivated_owners_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let router = common::test_app(pool.clone());
    let user = common::create_deactivated_user_with_password(&pool, "gone", "pw12345!").await;
    let app_row = common::create_app(&pool, "read").await;
    let token = oauth_token::create_token(
        &pool,
        &NewToken {
            user_id: Some(user.id),
            app_id: app_row.id,
            scopes: "read".to_owned(),
            ttl: None,
        },
    )
    .await
    .expect("token fixture inserts");

    let response = common::TestRequest::get("/api/v1/apps/verify_credentials")
        .bearer(&token.token)
        .send(&router)
        .await;
    assert_eq!(
        response.status,
        StatusCode::UNAUTHORIZED,
        "a deactivated owner's token is a 401"
    );
    assert_eq!(
        response.text(),
        r#"{"error":"The access token is invalid"}"#,
        "the exact 401 body"
    );
}

/// The fixture app is also reachable through the [`create_app_and_token`]
/// helper: a user-bound token minted with the app in one step.
#[tokio::test]
async fn verify_credentials_accepts_the_combined_fixture_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let router = common::test_app(pool.clone());
    let user = common::create_local_user(&pool, "combined").await;
    let raw = common::create_app_and_token(&pool, &user, "read").await;
    let response = common::TestRequest::get("/api/v1/apps/verify_credentials")
        .bearer(&raw)
        .send(&router)
        .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the one-step fixture authenticates"
    );
    assert_no_secrets(&response.json_value());
}

/// The entity carries none of the three secret fields, as an object
/// shape (the key must not appear at all, not even as null).
fn assert_no_secrets(body: &Value) {
    let fields = body.as_object().expect("the entity is an object");
    for secret in SECRET_FIELDS {
        assert!(!fields.contains_key(secret), "`{secret}` must be absent");
    }
}
