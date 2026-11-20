//! The webhook maintenance API (WBE-209), CRUD half: the create minting
//! the id and the 64-hex signing secret and answering the six-key entity,
//! the show and the bare-array listing, the present-only `PATCH` and the
//! form-encoded create, and the delete's 204 with the gone id's
//! `{"error": "Not found"}` after it. The route const and the helpers the
//! validation and the internal-and-gates submodules share ride here as
//! `pub(super)`.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::support::{NOT_FOUND, admin};
use crate::common::{self, TestRequest};

/// The webhook listing, creation, update and action routes all live under
/// this prefix.
pub(super) const WEBHOOKS: &str = "/api/v1/pleroma/admin/webhooks";

/// An admin holding both webhook scopes, as its bearer token.
pub(super) async fn webhook_admin(pool: &PgPool, nickname: &str) -> String {
    admin(pool, nickname, "admin:read:webhooks admin:write:webhooks")
        .await
        .1
}

/// `POST`s `body` to a webhook route as `token`, over JSON.
pub(super) async fn post_webhook(
    app: &Router,
    token: &str,
    path: &str,
    body: Value,
) -> common::TestResponse {
    TestRequest::post(path)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// `GET`s `path` as `token`.
pub(super) async fn get_webhook(app: &Router, token: &str, path: &str) -> common::TestResponse {
    TestRequest::get(path).bearer(token).send(app).await
}

/// `PATCH`es `body` to `/webhooks/{id}` as `token`.
pub(super) async fn patch_webhook(
    app: &Router,
    token: &str,
    id: &str,
    body: Value,
) -> common::TestResponse {
    TestRequest::patch(&format!("{WEBHOOKS}/{id}"))
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// `DELETE`s `/webhooks/{id}` as `token`.
pub(super) async fn delete_webhook(app: &Router, token: &str, id: &str) -> common::TestResponse {
    TestRequest::delete(&format!("{WEBHOOKS}/{id}"))
        .bearer(token)
        .send(app)
        .await
}

/// `POST`s an empty body to `/webhooks/{id}{suffix}` as `token`: the
/// action routes carry no parameters.
pub(super) async fn act_on_webhook(
    app: &Router,
    token: &str,
    id: &str,
    suffix: &str,
) -> common::TestResponse {
    post_webhook(app, token, &format!("{WEBHOOKS}/{id}{suffix}"), json!({})).await
}

/// Creates a webhook through the API, asserting the 200, and returns its
/// entity with the id text.
pub(super) async fn create_webhook(app: &Router, token: &str, body: Value) -> (Value, String) {
    let created = post_webhook(app, token, WEBHOOKS, body).await;
    common::assert_answer(&created, StatusCode::OK, "the create");
    let entity = created.json_value();
    let id = entity["id"].as_str().expect("the id is text").to_owned();
    (entity, id)
}

/// The `webhooks` row behind `id` as `(url, events, secret, enabled,
/// internal)`, `None` when the table no longer carries it.
pub(super) async fn stored_webhook(
    pool: &PgPool,
    id: Uuid,
) -> Option<(String, Vec<String>, String, bool, bool)> {
    sqlx::query_as::<_, (String, Vec<String>, String, bool, bool)>(
        "SELECT url, events, secret, enabled, internal FROM webhooks WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .expect("the webhook row reads back")
}

/// Exactly the ticket's six entity keys — no `created_at`, no `internal`
/// surprises.
pub(super) fn assert_webhook_keys(entity: &Value) {
    assert_eq!(
        entity.as_object().map(serde_json::Map::len),
        Some(6),
        "exactly the six keys the ticket pins: {entity}"
    );
}

/// The minted signing secret: 32 random bytes as lowercase hex.
pub(super) fn assert_secret(entity: &Value) {
    let secret = entity["secret"].as_str().expect("the secret is text");
    assert_eq!(secret.len(), 64, "32 bytes are 64 hex characters");
    assert!(
        secret
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "the secret is lowercase hex: {secret}"
    );
}

/// The CRUD round trip: a create mints the id and secret and answers the
/// six-key entity; the show and the bare-array listing answer the same
/// entity; the delete 204s, and the gone id is the 404
/// `{"error":"Not found"}` on the show and the delete alike.
#[tokio::test]
async fn webhook_crud_roundtrip() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = webhook_admin(&pool, "hookcruder").await;

    let (entity, id) = create_webhook(
        &app,
        &token,
        json!({"url": "https://example.org/hook",
               "events": ["report.created", "account.created"]}),
    )
    .await;
    assert_webhook_keys(&entity);
    assert_secret(&entity);
    assert!(Uuid::parse_str(&id).is_ok(), "the id is uuid text: {id}");
    assert_eq!(entity["url"], json!("https://example.org/hook"), "the url");
    let events = json!(["report.created", "account.created"]);
    assert_eq!(entity["events"], events, "and the events");
    assert_eq!(
        entity["enabled"],
        json!(true),
        "the gate defaults to enabled"
    );
    assert_eq!(
        entity["internal"],
        json!(false),
        "a created webhook is external"
    );

    let shown = get_webhook(&app, &token, &format!("{WEBHOOKS}/{id}")).await;
    common::assert_answer(&shown, StatusCode::OK, "the show");
    assert_eq!(
        shown.json_value(),
        entity,
        "the show answers the stored entity"
    );

    let listed = get_webhook(&app, &token, WEBHOOKS).await;
    common::assert_answer(&listed, StatusCode::OK, "the index");
    assert_eq!(listed.json_value(), json!([entity]), "Pleroma's bare array");

    let deleted = delete_webhook(&app, &token, &id).await;
    assert_eq!(
        deleted.status,
        StatusCode::NO_CONTENT,
        "the delete is a 204"
    );
    let row_id = Uuid::parse_str(&id).expect("the test id parses");
    assert!(
        stored_webhook(&pool, row_id).await.is_none(),
        "the row is gone"
    );

    let gone = get_webhook(&app, &token, &format!("{WEBHOOKS}/{id}")).await;
    let again = delete_webhook(&app, &token, &id).await;
    common::assert_all_refused(
        &[gone, again],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the deleted webhook, on the show and the delete alike",
    );
}

/// A present-only `PATCH` moves exactly the fields it carries and keeps
/// the rest of the row — the secret included; a form-encoded create passes
/// the same validation and reads its stringly-typed gate and array key.
#[tokio::test]
async fn webhook_patch_moves_only_the_present_fields() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = webhook_admin(&pool, "hookpatcher").await;

    let (entity, id) = create_webhook(
        &app,
        &token,
        json!({"url": "https://example.org/hook",
               "events": ["report.created", "account.created"]}),
    )
    .await;

    let gated = patch_webhook(&app, &token, &id, json!({"enabled": false})).await;
    common::assert_answer(&gated, StatusCode::OK, "the gate patch");
    let gated_entity = gated.json_value();
    assert_eq!(gated_entity["enabled"], json!(false), "the gate moved");
    assert_eq!(gated_entity["url"], entity["url"], "an absent url is kept");
    assert_eq!(
        gated_entity["events"], entity["events"],
        "an absent events list is kept"
    );
    assert_eq!(
        gated_entity["secret"], entity["secret"],
        "an update never turns the secret"
    );

    let moved = patch_webhook(
        &app,
        &token,
        &id,
        json!({"url": "http://example.org/moved", "events": ["report.updated"]}),
    )
    .await;
    common::assert_answer(&moved, StatusCode::OK, "the move patch");
    let moved_entity = moved.json_value();
    let moved_url = json!("http://example.org/moved");
    assert_eq!(
        moved_entity["url"], moved_url,
        "plain http is a delivery target too"
    );
    let one_event = json!(["report.updated"]);
    assert_eq!(moved_entity["events"], one_event, "the events moved");
    assert_eq!(
        moved_entity["enabled"],
        json!(false),
        "the gate stands at disabled"
    );
    let row_id = Uuid::parse_str(&id).expect("the test id parses");
    let stored = stored_webhook(&pool, row_id).await.expect("the row stands");
    assert_eq!(
        stored.0, "http://example.org/moved",
        "the row matches the answer"
    );
    assert_eq!(
        stored.1,
        vec!["report.updated".to_owned()],
        "url and events as answered"
    );
    assert!(!stored.3, "and the gate");

    let formed = TestRequest::post(WEBHOOKS)
        .bearer(&token)
        .form(&[
            ("url", "https://example.org/form-hook"),
            ("events[]", "report.created"),
            ("enabled", "false"),
        ])
        .send(&app)
        .await;
    common::assert_answer(&formed, StatusCode::OK, "the form create");
    let formed_entity = formed.json_value();
    assert_webhook_keys(&formed_entity);
    let form_events = json!(["report.created"]);
    assert_eq!(
        formed_entity["events"], form_events,
        "the form array key reaches the row"
    );
    assert_eq!(
        formed_entity["enabled"],
        json!(false),
        "and the stringly-typed gate"
    );
}
