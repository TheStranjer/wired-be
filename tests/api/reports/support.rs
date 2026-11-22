//! The fixtures and request helpers every report test shares.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    federation::publisher,
    models::{
        instance_rule,
        report::{self},
        status::Visibility,
        user::{Flag, NewUser, User},
    },
    pagination::{MAX_LIMIT, PageParams},
};

use crate::common;

/// The route every test posts to.
pub(super) const ROOT: &str = "/api/v1/reports";

/// Every JSON key a report entity must carry, sorted.
pub(super) const EXPECTED_KEYS: [&str; 10] = [
    "action_taken",
    "action_taken_at",
    "category",
    "comment",
    "created_at",
    "forwarded",
    "id",
    "rule_ids",
    "status_ids",
    "target_account",
];

/// The 422 wording of the cited-status check.
pub(super) const BAD_STATUSES: &str = "Invalid status_ids";
/// The 422 wording of the cited-rule check.
pub(super) const BAD_RULES: &str = "Invalid rule_ids";

/// A local user named `nickname` plus a bearer token granting `scopes`.
pub(super) async fn fixture(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` with a `write:reports` token: the shape
/// almost every filing in this file is made by.
pub(super) async fn reporter(pool: &PgPool, nickname: &str) -> (User, String) {
    fixture(pool, nickname, "write:reports").await
}

/// `POST /api/v1/reports` with the JSON `body` as `token`.
pub(super) async fn file(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(ROOT)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// A filing, asserted 200, answered as JSON.
pub(super) async fn filed(app: &Router, token: &str, body: Value) -> Value {
    let response = file(app, token, body).await;
    assert_eq!(response.status, StatusCode::OK, "the report files");
    response.json_value()
}

/// The minimal valid body: report the account, say nothing.
pub(super) fn about(target: &User) -> Value {
    json!({"account_id": target.id.to_string()})
}

/// The keys of a JSON object, sorted, for exact key-set assertions.
pub(super) fn json_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("reports render objects")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// The `id` the entity answered with, as the row's own `UUIDv7` text.
pub(super) fn entity_id(value: &Value) -> Uuid {
    let text = value["id"].as_str().expect("the entity carries its own id");
    Uuid::parse_str(text).expect("the entity id is an id")
}

/// The stored report the entity answered with, which must exist.
pub(super) async fn stored(pool: &PgPool, value: &Value) -> report::Report {
    report::get(pool, entity_id(value))
        .await
        .unwrap()
        .expect("the filed report is stored")
}

/// Every report in the database, newest filing first: what a refusal never
/// adds to and a filing always does.
pub(super) async fn filings(pool: &PgPool) -> Vec<report::Report> {
    let page = PageParams::from_query(Some(&format!("limit={MAX_LIMIT}")), MAX_LIMIT)
        .expect("the fixture page parses");
    report::list(pool, page).await.expect("the listing reads")
}

/// A live post by `author` of `visibility`, for the citations.
pub(super) async fn post(pool: &PgPool, author: &User, text: &str, visibility: Visibility) -> Uuid {
    common::create_status(pool, author, text, visibility)
        .await
        .id
}

/// A rule of this instance named `text`, for the `rule_ids` citations.
pub(super) async fn rule(pool: &PgPool, text: &str) -> Uuid {
    instance_rule::create(pool, text, "", 1)
        .await
        .expect("the rule fixture creates")
        .id
}

/// A local user named `nickname` whose account is deactivated: the shape
/// [`AuthUser`] refuses with its 403.
pub(super) async fn dormant_user(pool: &PgPool, nickname: &str) -> User {
    wired_be::models::user::insert(
        pool,
        &NewUser {
            is_active: Flag::new(false),
            ..NewUser::local(nickname, &format!("https://test.example/users/{nickname}"))
        },
    )
    .await
    .expect("the dormant fixture inserts")
}

/// The args of every queued `federator.publish` job, in insertion order.
pub(super) async fn publish_jobs(pool: &PgPool) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>("SELECT args FROM jobs WHERE kind = $1 ORDER BY inserted_at, id")
        .bind(publisher::KIND_PUBLISH)
        .fetch_all(pool)
        .await
        .expect("the publish jobs read")
        .into_iter()
        .map(|(args,)| args)
        .collect()
}

// --- `POST /api/v1/accounts/{id}/flag` -----------------------------------------------------

/// `POST /api/v1/accounts/{id}/flag` for the target `id`.
pub(super) async fn flag(app: &Router, token: &str, id: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(&format!("/api/v1/accounts/{id}/flag"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}
