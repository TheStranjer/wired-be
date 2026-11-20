//! The extractor matrix and the routes' admin demand (WBE-194): the
//! [`AdminUser`] gate and its `admin:read` scope half on `/admin`, the
//! [`StaffUser`] matrix on `/staff`, and the log, stats and config routes
//! each of which demands an admin token on the real router.
//!
//! [`AdminUser`]: wired_be::auth::admin::AdminUser
//! [`StaffUser`]: wired_be::auth::admin::StaffUser

use axum::http::StatusCode;
use serde_json::Value;

use super::config_writes::{CONFIG, CONFIG_DESCRIPTIONS, NEED_REBOOT, RESTART};
use super::support::{
    INVALID_TOKEN, LOG, NO_ADMIN_READ, NO_ADMIN_WRITE, NO_TOKEN, NOT_ADMIN, NOT_STAFF, STATS,
    USER_REQUIRED, admin, extractor_app, moderator, regular,
};
use crate::common::{self, TestRequest};

/// The extractor matrix on `/admin`: an admin passes, a moderator and a
/// regular user are refused as not-admin, an app-only token and no token
/// keep the shared rejections.
#[tokio::test]
async fn admin_extractor_matrix_over_http() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = extractor_app(pool.clone());
    let (_, admin_read) = admin(&pool, "root", "admin:read").await;
    let (_, mod_read) = moderator(&pool, "halfroot", "admin:read").await;
    let (_, plain) = regular(&pool, "user", "admin:read").await;
    let app_only = common::create_app_only_token(&pool, "admin:read").await;

    let granted = TestRequest::get("/admin")
        .bearer(&admin_read)
        .send(&app)
        .await;
    common::assert_answer(&granted, StatusCode::OK, "an admin reaches /admin");
    assert_eq!(
        granted.json_value()["nickname"],
        "root",
        "identity resolves"
    );

    for (token, who) in [(&mod_read, "a moderator"), (&plain, "a regular user")] {
        let refused = TestRequest::get("/admin").bearer(token).send(&app).await;
        common::assert_all_refused(&[refused], StatusCode::FORBIDDEN, NOT_ADMIN, who);
    }

    let app_only_refused = TestRequest::get("/admin")
        .bearer(&app_only)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[app_only_refused],
        StatusCode::FORBIDDEN,
        USER_REQUIRED,
        "an app-only token",
    );

    let anonymous = TestRequest::get("/admin").send(&app).await;
    let bogus = TestRequest::get("/admin")
        .bearer("no-such-token")
        .send(&app)
        .await;
    common::assert_all_refused(
        &[anonymous],
        StatusCode::UNAUTHORIZED,
        NO_TOKEN,
        "a request with no token",
    );
    common::assert_all_refused(
        &[bogus],
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "an unknown token",
    );
}

/// The scope half of the gate on `/admin`: a valid admin whose token
/// grants no `admin:read` is refused with the `admin:read` wording, and
/// the narrow `admin:read:accounts` alias passes the area it names.
#[tokio::test]
async fn admin_scope_gate_over_http() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = extractor_app(pool.clone());
    let (_, no_scope) = admin(&pool, "noscope", "read write").await;
    let (_, narrow) = admin(&pool, "narrow", "admin:read:accounts").await;

    let refused = TestRequest::get("/admin")
        .bearer(&no_scope)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[refused],
        StatusCode::FORBIDDEN,
        NO_ADMIN_READ,
        "an admin with no admin scope",
    );

    let granted = TestRequest::get("/admin").bearer(&narrow).send(&app).await;
    common::assert_answer(&granted, StatusCode::OK, "the narrow area scope passes");
}

/// The [`StaffUser`] matrix on `/staff`: admin and moderator both pass, a
/// regular user is refused as not staff, and the token-less rejections
/// are the shared ones.
#[tokio::test]
async fn staff_extractor_matrix_over_http() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = extractor_app(pool.clone());
    let (_, admin_read) = admin(&pool, "boss", "admin:read").await;
    let (_, mod_read) = moderator(&pool, "deputy", "admin:read").await;
    let (_, plain) = regular(&pool, "peon", "admin:read").await;
    let app_only = common::create_app_only_token(&pool, "admin:read").await;

    for (token, who, is_mod) in [
        (&admin_read, "an admin", false),
        (&mod_read, "a moderator", true),
    ] {
        let granted = TestRequest::get("/staff").bearer(token).send(&app).await;
        common::assert_answer(&granted, StatusCode::OK, &format!("{who} reaches /staff"));
        assert_eq!(
            granted.json_value()["is_moderator"],
            Value::Bool(is_mod),
            "{who} resolves with the right role flags"
        );
    }

    let refused = TestRequest::get("/staff").bearer(&plain).send(&app).await;
    common::assert_all_refused(
        &[refused],
        StatusCode::FORBIDDEN,
        NOT_STAFF,
        "a regular user",
    );

    let app_only_refused = TestRequest::get("/staff")
        .bearer(&app_only)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[app_only_refused],
        StatusCode::FORBIDDEN,
        USER_REQUIRED,
        "an app-only token",
    );

    let anonymous = TestRequest::get("/staff").send(&app).await;
    common::assert_all_refused(
        &[anonymous],
        StatusCode::UNAUTHORIZED,
        NO_TOKEN,
        "a request with no token",
    );
}

/// The log listing, stats and config routes — store, descriptions and the
/// reboot pair — all demand an admin: a moderator is refused the role, an
/// admin without the route's scope class is refused the scope, and nobody
/// without a token at all.
#[tokio::test]
async fn production_routes_demand_an_admin_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, mod_read) = moderator(&pool, "mod-no-admin", "admin:read").await;
    let (_, admin_no_scope) = admin(&pool, "admin-no-scope", "read").await;

    for route in [
        LOG,
        STATS,
        CONFIG,
        CONFIG_DESCRIPTIONS,
        NEED_REBOOT,
        RESTART,
    ] {
        let not_admin = TestRequest::get(route).bearer(&mod_read).send(&app).await;
        common::assert_all_refused(
            &[not_admin],
            StatusCode::FORBIDDEN,
            NOT_ADMIN,
            "a moderator",
        );

        let no_scope = TestRequest::get(route)
            .bearer(&admin_no_scope)
            .send(&app)
            .await;
        common::assert_all_refused(
            &[no_scope],
            StatusCode::FORBIDDEN,
            // every route above reads; only the restart request writes,
            // and a bare `read` admin holds neither admin class.
            if route == RESTART {
                NO_ADMIN_WRITE
            } else {
                NO_ADMIN_READ
            },
            "an admin without the route's admin scope",
        );

        let anonymous = TestRequest::get(route).send(&app).await;
        common::assert_all_refused(
            &[anonymous],
            StatusCode::UNAUTHORIZED,
            NO_TOKEN,
            "an anonymous request",
        );
    }
}
