//! The admin gates of the install routes: the `admin:read` GETs and the
//! `admin:write` POSTs of WBE-216 refuse weaker credentials and wrong
//! scopes.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::{
    download_zip_import::upload_pack,
    remote_mocks::{COULD_NOT_FETCH, UNREADABLE},
    support::{Refusal, admin_token, plain_token},
};
use crate::common;

/// The install routes' two GETs, with the query the remote one demands.
const INSTALL_GETS: [&str; 2] = [
    "/api/v1/pleroma/emoji/packs/remote?url=https://remote.invalid",
    "/api/v1/pleroma/emoji/packs/import",
];

/// The install routes' two POSTs and a body each; the auth extractor
/// rejects them before any of these bodies is parsed.
fn install_posts() -> [(&'static str, Value); 2] {
    [
        (
            "/api/v1/pleroma/emoji/packs/download",
            json!({"url": "https://remote.invalid", "name": "ghost"}),
        ),
        (
            "/api/v1/pleroma/emoji/packs/download_zip",
            json!({"name": "ghost"}),
        ),
    ]
}

/// The install routes answer the shared admin gate: no bearer, a bogus
/// token, an app-only token and a non-admin are refused alike on all
/// four, in the words the gate itself words them.
#[tokio::test]
async fn install_routes_refuse_weaker_credentials() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let app_only = common::create_app_only_token(&pool, "admin:read admin:write").await;
    let plain = plain_token(&pool, "installpleb", "admin:read admin:write").await;

    let refusals: &[Refusal<'_>] = &[
        (
            "no bearer token",
            None,
            StatusCode::UNAUTHORIZED,
            "Invalid credentials",
        ),
        (
            "a bogus token",
            Some("not-a-real-token"),
            StatusCode::UNAUTHORIZED,
            "The access token is invalid",
        ),
        (
            "an app-only token",
            Some(&app_only),
            StatusCode::FORBIDDEN,
            "This API requires an authenticated user",
        ),
        (
            "a non-admin user",
            Some(&plain),
            StatusCode::FORBIDDEN,
            "User is not an admin.",
        ),
    ];
    for path in &INSTALL_GETS {
        for (who, token, status, message) in refusals {
            let request = common::TestRequest::get(path);
            let response = match token {
                Some(bearer) => request.bearer(bearer).send(&app).await,
                None => request.send(&app).await,
            };
            common::assert_answer(&response, *status, &format!("get {path} as {who}"));
            common::assert_error(&response, message, &format!("get {path} as {who}"));
        }
    }
    for (path, body) in &install_posts() {
        for (who, token, status, message) in refusals {
            let request = common::TestRequest::post(path).json(body);
            let response = match token {
                Some(bearer) => request.bearer(bearer).send(&app).await,
                None => request.send(&app).await,
            };
            common::assert_answer(&response, *status, &format!("post {path} as {who}"));
            common::assert_error(&response, message, &format!("post {path} as {who}"));
        }
    }
}

/// The class the ticket pins per route: `admin:write` never grants the
/// GETs and `admin:read` never grants the POSTs, each refusal naming the
/// root it lacks; the narrow `admin:read:emoji` and `admin:write:emoji`
/// aliases pass the gate and reach the endpoints' own work — an answer
/// from past the gate on every one of them.
#[tokio::test]
async fn install_routes_enforce_their_admin_class() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let app = common::test_app_with_emoji_dir(pool.clone(), dir.path());
    let reader = admin_token(&pool, "installreader", "admin:read").await;
    let writer = admin_token(&pool, "installwriter", "admin:write").await;

    for path in &INSTALL_GETS {
        let refused = common::TestRequest::get(path)
            .bearer(&writer)
            .send(&app)
            .await;
        common::assert_answer(&refused, StatusCode::FORBIDDEN, "a writer on a reader");
        common::assert_error(
            &refused,
            "Insufficient permissions: admin:read.",
            "a writer on a reader",
        );
    }
    for (path, body) in &install_posts() {
        let refused = common::TestRequest::post(path)
            .bearer(&reader)
            .json(body)
            .send(&app)
            .await;
        common::assert_answer(&refused, StatusCode::FORBIDDEN, "a reader on a writer");
        common::assert_error(
            &refused,
            "Insufficient permissions: admin:write.",
            "a reader on a writer",
        );
    }

    let narrow_read = admin_token(&pool, "installnarrowr", "admin:read:emoji").await;
    let imported = common::TestRequest::get("/api/v1/pleroma/emoji/packs/import")
        .bearer(&narrow_read)
        .send(&app)
        .await;
    common::assert_answer(&imported, StatusCode::OK, "the narrow read alias imports");
    assert_eq!(
        imported.json_value(),
        json!([]),
        "the empty directory imports nothing"
    );
    let relayed =
        common::TestRequest::get("/api/v1/pleroma/emoji/packs/remote?url=https://remote.invalid")
            .bearer(&narrow_read)
            .send(&app)
            .await;
    common::assert_answer(
        &relayed,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the narrow read relay",
    );
    common::assert_error(&relayed, COULD_NOT_FETCH, "the narrow read relay");

    let narrow_write = admin_token(&pool, "installnarroww", "admin:write:emoji").await;
    let pulled = common::TestRequest::post("/api/v1/pleroma/emoji/packs/download")
        .bearer(&narrow_write)
        .json(&json!({"url": "https://remote.invalid", "name": "ghost"}))
        .send(&app)
        .await;
    common::assert_answer(
        &pulled,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the narrow write download",
    );
    let uploaded = upload_pack(&app, &narrow_write, "narrowzip", b"not a zip".to_vec()).await;
    common::assert_answer(
        &uploaded,
        StatusCode::BAD_REQUEST,
        "the narrow write upload",
    );
    common::assert_error(&uploaded, UNREADABLE, "the narrow write upload");

    assert!(
        !dir.path().join("narrowzip").exists() && !dir.path().join("ghost").exists(),
        "the post-gate failures left no packs behind"
    );
}
