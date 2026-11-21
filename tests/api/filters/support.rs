//! Shared consts, fixtures and helpers for every submodule of the
//! `filters` area: the routes and the entity keys, the thin wrappers around
//! the HTTP harness (the five CRUD calls, the fixture user and its token,
//! the seeds and the filter makers), and the readers of the rendered pages
//! the application tests weigh. Everything here is `pub(super)`, and a
//! submodule imports exactly the members it calls.

use axum::{Router, http::StatusCode};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    filter::{self, NewFilter},
    status::{self, NewStatus, Status},
    user::User,
};

use crate::common;

/// The root path all five endpoints live under.
pub(super) const ROOT: &str = "/api/v1/filters";

/// Every JSON key a filter entity must carry, sorted.
pub(super) const EXPECTED_KEYS: [&str; 6] = [
    "context",
    "expires_at",
    "id",
    "irreversible",
    "phrase",
    "whole_word",
];

/// A local user named `nickname` plus a bearer token granting `scopes`.
pub(super) async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// Creates a filter through the model — `phrase` in `home`, expiring at
/// `expires_at` — and returns its row.
pub(super) async fn seed(
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
pub(super) fn create_body() -> Value {
    json!({"phrase": "Spoilers", "context": ["home"]})
}

/// `POST /api/v1/filters` with the JSON `body` as `token`.
pub(super) async fn create_json(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(ROOT)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// `POST /api/v1/filters` with the form `fields` as `token`.
pub(super) async fn create_form(
    app: &Router,
    token: &str,
    fields: &[(&str, &str)],
) -> common::TestResponse {
    common::TestRequest::post(ROOT)
        .bearer(token)
        .form(fields)
        .send(app)
        .await
}

/// `GET /api/v1/filters` as `token`.
pub(super) async fn list(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get(ROOT).bearer(token).send(app).await
}

/// `GET /api/v1/filters` as `token`, asserted 200 and parsed.
pub(super) async fn listed(app: &Router, token: &str) -> Vec<Value> {
    let response = list(app, token).await;
    assert_eq!(response.status, StatusCode::OK, "the list answers 200");
    response.json()
}

/// `GET /api/v1/filters/{id}` as `token`.
pub(super) async fn show(app: &Router, token: &str, id: Uuid) -> common::TestResponse {
    common::TestRequest::get(&format!("{ROOT}/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// `PUT /api/v1/filters/{id}` with the JSON `body` as `token`.
pub(super) async fn update(
    app: &Router,
    token: &str,
    id: Uuid,
    body: Value,
) -> common::TestResponse {
    common::TestRequest::put(&format!("{ROOT}/{id}"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// `DELETE /api/v1/filters/{id}` as `token`.
pub(super) async fn destroy(app: &Router, token: &str, id: Uuid) -> common::TestResponse {
    common::TestRequest::delete(&format!("{ROOT}/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// The `phrase` of every element of `rows`, in order.
pub(super) fn phrases(rows: &[Value]) -> Vec<&str> {
    rows.iter()
        .map(|row| {
            row["phrase"]
                .as_str()
                .expect("every filter carries its phrase")
        })
        .collect()
}

/// The keys of a JSON object, sorted, for exact key-set assertions.
pub(super) fn json_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("filters render objects")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

// ---------------------------------------------------------------------------
// Filter application: the v1 semantics of `services::filter` over HTTP —
// the home, public and account timelines, the notification listing and the
// status' thread context.
// ---------------------------------------------------------------------------

/// The instance origin the fixtures post under.
pub(super) const BASE: &str = "https://test.example";
/// The home timeline route.
pub(super) const HOME: &str = "/api/v1/timelines/home";
/// The public timeline route.
pub(super) const PUBLIC: &str = "/api/v1/timelines/public";
/// The notification listing route.
pub(super) const NOTES: &str = "/api/v1/notifications";

/// `GET`s `path` with `token`.
pub(super) async fn get_as(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// `GET`s `path` with `token`, asserted 200, parsed.
pub(super) async fn page_of(app: &Router, token: &str, path: &str) -> Value {
    let response = get_as(app, token, path).await;
    assert_eq!(response.status, StatusCode::OK, "`{path}` answers 200");
    response.json_value()
}

/// Creates `owner`'s warning filter (whole-word, kept-but-flagged) for
/// `phrase` in `contexts`, and returns the stored row.
pub(super) async fn warner(
    pool: &PgPool,
    owner: &User,
    phrase: &str,
    contexts: &[&str],
) -> filter::Filter {
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
pub(super) async fn hider(
    pool: &PgPool,
    owner: &User,
    phrase: &str,
    contexts: &[&str],
) -> filter::Filter {
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
pub(super) async fn looser(
    pool: &PgPool,
    owner: &User,
    phrase: &str,
    contexts: &[&str],
) -> filter::Filter {
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
pub(super) async fn make_filter(pool: &PgPool, owner: &User, body: &NewFilter) -> filter::Filter {
    filter::create(pool, owner.id, body)
        .await
        .expect("fixture filter creates")
}

/// The expected `filtered` entry for a match of `f`: the ticket's v2
/// `FilterResult` shape, as the wire carries it.
pub(super) fn expected(f: &filter::Filter, context: &str, whole_word: bool) -> Value {
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
pub(super) fn ids(body: &Value) -> Vec<String> {
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
pub(super) fn filtered_of(body: &Value, id: &str) -> Value {
    let entries = body.as_array().expect("a listing is a JSON array");
    entries
        .iter()
        .find(|status| status["id"] == json!(id))
        .unwrap_or_else(|| panic!("`{id}` is on the page"))["filtered"]
        .clone()
}

/// The ids of `posts`, newest first — the order the timelines answer in
/// (decision 4: `UUIDv7` text sorts by creation).
pub(super) fn newest_first(posts: &[&Status]) -> Vec<String> {
    let mut ordered: Vec<Uuid> = posts.iter().map(|post| post.id).collect();
    ordered.sort_by(|a, b| b.cmp(a));
    ordered.into_iter().map(|id| id.to_string()).collect()
}

/// The id of `post`, as its JSON carries it.
pub(super) fn id_of(post: &Status) -> String {
    post.id.to_string()
}

/// Inserts `author`'s public reply to `parent`.
pub(super) async fn reply(pool: &PgPool, author: &User, parent: &Status, text: &str) -> Status {
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
