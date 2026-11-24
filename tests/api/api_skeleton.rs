//! Tests for the API router skeleton (`src/api.rs`) and its JSON fallback:
//! unmatched paths under `/api` answer Pleroma's `api_not_implemented`
//! (404 `{"error":"Not implemented"}`), every other unmatched path answers
//! 404 `{"error":"Not found"}`, both of those carry the `x-wired-fallback`
//! marker the route inventory (`tests/api/route_inventory.rs`) reads, and
//! `/health` still serves 204 through the composed router.

#![cfg(test)]

use axum::http::StatusCode;

use crate::common;

/// `/api/v1/nothing` is unmatched under the empty `/api/v1` router: 404 with
/// Pleroma's `api_not_implemented` message.
#[tokio::test]
async fn unmatched_api_path_reports_not_implemented() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/api/v1/nothing").send(&app).await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "unmatched API path status"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "errors answer with JSON"
    );
    assert_eq!(
        response.error_field().as_deref(),
        Some("Not implemented"),
        "unmatched API paths use api_not_implemented"
    );
}

/// The same applies under `/api/v2` and to any other path below `/api/`,
/// including the bare `/api` prefix.
#[tokio::test]
async fn other_api_paths_also_report_not_implemented() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    for path in ["/api/v2/nothing", "/api/v1/pleroma/nothing", "/api"] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "unmatched API path status for {path}"
        );
        assert_eq!(
            response.error_field().as_deref(),
            Some("Not implemented"),
            "api_not_implemented message for {path}"
        );
    }
}

/// Any unmatched path outside `/api` answers 404 "Not found", the wording
/// of Pleroma's router-level fallback.
#[tokio::test]
async fn unmatched_non_api_path_reports_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/nothing").send(&app).await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "unmatched non-API path status"
    );
    assert_eq!(
        response.error_field().as_deref(),
        Some("Not found"),
        "non-API paths get the plain router-level 404"
    );
}

/// A path shaped like an API prefix but not below it is not `api`:
/// `/apibot` gets the plain "Not found" 404.
#[tokio::test]
async fn api_lookalike_prefix_reports_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/apibot").send(&app).await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "status for a non-API lookalike path"
    );
    assert_eq!(
        response.error_field().as_deref(),
        Some("Not found"),
        "/apibot is not under /api/"
    );
}

/// `/health` still answers 204 through the composed router, and the JSON
/// fallbacks are error objects, never the health route.
#[tokio::test]
async fn health_still_serves_no_content() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/health").send(&app).await;
    assert_eq!(response.status, StatusCode::NO_CONTENT, "health status");
    assert!(response.body.is_empty(), "health body is empty");
}

/// A wrong verb on `/health`, a known path, is still axum's default 405,
/// not the JSON fallback.
#[tokio::test]
async fn known_path_wrong_verb_is_method_not_allowed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::post("/health").send(&app).await;
    assert_eq!(
        response.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "405 on a known path, not the fallback 404"
    );
    assert_eq!(response.text(), "", "axum's default 405 body");
    assert_eq!(
        response.header("content-type"),
        None,
        "axum's default 405 carries no JSON body"
    );
}

/// Unmatched paths also answer with the fallback for client verbs like
/// `POST`, since no route restricts the method there.
#[tokio::test]
async fn fallback_covers_all_verbs() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::post("/api/v1/nothing")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "POST to an unmatched API path"
    );
    assert_eq!(
        response.error_field().as_deref(),
        Some("Not implemented"),
        "POST keeps the api_not_implemented body"
    );
    let put_response = common::TestRequest::put("/nothing").send(&app).await;
    assert_eq!(
        put_response.status,
        StatusCode::NOT_FOUND,
        "PUT to an unmatched non-API path"
    );
    assert_eq!(
        put_response.error_field().as_deref(),
        Some("Not found"),
        "PUT keeps the Not found body"
    );
}

/// `GET /api/openapi` is not routed (Pleroma serves no such document): the
/// `/api` fallback answers 404 "Not implemented" and marks it.
#[tokio::test]
async fn openapi_document_is_not_routed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/api/openapi").send(&app).await;
    assert_eq!(response.status, StatusCode::NOT_FOUND, "no such route");
    assert_eq!(
        response.error_field().as_deref(),
        Some("Not implemented"),
        "the api_not_implemented body"
    );
    assert_eq!(
        response.header("x-wired-fallback"),
        Some("1"),
        "the fallback marks its answer"
    );
}

/// Both fallback branches mark their answer with `x-wired-fallback: 1`, and a
/// route the router matched does not carry it: that header is what
/// `route_inventory.rs` reads to tell "no handler is routed here" from a
/// handler's own 404.
#[tokio::test]
async fn fallback_answers_carry_the_marker_header() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    for path in ["/api/v1/nothing", "/nothing"] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(
            response.header("x-wired-fallback"),
            Some("1"),
            "the fallback marks its own 404 for {path}"
        );
    }
    let health = common::TestRequest::get("/health").send(&app).await;
    assert_eq!(
        health.header("x-wired-fallback"),
        None,
        "a matched route carries no fallback marker"
    );
}
