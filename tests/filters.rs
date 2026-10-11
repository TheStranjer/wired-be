//! Filters over HTTP (`/api/v1/filters`, WBE-98): the Mastodon v1 CRUD.
//!
//! The tests pin what the ticket promises: create, read, list, replace and
//! delete round-trip the six-key `Filter` entity; `context[]` array keys
//! work in form bodies; an expired filter leaves the list but is still a
//! row its owner can read; a blank phrase and a bad context answer the
//! `{"error":"Validation failed: …"}` 422s; every route enforces its half
//! of the `AuthUser` + scope pipeline (`read:filters` to look,
//! `write:filters` to change); and an id that is malformed, unknown or
//! another account's is one and the same 404.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::{Router, http::StatusCode};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    models::{
        filter::{self, NewFilter},
        user::User,
    },
    util::time::parse_time,
};

/// The root path all five endpoints live under.
const ROOT: &str = "/api/v1/filters";

/// Every JSON key a filter entity must carry, sorted.
const EXPECTED_KEYS: [&str; 6] = [
    "context",
    "expires_at",
    "id",
    "irreversible",
    "phrase",
    "whole_word",
];

/// A local user named `nickname` plus a bearer token granting `scopes`.
async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// Creates a filter through the model — `phrase` in `home`, expiring at
/// `expires_at` — and returns its row.
async fn seed(
    pool: &PgPool,
    owner: &User,
    text: &str,
    expires_at: Option<DateTime<Utc>>,
) -> filter::Filter {
    filter::create(
        pool,
        owner.id,
        &NewFilter {
            phrase: text.to_owned(),
            context: vec!["home".to_owned()],
            expires_at,
            ..NewFilter::default()
        },
    )
    .await
    .expect("fixture filter creates")
}

/// The minimal valid create body.
fn create_body() -> Value {
    json!({"phrase": "Spoilers", "context": ["home"]})
}

/// `POST /api/v1/filters` with the JSON `body` as `token`.
async fn create_json(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(ROOT)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// `POST /api/v1/filters` with the form `fields` as `token`.
async fn create_form(app: &Router, token: &str, fields: &[(&str, &str)]) -> common::TestResponse {
    common::TestRequest::post(ROOT)
        .bearer(token)
        .form(fields)
        .send(app)
        .await
}

/// `GET /api/v1/filters` as `token`.
async fn list(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get(ROOT).bearer(token).send(app).await
}

/// `GET /api/v1/filters` as `token`, asserted 200 and parsed.
async fn listed(app: &Router, token: &str) -> Vec<Value> {
    let response = list(app, token).await;
    assert_eq!(response.status, StatusCode::OK, "the list answers 200");
    response.json()
}

/// `GET /api/v1/filters/{id}` as `token`.
async fn show(app: &Router, token: &str, id: Uuid) -> common::TestResponse {
    common::TestRequest::get(&format!("{ROOT}/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// `PUT /api/v1/filters/{id}` with the JSON `body` as `token`.
async fn update(app: &Router, token: &str, id: Uuid, body: Value) -> common::TestResponse {
    common::TestRequest::put(&format!("{ROOT}/{id}"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// `DELETE /api/v1/filters/{id}` as `token`.
async fn destroy(app: &Router, token: &str, id: Uuid) -> common::TestResponse {
    common::TestRequest::delete(&format!("{ROOT}/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// The `phrase` of every element of `rows`, in order.
fn phrases(rows: &[Value]) -> Vec<&str> {
    rows.iter()
        .map(|row| {
            row["phrase"]
                .as_str()
                .expect("every filter carries its phrase")
        })
        .collect()
}

/// The keys of a JSON object, sorted, for exact key-set assertions.
fn json_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("filters render objects")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// An account with nothing stored answers 200 with the empty JSON array.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn empty_database_answers_the_empty_list(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn crud_round_trip(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn form_bodies_with_array_keys_are_accepted(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn update_replaces_defaults_instead_of_keeping_stored_values(pool: PgPool) {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn expired_filters_are_hidden_from_the_list(pool: PgPool) {
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

/// The `{"error":"Validation failed: …"}` bodies the ticket names, and
/// their precedence: validation runs before any lookup, so even a PUT to
/// a nonexistent id answers 422 for a bad body.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn invalid_parameters_answer_mastodons_validation_wording(pool: PgPool) {
    let (_alice, token) = fixture(&pool, "alice", "write:filters").await;
    let app = common::test_app(pool.clone());

    for (what, body, message) in [
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
    ] {
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
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn ownership_and_missing_ids_answer_404(pool: PgPool) {
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

/// Reading needs a user token with `read:filters` (or the broader `read`);
/// writing needs one with `write:filters` (or `write`). Missing tokens are
/// the 401s, app-only tokens and missing scopes the 403s, all with
/// Pleroma's wording.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_scope_pipeline_guards_every_route(pool: PgPool) {
    let (alice, alice_token) = fixture(&pool, "alice", "read write").await;
    let hers = seed(&pool, &alice, "hers", None).await;
    let (_reader, read_only) = fixture(&pool, "reader", "read").await;
    let (_writer, write_broad) = fixture(&pool, "writer", "write").await;
    let (_narrow, wrong_scope) = fixture(&pool, "narrow", "read:statuses write:statuses").await;
    let (_filer, filter_read) = fixture(&pool, "filer", "read:filters").await;
    let (_mutator, filter_write) = fixture(&pool, "mutator", "write:filters").await;
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let app = common::test_app(pool.clone());

    let anonymous = common::TestRequest::get(ROOT).send(&app).await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "no token is a 401"
    );
    assert_eq!(
        anonymous.json_value()["error"],
        "Invalid credentials",
        "Pleroma's wording for an absent token"
    );
    let bogus = list(&app, "not-a-real-token").await;
    assert_eq!(
        bogus.status,
        StatusCode::UNAUTHORIZED,
        "a bogus token is a 401"
    );
    assert_eq!(
        bogus.json_value()["error"],
        "The access token is invalid",
        "naming the token as the problem"
    );
    let as_app = list(&app, &app_only).await;
    assert_eq!(
        as_app.status,
        StatusCode::FORBIDDEN,
        "an app token is a 403"
    );
    assert_eq!(
        as_app.json_value()["error"],
        "This API requires an authenticated user",
        "naming what is missing"
    );

    for (who, token) in [("read:statuses", &wrong_scope), ("write", &write_broad)] {
        let response = list(&app, token).await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "`{who}` may not list"
        );
        assert_eq!(
            response.json_value()["error"],
            "Insufficient permissions: read:filters.",
            "the denial names the missing scope for `{who}`"
        );
        let shown = show(&app, token, hers.id).await;
        assert_eq!(shown.status, StatusCode::FORBIDDEN, "`{who}` may not show");
    }

    for (who, token) in [("read:statuses", &wrong_scope), ("read", &read_only)] {
        let response = create_json(&app, token, create_body()).await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "`{who}` may not create"
        );
        assert_eq!(
            response.json_value()["error"],
            "Insufficient permissions: write:filters.",
            "the denial names the missing scope for `{who}`"
        );
        let replaced = update(
            &app,
            token,
            hers.id,
            json!({"phrase": "x", "context": ["home"]}),
        )
        .await;
        assert_eq!(
            replaced.status,
            StatusCode::FORBIDDEN,
            "`{who}` may not update"
        );
        let deleted = destroy(&app, token, hers.id).await;
        assert_eq!(
            deleted.status,
            StatusCode::FORBIDDEN,
            "`{who}` may not delete"
        );
    }

    // The granted side: the narrow sub-scopes on their own users, and the
    // broad trees on the owner of the row touched by id.
    assert_eq!(
        list(&app, &filter_read).await.status,
        StatusCode::OK,
        "`read:filters` lists"
    );
    assert_eq!(
        create_json(&app, &filter_write, create_body()).await.status,
        StatusCode::OK,
        "`write:filters` creates"
    );
    assert_eq!(
        show(&app, &alice_token, hers.id).await.status,
        StatusCode::OK,
        "the broad `read` shows"
    );
    assert_eq!(
        create_json(&app, &alice_token, create_body()).await.status,
        StatusCode::OK,
        "the broad `write` creates"
    );
    assert_eq!(
        update(
            &app,
            &alice_token,
            hers.id,
            json!({"phrase": "rewritten", "context": ["home"]})
        )
        .await
        .status,
        StatusCode::OK,
        "and updates"
    );
    assert_eq!(
        destroy(&app, &alice_token, hers.id).await.status,
        StatusCode::OK,
        "and deletes"
    );
    assert_eq!(
        phrases(&listed(&app, &alice_token).await),
        ["Spoilers"],
        "alice's list afterwards: her created row, the deleted one gone"
    );
    assert_eq!(
        phrases(&listed(&app, &filter_read).await),
        Vec::<&str>::new(),
        "while the denial loop above stored nothing for anybody"
    );
}
