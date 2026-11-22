//! The auth and scope gates over both admin areas.

use super::*;

/// The auth matrix over both areas: no token is 401, an app-only token
/// and a non-admin user are 403, and the scope gates answer per class
/// and area — `admin:read:oauth_apps` opens the listing but no write,
/// `admin:write:instance_documents` the document writes but not their
/// read, and a foreign area scope opens nothing.
#[tokio::test]
async fn the_auth_gates_answer_per_class_and_area() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let plain = common::create_local_user(&pool, "plain").await;
    let plain_token = common::create_app_and_token(&pool, &plain, "admin:write").await;
    let app_only = common::create_app_only_token(&pool, "admin:read").await;
    let reports_admin = admin(&pool, "reports", "admin:read:reports").await;
    let apps_read_admin = admin(&pool, "appsreader", "admin:read:oauth_apps").await;
    let docs_write_admin = admin(&pool, "docswriter", "admin:write:instance_documents").await;

    let anonymous = TestRequest::get(APPS).send(&app).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token, no listing");
    common::assert_error(&anonymous, NO_TOKEN, "the shared 401 wording");

    let app_only_read = TestRequest::get(APPS).bearer(&app_only).send(&app).await;
    common::assert_answer(&app_only_read, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(&app_only_read, USER_REQUIRED, "needs a user");

    let plain_write = TestRequest::post(APPS)
        .bearer(&plain_token)
        .json(&json!({"name": "Nope", "redirect_uris": OOB}))
        .send(&app)
        .await;
    common::assert_answer(&plain_write, StatusCode::FORBIDDEN, "a non-admin");
    common::assert_error(&plain_write, NOT_ADMIN, "is no admin");

    let wrong_area = TestRequest::get(APPS)
        .bearer(&reports_admin)
        .send(&app)
        .await;
    common::assert_answer(&wrong_area, StatusCode::FORBIDDEN, "a foreign area scope");
    common::assert_error(&wrong_area, NO_ADMIN_READ, "names the class root");

    let read_write = TestRequest::post(APPS)
        .bearer(&apps_read_admin)
        .json(&json!({"name": "X", "redirect_uris": OOB}))
        .send(&app)
        .await;
    common::assert_answer(
        &read_write,
        StatusCode::FORBIDDEN,
        "a read grant writes nothing",
    );
    common::assert_error(&read_write, NO_ADMIN_WRITE, "naming the write root");

    let docs_read = TestRequest::get(DOC_TOS)
        .bearer(&docs_write_admin)
        .send(&app)
        .await;
    common::assert_answer(
        &docs_read,
        StatusCode::FORBIDDEN,
        "a documents-write grant reads nothing",
    );
    common::assert_error(&docs_read, NO_ADMIN_READ, "the read root");

    let docs_write = TestRequest::delete(DOC_TOS)
        .bearer(&docs_write_admin)
        .send(&app)
        .await;
    common::assert_answer(
        &docs_write,
        StatusCode::NOT_FOUND,
        "the right grant reaches the handler (which 404s the empty store)",
    );

    let listing = TestRequest::get(APPS)
        .bearer(&apps_read_admin)
        .send(&app)
        .await;
    common::assert_answer(
        &listing,
        StatusCode::OK,
        "the narrow read scope opens its own area",
    );
}
