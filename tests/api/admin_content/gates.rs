//! The access gate of the content routes: credentials, the admin class and
//! the scopes each route demands.

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use wired_be::models::user::{Flag, NewUser};

use super::support::{
    ALL_ADMIN, NOTICES, RULES, a_notice, a_rule, admin, app, destroy, entity_id, get, ids_of,
    notice_count, patch, post_rule, rule_count,
};
use crate::common;

/// The tokens the gate tests hand the routes: the admin's roots, a
/// non-admin and a moderator carrying the same scopes, an app-only token,
/// an admin with only ordinary scopes, and the four narrow area scopes.
#[derive(Debug)]
struct Tokens {
    /// The admin's `admin:read admin:write`.
    all: String,
    /// A non-admin's same roots.
    plain: String,
    /// A moderator's same roots.
    staff: String,
    /// A user-unbound token with the same roots.
    app_only: String,
    /// An admin with `read write` and no admin grant.
    ordinary: String,
    /// An admin holding only `admin:read:rules`.
    read_rules: String,
    /// An admin holding only `admin:write:rules`.
    write_rules: String,
    /// An admin holding only `admin:read:announcements`.
    read_notices: String,
    /// An admin holding only `admin:write:announcements`.
    write_notices: String,
}

/// Mints the [`Tokens`] fixture.
async fn tokens(pool: &PgPool) -> Tokens {
    let (_root, all) = admin(pool, "root", ALL_ADMIN).await;
    let pleb = common::create_local_user(pool, "pleb").await;
    let mod_user = common::create_local_user_with(pool, "mod", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    Tokens {
        all,
        plain: common::create_app_and_token(pool, &pleb, "read write admin:read admin:write").await,
        staff: common::create_app_and_token(pool, &mod_user, ALL_ADMIN).await,
        app_only: common::create_app_only_token(pool, ALL_ADMIN).await,
        ordinary: admin(pool, "unscoped", "read write").await.1,
        read_rules: admin(pool, "reader", "admin:read:rules").await.1,
        write_rules: admin(pool, "mover", "admin:write:rules").await.1,
        read_notices: admin(pool, "watcher", "admin:read:announcements").await.1,
        write_notices: admin(pool, "poster", "admin:write:announcements").await.1,
    }
}

/// One request to each of the seven routes, for the walk of refusals.
fn routes() -> [fn() -> common::TestRequest; 7] {
    [
        || common::TestRequest::get(RULES),
        || common::TestRequest::post(RULES).json(&json!({"text": "x"})),
        || common::TestRequest::patch(&format!("{RULES}/1")).json(&json!({"text": "x"})),
        || common::TestRequest::delete(&format!("{RULES}/1")),
        || common::TestRequest::get(NOTICES),
        || common::TestRequest::post(NOTICES).json(&json!({"content": "x"})),
        || common::TestRequest::delete(&format!("{NOTICES}/1")),
    ]
}

/// The half of the gate that comes before any scope is consulted: no
/// credentials and an unknown token are the 401s Pleroma's user-identity
/// plug gives, on every route.
#[tokio::test]
async fn every_content_route_demands_a_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);

    for route in routes() {
        let unauthenticated = route().send(&app).await;
        common::assert_answer(&unauthenticated, StatusCode::UNAUTHORIZED, "no token");
        common::assert_error(&unauthenticated, "Invalid credentials", "no token");

        let bogus = route().bearer("not-a-real-token").send(&app).await;
        common::assert_answer(&bogus, StatusCode::UNAUTHORIZED, "a bogus token");
        common::assert_error(&bogus, "The access token is invalid", "a bogus token");
    }
}

/// The role half of the gate: an app-only token, a non-admin and a
/// moderator are refused on every route — Pleroma's `UserIsAdminPlug`
/// order, before the scope check and before a body is read.
#[tokio::test]
async fn every_content_route_demands_an_admin() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let everyone = tokens(&pool).await;

    for route in routes() {
        let unbound = route().bearer(&everyone.app_only).send(&app).await;
        common::assert_answer(&unbound, StatusCode::FORBIDDEN, "an app-only token");
        common::assert_error(
            &unbound,
            "This API requires an authenticated user",
            "an app-only token",
        );

        for (token, who) in [
            (everyone.plain.as_str(), "a non-admin user"),
            (everyone.staff.as_str(), "a moderator"),
        ] {
            let refused = route().bearer(token).send(&app).await;
            common::assert_answer(&refused, StatusCode::FORBIDDEN, who);
            common::assert_error(&refused, "User is not an admin.", who);
        }
    }
    assert_eq!(rule_count(&pool).await, 0, "no refusal wrote a rule");
    assert_eq!(notice_count(&pool).await, 0, "or a notice");
}

/// The scope half of the gate: the wrong class on the right area, the right
/// class on the wrong area, and an admin with no admin grant at all are all
/// 403s naming the class root the endpoint needed.
#[tokio::test]
async fn the_content_scopes_refuse_the_wrong_class_and_area() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let everyone = tokens(&pool).await;

    let cases = [
        (
            get(&app, &everyone.write_rules, RULES).await,
            "a write scope on a read",
            "Insufficient permissions: admin:read.",
        ),
        (
            post_rule(&app, &everyone.read_rules, &json!({"text": "x"})).await,
            "a read scope on a write",
            "Insufficient permissions: admin:write.",
        ),
        (
            get(&app, &everyone.read_rules, NOTICES).await,
            "rule rights do not read notices",
            "Insufficient permissions: admin:read.",
        ),
        (
            post_rule(&app, &everyone.write_notices, &json!({"text": "x"})).await,
            "notice rights do not write rules",
            "Insufficient permissions: admin:write.",
        ),
        (
            get(&app, &everyone.ordinary, RULES).await,
            "an admin with no admin grant",
            "Insufficient permissions: admin:read.",
        ),
        (
            patch(
                &app,
                &everyone.read_notices,
                &format!("{NOTICES}/1"),
                &json!({"content": "x"}),
            )
            .await,
            "a notice read on a notice write",
            "Insufficient permissions: admin:write.",
        ),
    ];
    for (response, who, message) in cases {
        common::assert_answer(&response, StatusCode::FORBIDDEN, who);
        common::assert_error(&response, message, who);
    }
}

/// The grant half of the gate: each narrow area scope passes its own
/// routes, and the two areas are independent of one another.
#[tokio::test]
async fn the_content_scopes_pass_their_own_routes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let everyone = tokens(&pool).await;

    let written = a_rule(&app, &everyone.write_rules, "scoped", 0).await;
    let id = entity_id(&written);
    common::assert_answer(
        &patch(
            &app,
            &everyone.write_rules,
            &format!("{RULES}/{id}"),
            &json!({"priority": 1}),
        )
        .await,
        StatusCode::OK,
        "admin:write:rules on the patch",
    );
    common::assert_answer(
        &destroy(&app, &everyone.write_rules, &format!("{RULES}/{id}")).await,
        StatusCode::OK,
        "admin:write:rules on the delete",
    );
    common::assert_answer(
        &get(&app, &everyone.read_rules, RULES).await,
        StatusCode::OK,
        "admin:read:rules on the listing",
    );

    let notice = a_notice(&app, &everyone.write_notices, "scoped").await;
    let nid = entity_id(&notice);
    common::assert_answer(
        &get(&app, &everyone.read_notices, &format!("{NOTICES}/{nid}")).await,
        StatusCode::OK,
        "admin:read:announcements on the show",
    );
    common::assert_answer(
        &get(&app, &everyone.read_notices, NOTICES).await,
        StatusCode::OK,
        "and on the listing",
    );
    common::assert_answer(
        &patch(
            &app,
            &everyone.write_notices,
            &format!("{NOTICES}/{nid}"),
            &json!({"all_day": true}),
        )
        .await,
        StatusCode::OK,
        "admin:write:announcements on the patch",
    );
    common::assert_answer(
        &destroy(&app, &everyone.write_notices, &format!("{NOTICES}/{nid}")).await,
        StatusCode::OK,
        "and on the delete",
    );
    common::assert_answer(
        &get(&app, &everyone.all, RULES).await,
        StatusCode::OK,
        "and the broad roots of one token pass either area",
    );
}

/// The class roots are the broad grant: `admin:read` reads both listings
/// and `admin:write` writes both kinds of content, with no area scope in
/// either token.
#[tokio::test]
async fn the_admin_class_roots_pass_every_route_of_their_class() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = app(&pool);
    let (_reader, read) = admin(&pool, "watcher", "admin:read").await;
    let (_mover, write) = admin(&pool, "pusher", "admin:write").await;

    let written = a_rule(&app, &write, "by the root", 1).await;
    let notice = a_notice(&app, &write, "by the root").await;
    let rules = get(&app, &read, RULES).await.json_value();
    assert_eq!(
        rules,
        json!([{"id": entity_id(&written), "text": "by the root", "hint": "", "priority": 1}]),
        "the read root lists what the write root made"
    );
    assert_eq!(
        ids_of(&get(&app, &read, NOTICES).await.json_value()),
        vec![entity_id(&notice)],
        "and reads the notice area too"
    );
}
