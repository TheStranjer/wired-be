//! The legacy `/api/pleroma/*` reroute (WBE-894): a request under
//! `/api/pleroma/` that no route matches is handled as the same request to
//! `/api/v1/pleroma/*`, as Pleroma's `LegacyPleromaApiRerouterPlug` does.
//!
//! Routes WiredBE defines under `/api/pleroma/` keep precedence, the rest of
//! the path space keeps the usual JSON 404, and the rewritten request goes
//! through the normal router, so auth applies exactly as on a direct call.

#![cfg(test)]

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::status::Visibility;

use crate::common;

/// The exact body of the app-wide `/api` fallback 404.
const NOT_IMPLEMENTED_BODY: &str = r#"{"error":"Not implemented"}"#;

/// One unicode emoji, percent-encoded as a client sends it in a path.
const THUMB_SEGMENT: &str = "%F0%9F%91%8D";

/// An aliased read answers exactly what the v1 path answers.
#[tokio::test]
async fn an_aliased_get_answers_like_the_v1_path() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());
    let direct = common::TestRequest::get("/api/v1/pleroma/captcha")
        .send(&app)
        .await;
    let aliased = common::TestRequest::get("/api/pleroma/captcha")
        .send(&app)
        .await;
    assert_eq!(direct.status, StatusCode::OK, "the v1 path answers");
    assert_eq!(aliased.status, direct.status, "same status");
    assert_eq!(aliased.text(), direct.text(), "same body");
    assert_eq!(
        aliased.header("x-wired-fallback"),
        None,
        "a handler answered, not the fallback"
    );
}

/// An authenticated write through the alias reaches the v1 handler and acts.
#[tokio::test]
async fn an_aliased_write_reaches_the_v1_handler() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let reader = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &reader, "read:statuses write:statuses").await;
    let author = common::create_local_user(&pool, "bob").await;
    let status = common::create_status(&pool, &author, "a post", Visibility::Public).await;

    let response = common::TestRequest::put(&format!(
        "/api/pleroma/statuses/{}/reactions/{THUMB_SEGMENT}",
        status.id
    ))
    .bearer(&token)
    .send(&app)
    .await;
    common::assert_answer(&response, StatusCode::OK, "the aliased reaction");

    let listed =
        common::TestRequest::get(&format!("/api/v1/pleroma/statuses/{}/reactions", status.id))
            .bearer(&token)
            .send(&app)
            .await;
    common::assert_answer(&listed, StatusCode::OK, "the v1 reaction list");
    let entries = listed.json_value();
    assert_eq!(entries[0]["name"], json!("👍"), "the reaction was stored");
    assert_eq!(entries[0]["count"], json!(1), "exactly once");
    assert_eq!(entries[0]["me"], json!(true), "by the token's user");
}

/// Without a token the alias gets the refusal the v1 path gives.
#[tokio::test]
async fn an_aliased_write_without_a_token_is_refused_like_v1() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let author = common::create_local_user(&pool, "bob").await;
    let status = common::create_status(&pool, &author, "a post", Visibility::Public).await;
    let tail = format!("/statuses/{}/reactions/{THUMB_SEGMENT}", status.id);

    let direct = common::TestRequest::put(&format!("/api/v1/pleroma{tail}"))
        .send(&app)
        .await;
    let aliased = common::TestRequest::put(&format!("/api/pleroma{tail}"))
        .send(&app)
        .await;
    assert_eq!(direct.status, StatusCode::UNAUTHORIZED, "v1 refuses");
    assert_eq!(aliased.status, direct.status, "same refusal status");
    assert_eq!(aliased.text(), direct.text(), "same refusal body");
}

/// The query string travels with the rewritten request.
#[tokio::test]
async fn an_aliased_get_keeps_its_query_string() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());
    let without = common::TestRequest::get("/api/pleroma/emoji/packs/archive")
        .send(&app)
        .await;
    assert_eq!(without.status, StatusCode::BAD_REQUEST, "name is required");

    let direct = common::TestRequest::get("/api/v1/pleroma/emoji/packs/archive?name=nope")
        .send(&app)
        .await;
    let aliased = common::TestRequest::get("/api/pleroma/emoji/packs/archive?name=nope")
        .send(&app)
        .await;
    assert_ne!(aliased.status, without.status, "the parameter arrived");
    assert_eq!(aliased.status, direct.status, "same status as v1");
    assert_eq!(aliased.text(), direct.text(), "same body as v1");
}

/// A route `WiredBE` defines under `/api/pleroma/` still answers itself.
#[tokio::test]
async fn an_explicit_pleroma_route_keeps_precedence() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());
    let response = common::TestRequest::get("/api/pleroma/frontend_configurations")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "status");
    assert_eq!(response.text(), "{}", "its own handler's document");
}

/// A path that matches nothing even after the rewrite is the usual JSON 404.
#[tokio::test]
async fn an_unknown_pleroma_path_is_the_usual_json_404() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());
    let response = common::TestRequest::get("/api/pleroma/does-not-exist")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND, "status");
    assert_eq!(response.text(), NOT_IMPLEMENTED_BODY, "body");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "JSON"
    );
    assert_eq!(response.header("x-wired-fallback"), Some("1"), "marker");
}

/// Only the exact `/api/pleroma/` prefix is rewritten.
#[tokio::test]
async fn other_prefixes_are_not_rewritten() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());
    for path in [
        "/api/pleromax/captcha",
        "/api/pleroma",
        "/x/api/pleroma/captcha",
    ] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path} status");
        assert_eq!(
            response.header("x-wired-fallback"),
            Some("1"),
            "{path} is the fallback's own answer"
        );
    }
}
