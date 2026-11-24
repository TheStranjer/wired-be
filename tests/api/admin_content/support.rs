//! The shared fixtures of the content tests: the route and error constants
//! and the helpers that seed rules and announcements through the API.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    models::user::{Flag, NewUser, User},
    state::AppState,
};

use crate::common;

/// The admin rule routes.
pub(super) const RULES: &str = "/api/v1/pleroma/admin/rules";
/// The admin announcement routes.
pub(super) const NOTICES: &str = "/api/v1/pleroma/admin/announcements";
/// The public rules listing the instance publishes.
pub(super) const PUBLIC_RULES: &str = "/api/v1/instance/rules";
/// The public instance document, whose `rules` block is the same list.
pub(super) const INSTANCE: &str = "/api/v1/instance";
/// The user-facing announcements listing.
pub(super) const USER_NOTICES: &str = "/api/v1/announcements";
/// The admin scopes both halves of every gate here accept, as roots.
pub(super) const ALL_ADMIN: &str = "admin:read admin:write";
/// Every JSON key the admin rule entity carries, sorted.
pub(super) const RULE_KEYS: [&str; 4] = ["hint", "id", "priority", "text"];
/// Every JSON key an announcement entity carries, sorted: the client API's
/// fourteen, `all_day` and the `pleroma` block included.
pub(super) const NOTICE_KEYS: [&str; 14] = [
    "all_day",
    "content",
    "emojis",
    "ends_at",
    "id",
    "mentions",
    "pleroma",
    "published_at",
    "reactions",
    "read",
    "starts_at",
    "statuses",
    "tags",
    "updated_at",
];
/// 400 wording a blank required field earns.
pub(super) const BLANK: &str = "can't be blank";
/// 400 wording a rule text past 500 characters earns.
pub(super) const TOO_LONG: &str = "should be at most 500 character(s)";
/// 400 wording a bound that is no RFC 3339 timestamp earns.
pub(super) const INVALID_TIME: &str = "is invalid";
/// 400 wording a window whose end does not follow its start earns.
pub(super) const ENDS_AFTER: &str = "must be after starts_at";
/// A window bound far enough in the past to be open already.
pub(super) const PAST: &str = "2000-06-01T12:00:00.250Z";
/// A window bound far enough in the future to be closed only much later.
pub(super) const FUTURE: &str = "2100-06-01T12:00:00.250Z";

// --- fixtures ---------------------------------------------------------------------------

/// The router over the stock test config.
pub(super) fn app(pool: &PgPool) -> Router {
    common::test_app(pool.clone())
}

/// The router beside the state whose hub the stream events are read from.
pub(super) fn app_with_state(pool: &PgPool) -> (Router, AppState) {
    let state = common::test_app_state(pool.clone(), common::test_config());
    (wired_be::app(state.clone()), state)
}

/// A local admin named `nickname` with a bearer token granting `scopes`.
pub(super) async fn admin(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// The newest `moderation_log` row written for `action`: its sentence and
/// data map, insisting at least one stands.
pub(super) async fn log_entry(pool: &PgPool, action: &str) -> (String, Value) {
    let found = sqlx::query_as::<_, (String, Value)>(
        "SELECT message, data FROM moderation_log WHERE action = $1 \
         ORDER BY inserted_at DESC, id DESC LIMIT 1",
    )
    .bind(action)
    .fetch_optional(pool)
    .await
    .expect("the log reads");
    found.expect("at least one entry for the action")
}

/// How many `instance_rules` rows the database holds.
pub(super) async fn rule_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM instance_rules")
        .fetch_one(pool)
        .await
        .expect("the rules count")
}

/// How many `announcements` rows the database holds.
pub(super) async fn notice_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM announcements")
        .fetch_one(pool)
        .await
        .expect("the notices count")
}

// --- request helpers --------------------------------------------------------------------

/// `POST RULES` with `body` as `token`.
pub(super) async fn post_rule(app: &Router, token: &str, body: &Value) -> common::TestResponse {
    common::TestRequest::post(RULES)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// `POST NOTICES` with `body` as `token`.
pub(super) async fn post_notice(app: &Router, token: &str, body: &Value) -> common::TestResponse {
    common::TestRequest::post(NOTICES)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// `PATCH path` with `body` as `token`.
pub(super) async fn patch(
    app: &Router,
    token: &str,
    path: &str,
    body: &Value,
) -> common::TestResponse {
    common::TestRequest::patch(path)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// `GET path` as `token`.
pub(super) async fn get(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}

/// `DELETE path` as `token`.
pub(super) async fn destroy(app: &Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::delete(path)
        .bearer(token)
        .send(app)
        .await
}

/// `POST RULES` for `body`, insisting on 200 and answering the entity.
pub(super) async fn create_rule(app: &Router, token: &str, body: &Value) -> Value {
    let created = post_rule(app, token, body).await;
    common::assert_answer(&created, StatusCode::OK, "creating a rule");
    created.json_value()
}

/// `POST NOTICES` for `body`, insisting on 200 and answering the entity.
pub(super) async fn create_notice(app: &Router, token: &str, body: &Value) -> Value {
    let created = post_notice(app, token, body).await;
    common::assert_answer(&created, StatusCode::OK, "creating an announcement");
    created.json_value()
}

/// A rule reading `text` at `priority`, as the create answered it.
pub(super) async fn a_rule(app: &Router, token: &str, text: &str, priority: i64) -> Value {
    create_rule(app, token, &json!({"text": text, "priority": priority})).await
}

/// A notice reading `content`, as the create answered it.
pub(super) async fn a_notice(app: &Router, token: &str, content: &str) -> Value {
    create_notice(app, token, &json!({"content": content})).await
}

/// The `text`s of a returned rule array, in answer order.
pub(super) fn texts_of(listed: &Value) -> Vec<String> {
    listed
        .as_array()
        .expect("the listings are arrays")
        .iter()
        .map(|entry| {
            entry["text"]
                .as_str()
                .expect("every rule carries its text")
                .to_owned()
        })
        .collect()
}
