//! Filters over HTTP (`/api/v1/filters`, WBE-98): the Mastodon v1 CRUD,
//! and (WBE-129) the v1 application of those filters to the rendered
//! listings.
//!
//! The CRUD tests pin what the ticket promises: create, read, list, replace
//! and delete round-trip the six-key `Filter` entity; `context[]` array keys
//! work in form bodies; an expired filter leaves the list but is still a
//! row its owner can read; a blank phrase and a bad context answer the
//! `{"error":"Validation failed: …"}` 422s; every route enforces its half
//! of the `AuthUser` + scope pipeline (`read:filters` to look,
//! `write:filters` to change); and an id that is malformed, unknown or
//! another account's is one and the same 404.
//!
//! The application tests under `services::filter`'s semantics: a warning
//! filter keeps its matches and sets the status entity's `filtered` array
//! to the v2 `FilterResult` shape; an `irreversible` one removes them from
//! the home, public, account and thread pages — and drops the whole
//! notification whose status it matched from the listing; whole-word and
//! context scoping, expiry, reblog originals and the viewer's own-status
//! exemption all decide the page, and an unauthenticated (or unfiltered)
//! read carries the empty array everywhere.
//!
//! And (WBE-191 slice 5) every successful write publishes Mastodon's
//! `filters_changed` onto the hub — the owner's `user:` stream, the owner's
//! audience, no payload — while a refused write publishes nothing.

#![cfg(test)]

use axum::{Router, http::StatusCode};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::broadcast;
use uuid::Uuid;
use wired_be::{
    models::{
        filter::{self, NewFilter},
        follow,
        status::{self, NewStatus, Status, Visibility},
        user::User,
    },
    streaming::hub::{Audience, StreamEvent},
    util::time::parse_time,
};

use crate::common;

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

/// The credential half of the pipeline: no token at all is the 401 with
/// Pleroma's "Invalid credentials", a token no row carries the 401 naming it,
/// and an app-only token the 403 demanding an authenticated user.
#[tokio::test]
async fn the_credential_gates_refuse_every_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", "read write").await;
    let hers = seed(&pool, &alice, "hers", None).await;
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let app = common::test_app(pool);

    let anonymous = common::TestRequest::get(ROOT).send(&app).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token is a 401");
    common::assert_error(&anonymous, "Invalid credentials", "the absent token");

    let bogus = [
        list(&app, "not-a-real-token").await,
        show(&app, "not-a-real-token", hers.id).await,
    ];
    common::assert_all_refused(
        &bogus,
        StatusCode::UNAUTHORIZED,
        "The access token is invalid",
        "a token no row carries",
    );

    let userless = [
        list(&app, &app_only).await,
        create_json(&app, &app_only, create_body()).await,
    ];
    common::assert_all_refused(
        &userless,
        StatusCode::FORBIDDEN,
        "This API requires an authenticated user",
        "an app-only token",
    );
}

/// The scope half of the pipeline: reading needs `read:filters` (or `read`)
/// and writing `write:filters` (or `write`); every route a token lacks the
/// scope for answers the 403 naming it.
#[tokio::test]
async fn tokens_without_the_route_scope_are_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", "read write").await;
    let hers = seed(&pool, &alice, "hers", None).await;
    let (_reader, read_only) = fixture(&pool, "reader", "read").await;
    let (_writer, write_broad) = fixture(&pool, "writer", "write").await;
    let (_narrow, wrong_scope) = fixture(&pool, "narrow", "read:statuses write:statuses").await;
    let app = common::test_app(pool);

    for (who, token) in [("read:statuses", &wrong_scope), ("write", &write_broad)] {
        let refused = [list(&app, token).await, show(&app, token, hers.id).await];
        common::assert_all_refused(
            &refused,
            StatusCode::FORBIDDEN,
            "Insufficient permissions: read:filters.",
            &format!("`{who}` on every reading route"),
        );
    }
    for (who, token) in [("read:statuses", &wrong_scope), ("read", &read_only)] {
        let refused = [
            create_json(&app, token, create_body()).await,
            update(
                &app,
                token,
                hers.id,
                json!({"phrase": "x", "context": ["home"]}),
            )
            .await,
            destroy(&app, token, hers.id).await,
        ];
        common::assert_all_refused(
            &refused,
            StatusCode::FORBIDDEN,
            "Insufficient permissions: write:filters.",
            &format!("`{who}` on every writing route"),
        );
    }
}

/// The granted side: the narrow sub-scopes pass on their own routes, the
/// broad `read` and `write` trees pass everywhere, and what was denied really
/// stored nothing.
#[tokio::test]
async fn the_granted_scopes_use_every_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read write").await;
    let hers = seed(&pool, &alice, "hers", None).await;
    let (_filer, filter_read) = fixture(&pool, "filer", "read:filters").await;
    let (_mutator, filter_write) = fixture(&pool, "mutator", "write:filters").await;
    let app = common::test_app(pool);

    let granted_reads = [
        list(&app, &filter_read).await,
        show(&app, &alice_token, hers.id).await,
    ];
    common::assert_all_answered(
        &granted_reads,
        StatusCode::OK,
        "`read:filters` lists and the broad read shows",
    );
    let rewrite = json!({"phrase": "rewritten", "context": ["home"]});
    let granted_writes = [
        create_json(&app, &filter_write, create_body()).await,
        create_json(&app, &alice_token, create_body()).await,
        update(&app, &alice_token, hers.id, rewrite).await,
        destroy(&app, &alice_token, hers.id).await,
    ];
    common::assert_all_answered(
        &granted_writes,
        StatusCode::OK,
        "`write:filters` creates; the broad write creates, updates and deletes",
    );
    assert_eq!(
        phrases(&listed(&app, &alice_token).await),
        ["Spoilers"],
        "alice's list afterwards: her created row, the deleted one gone"
    );
    assert_eq!(
        phrases(&listed(&app, &filter_read).await),
        Vec::<&str>::new(),
        "while the read-only filer stored nothing of its own"
    );
}

// ---------------------------------------------------------------------------
// Filter application: the v1 semantics of `services::filter` over HTTP —
// the home, public and account timelines, the notification listing and the
// status' thread context.
// ---------------------------------------------------------------------------

/// The instance origin the fixtures post under.
const BASE: &str = "https://test.example";
/// The home timeline route.
const HOME: &str = "/api/v1/timelines/home";
/// The public timeline route.
const PUBLIC: &str = "/api/v1/timelines/public";
/// The notification listing route.
const NOTES: &str = "/api/v1/notifications";

/// `GET`s `path` with `token`.
async fn get_as(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// `GET`s `path` with `token`, asserted 200, parsed.
async fn page_of(app: &Router, token: &str, path: &str) -> Value {
    let response = get_as(app, token, path).await;
    assert_eq!(response.status, StatusCode::OK, "`{path}` answers 200");
    response.json_value()
}

/// Creates `owner`'s warning filter (whole-word, kept-but-flagged) for
/// `phrase` in `contexts`, and returns the stored row.
async fn warner(pool: &PgPool, owner: &User, phrase: &str, contexts: &[&str]) -> filter::Filter {
    make_filter(
        pool,
        owner,
        &NewFilter {
            phrase: phrase.to_owned(),
            context: contexts.iter().map(|name| (*name).to_owned()).collect(),
            ..NewFilter::default()
        },
    )
    .await
}

/// The same filter hiding (irreversible) instead of warning.
async fn hider(pool: &PgPool, owner: &User, phrase: &str, contexts: &[&str]) -> filter::Filter {
    make_filter(
        pool,
        owner,
        &NewFilter {
            phrase: phrase.to_owned(),
            context: contexts.iter().map(|name| (*name).to_owned()).collect(),
            irreversible: true,
            ..NewFilter::default()
        },
    )
    .await
}

/// The same warning filter with whole-word off: a substring match.
async fn looser(pool: &PgPool, owner: &User, phrase: &str, contexts: &[&str]) -> filter::Filter {
    make_filter(
        pool,
        owner,
        &NewFilter {
            phrase: phrase.to_owned(),
            context: contexts.iter().map(|name| (*name).to_owned()).collect(),
            whole_word: false,
            ..NewFilter::default()
        },
    )
    .await
}

/// The one model write the three seeding helpers share.
async fn make_filter(pool: &PgPool, owner: &User, body: &NewFilter) -> filter::Filter {
    filter::create(pool, owner.id, body)
        .await
        .expect("fixture filter creates")
}

/// The expected `filtered` entry for a match of `f`: the ticket's v2
/// `FilterResult` shape, as the wire carries it.
fn expected(f: &filter::Filter, context: &str, whole_word: bool) -> Value {
    json!({
        "filter": {
            "id": f.id.to_string(),
            "title": f.phrase,
            "context": [context],
            "expires_at": Value::Null,
            "filter_action": "warn",
            "keywords": [{
                "id": f.id.to_string(),
                "keyword": f.phrase,
                "whole_word": whole_word,
            }],
            "statuses": [],
        },
        "keyword_matches": [f.phrase],
        "status_matches": Value::Null,
    })
}

/// The ids of the statuses of a rendered page, in answer order.
fn ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("a listing is a JSON array")
        .iter()
        .map(|status| {
            status["id"]
                .as_str()
                .expect("every Status entity carries its id")
                .to_owned()
        })
        .collect()
}

/// The `filtered` array of the page entry with id `id`.
fn filtered_of(body: &Value, id: &str) -> Value {
    let entries = body.as_array().expect("a listing is a JSON array");
    entries
        .iter()
        .find(|status| status["id"] == json!(id))
        .unwrap_or_else(|| panic!("`{id}` is on the page"))["filtered"]
        .clone()
}

/// The ids of `posts`, newest first — the order the timelines answer in
/// (decision 4: `UUIDv7` text sorts by creation).
fn newest_first(posts: &[&Status]) -> Vec<String> {
    let mut ordered: Vec<Uuid> = posts.iter().map(|post| post.id).collect();
    ordered.sort_by(|a, b| b.cmp(a));
    ordered.into_iter().map(|id| id.to_string()).collect()
}

/// The id of `post`, as its JSON carries it.
fn id_of(post: &Status) -> String {
    post.id.to_string()
}

/// Inserts `author`'s public reply to `parent`.
async fn reply(pool: &PgPool, author: &User, parent: &Status, text: &str) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(parent.user_id),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("a fixture reply inserts")
}

/// A warning filter annotates the matching home-timeline status with the
/// v2 result — and whole-word matching decides what counts: the glued
/// substring and the clean post stay an empty `filtered` array.
#[tokio::test]
async fn a_home_warning_annotates_whole_word_matches() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _followed = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows bob");
    let bounded = common::create_status(&pool, &bob, "a foo bar", Visibility::Public).await;
    let glued = common::create_status(&pool, &bob, "loved the foood", Visibility::Public).await;
    let clean = common::create_status(&pool, &bob, "quiet evening", Visibility::Public).await;
    let phrase = warner(&pool, &alice, "foo", &["home"]).await;
    let app = common::test_app(pool);

    let body = page_of(&app, &alice_token, HOME).await;
    assert_eq!(
        ids(&body),
        newest_first(&[&bounded, &glued, &clean]),
        "a warning never removes a status"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&bounded)),
        json!([expected(&phrase, "home", true)]),
        "the bounded occurrence is annotated with exactly the ticket's shape"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&glued)),
        json!([]),
        "the glued occurrence is no whole word"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&clean)),
        json!([]),
        "and the unmatched status carries the empty array"
    );
}

/// Whole-word off makes the glued occurrence a substring match — while the
/// viewer's own matching status stays exempt and unannotated.
#[tokio::test]
async fn a_loose_home_filter_matches_substrings_but_never_the_viewers_own() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _followed = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows bob");
    let glued = common::create_status(&pool, &bob, "loved the foood", Visibility::Public).await;
    let own = common::create_status(&pool, &alice, "my own foo", Visibility::Public).await;
    let phrase = looser(&pool, &alice, "foo", &["home"]).await;
    let app = common::test_app(pool);

    let body = page_of(&app, &alice_token, HOME).await;
    assert_eq!(
        filtered_of(&body, &id_of(&glued)),
        json!([expected(&phrase, "home", false)]),
        "the substring of a followed post annotates"
    );
    assert_eq!(
        ids(&body),
        newest_first(&[&own, &glued]),
        "and the viewer's own match was never in question"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&own)),
        json!([]),
        "exempt means unannotated too"
    );
}

/// An irreversible filter removes its matches from the home timeline — and
/// never the viewer's own statuses.
#[tokio::test]
async fn an_irreversible_home_filter_removes_matches_and_spares_own_posts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _followed = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows bob");
    let own = common::create_status(&pool, &alice, "my own spam", Visibility::Public).await;
    let theirs = common::create_status(&pool, &bob, "their spam", Visibility::Public).await;
    let clean = common::create_status(&pool, &bob, "quiet evening", Visibility::Public).await;
    let _doomed = hider(&pool, &alice, "spam", &["home"]).await;
    let app = common::test_app(pool);

    let body = page_of(&app, &alice_token, HOME).await;
    assert_eq!(
        ids(&body),
        newest_first(&[&own, &clean]),
        "the stranger's match leaves the page and the viewer's own stays"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&own)),
        json!([]),
        "the exempt match carries the empty annotation"
    );
    assert!(
        !ids(&body).contains(&id_of(&theirs)),
        "and it left no annotated trace"
    );
}

/// An expired filter removes and annotates nothing.
#[tokio::test]
async fn an_expired_home_filter_changes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let _followed = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows bob");
    let row = common::create_status(&pool, &bob, "the spam word", Visibility::Public).await;
    let lapsed = Utc::now() - Duration::days(1);
    let _gone = seed(&pool, &alice, "spam", Some(lapsed)).await;
    let app = common::test_app(pool);

    let body = page_of(&app, &alice_token, HOME).await;
    assert_eq!(
        ids(&body),
        vec![id_of(&row)],
        "the expired row hides nothing"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&row)),
        json!([]),
        "and annotates nothing"
    );
}

/// Context scoping over HTTP: on the public page a `public` filter applies
/// and a `home`-only one does not — and the viewer's own post stays even
/// under a matching public filter.
#[tokio::test]
async fn the_public_page_answers_to_public_context_filters_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let home_match = common::create_status(&pool, &bob, "the key word", Visibility::Public).await;
    let public_match =
        common::create_status(&pool, &bob, "the spam sign", Visibility::Public).await;
    let own = common::create_status(&pool, &alice, "my spam too", Visibility::Public).await;
    let _home_only = hider(&pool, &alice, "word", &["home"]).await;
    let _public_hider = hider(&pool, &alice, "spam", &["public"]).await;
    let app = common::test_app(pool);

    let body = page_of(&app, &alice_token, PUBLIC).await;
    assert!(
        ids(&body).contains(&id_of(&home_match)),
        "the home-only filter ignores this page"
    );
    assert!(
        !ids(&body).contains(&id_of(&public_match)),
        "the public-context irreversible match is gone, leaving no entry \
         to annotate"
    );
    assert!(
        ids(&body).contains(&id_of(&own)),
        "the viewer's own public match stays, unannotated"
    );
    assert_eq!(
        filtered_of(&body, &id_of(&own)),
        json!([]),
        "the exemption reads as an empty array"
    );
}

/// A boost is filtered by its original's text: the irreversible match
/// takes the reblog row out with the original, and the warning filter
/// annotates the boost that rides the thread.
#[tokio::test]
async fn a_boost_is_filtered_by_its_originals_text() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let original =
        common::create_status(&pool, &bob, "the spam original", Visibility::Public).await;
    let boost = status::create_reblog(&pool, BASE, &carol, &original, Visibility::Public)
        .await
        .expect("a fixture reblog writes")
        .0;
    let hider_row = hider(&pool, &alice, "spam", &["public"]).await;
    let app = common::test_app(pool.clone());

    let body = page_of(&app, &alice_token, PUBLIC).await;
    assert!(
        !ids(&body).contains(&id_of(&boost)) && !ids(&body).contains(&id_of(&original)),
        "the irreversible match takes the boost and its original out"
    );

    let deleted = filter::delete(&pool, hider_row.id, alice.id)
        .await
        .expect("the hider deletes");
    assert!(deleted, "and the row went");
    let warner_row = warner(&pool, &alice, "spam", &["public"]).await;
    let warned = page_of(&app, &alice_token, PUBLIC).await;
    assert_eq!(
        ids(&warned),
        newest_first(&[&boost, &original]),
        "the warning brings both back"
    );
    assert_eq!(
        filtered_of(&warned, &id_of(&boost)),
        json!([expected(&warner_row, "public", true)]),
        "and the boost rides its original's match, annotated"
    );
}

/// The notification listing annotates a warning match on the notification's
/// status, leaves status-less rows and the viewer's own posts alone, and
/// the irreversible match drops the whole notification.
#[tokio::test]
async fn the_notification_listing_applies_notifications_context_filters() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:notifications").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let matched = common::create_status(&pool, &bob, "a spam favourite", Visibility::Public).await;
    let own = common::create_status(&pool, &alice, "my own spam", Visibility::Public).await;
    let on_matched =
        common::create_notification(&pool, &alice, &bob, "favourite", Some(&matched)).await;
    let on_own = common::create_notification(&pool, &alice, &bob, "favourite", Some(&own)).await;
    let no_status = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let hider_row = hider(&pool, &alice, "favourite", &["notifications"]).await;
    let warn_row = warner(&pool, &alice, "spam", &["notifications"]).await;
    let app = common::test_app(pool.clone());

    let body = page_of(&app, &alice_token, NOTES).await;
    let entities = body.as_array().expect("the listing is an array");
    assert_eq!(
        entities.len(),
        2,
        "the notification about the viewer's own post and the follow stay, \
         the irreversible match is gone"
    );
    assert_eq!(
        entities[0]["id"],
        no_status.id.to_string(),
        "the status-less newest row is untouched"
    );
    let own_note = &entities[1];
    assert_eq!(
        own_note["id"],
        on_own.id.to_string(),
        "and the own-status row"
    );
    assert_eq!(
        own_note["status"]["filtered"],
        json!([]),
        "the viewer's own status is exempt even in the notifications context"
    );

    let _gone = filter::delete(&pool, hider_row.id, alice.id)
        .await
        .expect("the hider deletes");
    let warned = page_of(&app, &alice_token, NOTES).await;
    let notes = warned.as_array().expect("the listing is an array");
    assert_eq!(notes.len(), 3, "with the hider gone all three rows read");
    let liked = notes
        .iter()
        .find(|note| note["id"] == on_matched.id.to_string())
        .expect("the favourite notification is back");
    assert_eq!(
        liked["status"]["filtered"],
        json!([expected(&warn_row, "notifications", true)]),
        "its status carries the warning annotation"
    );
    assert!(
        notes
            .iter()
            .any(|note| note["id"] == no_status.id.to_string() && note.get("status").is_none()),
        "a follow notification carries no status key at all"
    );
}

/// The thread context pass annotates and removes on both arms of
/// `GET /statuses/{id}/context`, and an anonymous read is the no-op.
#[tokio::test]
async fn the_thread_context_filters_both_arms() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let root = common::create_status(&pool, &bob, "the spam root", Visibility::Public).await;
    let middle = reply(&pool, &carol, &root, "quiet middle").await;
    let leaf = reply(&pool, &bob, &middle, "a spam reply").await;
    let hider_row = hider(&pool, &alice, "spam", &["thread"]).await;
    let app = common::test_app(pool.clone());

    let context = page_of(
        &app,
        &alice_token,
        &format!("/api/v1/statuses/{}/context", middle.id),
    )
    .await;
    assert_eq!(
        context["ancestors"],
        json!([]),
        "the matched ancestor is gone"
    );
    assert_eq!(
        context["descendants"],
        json!([]),
        "and the matched descendant"
    );

    let deleted = filter::delete(&pool, hider_row.id, alice.id)
        .await
        .expect("the hider deletes");
    assert!(deleted, "and the row went");
    let warner_row = warner(&pool, &alice, "spam", &["thread"]).await;
    let warned = page_of(
        &app,
        &alice_token,
        &format!("/api/v1/statuses/{}/context", middle.id),
    )
    .await;
    assert_eq!(
        warned["ancestors"][0]["filtered"],
        json!([expected(&warner_row, "thread", true)]),
        "with the hider gone the ancestor reads annotated"
    );
    assert_eq!(
        warned["descendants"][0]["filtered"],
        json!([expected(&warner_row, "thread", true)]),
        "and so does the descendant"
    );

    let anonymous = common::TestRequest::get(&format!("/api/v1/statuses/{}/context", middle.id))
        .send(&app)
        .await;
    assert_eq!(
        anonymous.status,
        StatusCode::OK,
        "an anonymous thread reads"
    );
    let anon = anonymous.json_value();
    assert_eq!(
        anon["ancestors"].as_array().expect("an array").len(),
        1,
        "missing filters (no auth) remove nothing"
    );
    assert_eq!(
        anon["ancestors"][0]["filtered"],
        json!([]),
        "and annotate nothing"
    );
    assert_eq!(
        ids(&anon["descendants"]),
        vec![id_of(&leaf)],
        "the whole thread stands"
    );
}

/// The account-status listing filters for a stranger — and not for the
/// account owner reading their own profile.
#[tokio::test]
async fn the_account_listing_filters_for_strangers_never_for_the_owner() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read:statuses").await;
    let (bob, bob_token) = fixture(&pool, "bob", "read:statuses").await;
    let matched = common::create_status(&pool, &bob, "profile spam", Visibility::Public).await;
    let clean = common::create_status(&pool, &bob, "profile clean", Visibility::Public).await;
    let profile = format!("/api/v1/accounts/{}/statuses", bob.id);
    let _stranger_hider = hider(&pool, &alice, "spam", &["account"]).await;
    let _owner_hider = hider(&pool, &bob, "spam", &["account"]).await;
    let app = common::test_app(pool);

    let as_alice = page_of(&app, &alice_token, &profile).await;
    assert_eq!(
        ids(&as_alice),
        vec![id_of(&clean)],
        "the stranger's account-context filter removes the match"
    );
    let as_bob = page_of(&app, &bob_token, &profile).await;
    assert_eq!(
        ids(&as_bob),
        newest_first(&[&matched, &clean]),
        "the owner's own matching filter hides nothing of their profile"
    );
    assert_eq!(
        filtered_of(&as_bob, &id_of(&matched)),
        json!([]),
        "and annotates nothing either"
    );
}

/// Every rendered status carries the `filtered` key, even when the viewer
/// has no filters at all.
#[tokio::test]
async fn a_user_without_filters_reads_the_empty_array() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_carol, carol_token) = fixture(&pool, "carol", "read:statuses").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let row = common::create_status(&pool, &bob, "plain post", Visibility::Public).await;
    let app = common::test_app(pool);

    let body = page_of(&app, &carol_token, PUBLIC).await;
    assert_eq!(ids(&body), vec![id_of(&row)], "the page is the page");
    assert_eq!(
        filtered_of(&body, &id_of(&row)),
        json!([]),
        "and the entity answers the empty filtered array"
    );
}

/// The `filters_changed` event the hub holds after a successful write,
/// asserted to be exactly what the emitter owes (WBE-191 slice 5): bare,
/// on the owner's own `user:` stream, with the owner as the whole audience
/// — which at the socket is what reaches nobody else.
fn assert_filters_changed(event: &StreamEvent, owner: &User) {
    assert_eq!(
        event.event, "filters_changed",
        "Mastodon's filter-changed event"
    );
    assert_eq!(
        event.streams,
        vec![format!("user:{}", owner.id)],
        "the event rides exactly the owner's user stream: {:?}",
        event.streams
    );
    assert_eq!(
        event.audience,
        Audience::Users(vec![owner.id]),
        "the owner is the whole audience"
    );
    assert!(event.payload.is_none(), "the event is bare");
    assert!(event.render.is_none(), "and there is no entity to render");
    assert!(
        !event.local_only,
        "the audience already names a local owner"
    );
}

/// One write, one event: nothing else waits on the hub.
///
/// # Panics
///
/// If the hub holds an event the test did not account for.
fn assert_one_event_only(rx: &mut broadcast::Receiver<Arc<StreamEvent>>) {
    assert!(
        matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
        "the write published exactly one event"
    );
}

/// A successful `POST` publishes Mastodon's `filters_changed` onto the hub,
/// on the owner's stream and to the owner's audience alone.
#[tokio::test]
async fn a_created_filter_publishes_filters_changed_to_the_owner_only() {
    let db = common::test_db().await;
    let state = common::test_app_state(db.pool().clone(), common::test_config());
    let app = wired_be::app(state.clone());
    let (alice, token) = fixture(&state.db, "push-alice", "write:filters").await;
    let mut rx = state.hub.subscribe();

    let response = create_json(&app, &token, create_body()).await;
    assert_eq!(response.status, StatusCode::OK, "the create answers 200");

    let event = rx.recv().await.expect("the create published an event");
    assert_filters_changed(&event, &alice);
    assert_one_event_only(&mut rx);
}

/// A successful `PUT` publishes it the same way.
#[tokio::test]
async fn an_updated_filter_publishes_filters_changed_to_the_owner_only() {
    let db = common::test_db().await;
    let state = common::test_app_state(db.pool().clone(), common::test_config());
    let app = wired_be::app(state.clone());
    let (alice, token) = fixture(&state.db, "push-alice", "write:filters").await;
    let row = seed(&state.db, &alice, "Spoilers", None).await;
    let mut rx = state.hub.subscribe();

    let response = update(&app, &token, row.id, create_body()).await;
    assert_eq!(response.status, StatusCode::OK, "the update answers 200");

    let event = rx.recv().await.expect("the update published an event");
    assert_filters_changed(&event, &alice);
    assert_one_event_only(&mut rx);
}

/// A successful `DELETE` publishes it too.
#[tokio::test]
async fn a_deleted_filter_publishes_filters_changed_to_the_owner_only() {
    let db = common::test_db().await;
    let state = common::test_app_state(db.pool().clone(), common::test_config());
    let app = wired_be::app(state.clone());
    let (alice, token) = fixture(&state.db, "push-alice", "write:filters").await;
    let row = seed(&state.db, &alice, "Spoilers", None).await;
    let mut rx = state.hub.subscribe();

    let response = destroy(&app, &token, row.id).await;
    assert_eq!(response.status, StatusCode::OK, "the delete answers 200");

    let event = rx.recv().await.expect("the delete published an event");
    assert_filters_changed(&event, &alice);
    assert_one_event_only(&mut rx);
}

/// The refusal paths publish nothing: a write that never happened tells the
/// owner's sockets nothing. A 422 create, a 404 update and a 404 delete are
/// the three ways these routes refuse.
#[tokio::test]
async fn a_refused_filter_write_publishes_nothing() {
    let db = common::test_db().await;
    let state = common::test_app_state(db.pool().clone(), common::test_config());
    let app = wired_be::app(state.clone());
    let (alice, token) = fixture(&state.db, "push-alice", "write:filters").await;
    let row = seed(&state.db, &alice, "Spoilers", None).await;
    let mut rx = state.hub.subscribe();

    let rejected = create_json(&app, &token, json!({"phrase": "", "context": ["home"]})).await;
    assert_eq!(rejected.status, StatusCode::UNPROCESSABLE_ENTITY);
    let missing = update(&app, &token, Uuid::now_v7(), create_body()).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let gone = destroy(&app, &token, Uuid::now_v7()).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    let expired = destroy(&app, &token, row.id).await;
    assert_eq!(
        expired.status,
        StatusCode::OK,
        "the real delete does answer"
    );

    let event = rx.recv().await.expect("the delete published an event");
    assert_filters_changed(&event, &alice);
    assert_one_event_only(&mut rx);
}
