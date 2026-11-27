//! The credential and scope gates of every notification route: no
//! credential answers 401, an app-only token 403, the write routes demand
//! `write:notifications` and the reads `read:notifications`.

use axum::{Router, http::StatusCode};
use serde_json::json;
use uuid::Uuid;

use super::support::{READ, READ_ROUTE, READ_ROUTE_V1, V1, WRITE, assert_empty_object, fixture};
use crate::common;

/// No credential answers 401 "Invalid credentials" on every route here,
/// and an app-only token — a live credential that names no user — answers
/// 403 "This API requires an authenticated user".
#[tokio::test]
async fn every_route_demands_a_user_credential() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let note = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let app = common::test_app(pool);

    let anonymous = call_every_route(&app, "", note.id).await;
    common::assert_all_refused(
        &anonymous,
        StatusCode::UNAUTHORIZED,
        "Invalid credentials",
        "an unauthenticated flat notification request",
    );

    let app_only_refusals = call_every_route(&app, &app_only, note.id).await;
    common::assert_all_refused(
        &app_only_refusals,
        StatusCode::FORBIDDEN,
        "This API requires an authenticated user",
        "an app-only credential on the flat notification API",
    );
}

/// One of the routes under test: verb, path, and whether the request
/// carries an `{"id": …}` body — the handlers that read an id get one, so
/// a missing parameter never outruns the credential check.
#[derive(Clone, Debug)]
struct GatedRoute {
    /// The verb, `"GET"`, `"POST"` or `"DELETE"`.
    verb: &'static str,
    /// The request target, query string included.
    path: String,
    /// Whether to send an `id` body with the request.
    with_id_body: bool,
}

/// The five write routes of this file, all mounted on `id` where they
/// name one.
fn gated_routes(id: Uuid) -> Vec<GatedRoute> {
    vec![
        GatedRoute {
            verb: "POST",
            path: format!("{V1}/clear"),
            with_id_body: false,
        },
        GatedRoute {
            verb: "POST",
            path: format!("{V1}/{id}/dismiss"),
            with_id_body: true,
        },
        GatedRoute {
            verb: "POST",
            path: format!("{V1}/dismiss"),
            with_id_body: true,
        },
        GatedRoute {
            verb: "DELETE",
            path: format!("{V1}/destroy_multiple?ids[]={id}"),
            with_id_body: false,
        },
        GatedRoute {
            verb: "POST",
            path: READ_ROUTE.to_owned(),
            with_id_body: true,
        },
        GatedRoute {
            verb: "POST",
            path: READ_ROUTE_V1.to_owned(),
            with_id_body: true,
        },
    ]
}

/// The two read routes, both mounted on `id` where they name one.
fn read_routes(id: Uuid) -> Vec<GatedRoute> {
    vec![
        GatedRoute {
            verb: "GET",
            path: V1.to_owned(),
            with_id_body: false,
        },
        GatedRoute {
            verb: "GET",
            path: format!("{V1}/{id}"),
            with_id_body: false,
        },
    ]
}

/// Send `route` with the bearer `token`; empty `token` sends no
/// credential.
async fn call(app: &Router, route: &GatedRoute, token: &str, id: Uuid) -> common::TestResponse {
    let mut request = match route.verb {
        "GET" => common::TestRequest::get(&route.path),
        "DELETE" => common::TestRequest::delete(&route.path),
        _ => common::TestRequest::post(&route.path),
    };
    if !token.is_empty() {
        request = request.bearer(token);
    }
    if route.with_id_body {
        request = request.json(&json!({"id": id}));
    }
    request.send(app).await
}

/// Call every route of the flat API — reads and writes — with `token`
/// (empty: unauthenticated).
async fn call_every_route(app: &Router, token: &str, id: Uuid) -> Vec<common::TestResponse> {
    let mut routes = read_routes(id);
    routes.extend(gated_routes(id));
    let mut responses = Vec::new();
    for route in routes {
        responses.push(call(app, &route, token, id).await);
    }
    responses
}

/// Every write route demands `write:notifications`: a
/// `read:notifications` token is refused with Pleroma's wording, and the
/// root `write` grant passes the hierarchy.
#[tokio::test]
async fn every_route_demands_write_notifications() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let note = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let narrow = common::create_app_and_token(&pool, &alice, READ).await;
    let write_root = common::create_app_and_token(&pool, &alice, "write").await;
    let app = common::test_app(pool);

    let mut refused = Vec::new();
    for route in gated_routes(note.id) {
        refused.push(call(&app, &route, &narrow, note.id).await);
    }
    common::assert_all_refused(
        &refused,
        StatusCode::FORBIDDEN,
        "Insufficient permissions: write:notifications.",
        "a read:notifications token on the flat notification routes",
    );

    let granted = common::TestRequest::post(&format!("{V1}/clear"))
        .bearer(&write_root)
        .send(&app)
        .await;
    assert_empty_object(&granted, "`write` covers `write:notifications`");
}

/// The two reads demand `read:notifications`: a `write:notifications`
/// token is refused with Pleroma's wording, and the root `read` grant
/// passes the hierarchy.
#[tokio::test]
async fn the_reads_demand_read_notifications() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let note = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let narrow = common::create_app_and_token(&pool, &alice, WRITE).await;
    let read_root = common::create_app_and_token(&pool, &alice, "read").await;
    let app = common::test_app(pool);

    let mut refused = Vec::new();
    for route in read_routes(note.id) {
        refused.push(call(&app, &route, &narrow, note.id).await);
    }
    common::assert_all_refused(
        &refused,
        StatusCode::FORBIDDEN,
        "Insufficient permissions: read:notifications.",
        "a write:notifications token on the flat notification reads",
    );

    for route in read_routes(note.id) {
        let granted = call(&app, &route, &read_root, note.id).await;
        common::assert_answer(
            &granted,
            StatusCode::OK,
            &format!("`read` covers the scope on {}", route.path),
        );
    }
}
