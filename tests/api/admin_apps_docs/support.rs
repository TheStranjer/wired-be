//! The fixtures, constants and request helpers the admin app and document
//! tests share. The test modules are its children, so they reach every
//! helper through `use super::*`.

mod apps;
mod documents;
mod gates;

use axum::http::StatusCode;
use bytes::Bytes;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    oauth_app,
    user::{Flag, NewUser},
};

use crate::common::{self, MultipartPart, TestRequest};

/// The app listing/creation route.
const APPS: &str = "/api/v1/pleroma/admin/oauth_app";
/// The retired public HTML route of the terms of service.
const PUBLIC_TOS: &str = "/instance/terms-of-service.html";
/// The public JSON route of the terms of service.
const JSON_TOS: &str = "/api/v1/instance/terms_of_service";
/// The admin document route for the terms of service.
const DOC_TOS: &str = "/api/v1/pleroma/admin/instance_document/terms-of-service";
/// The document store's 1 MiB ceiling.
const MAX_DOC: usize = 1024 * 1024;
/// The 401 body when no token is presented.
const NO_TOKEN: &str = "Invalid credentials";
/// The 403 body for an app-only (user-unbound) token.
const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 body for a valid token whose user is not an admin.
const NOT_ADMIN: &str = "User is not an admin.";
/// The 403 body of a missing `admin:read` grant.
const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The 403 body of a missing `admin:write` grant.
const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";
/// The 404 body of the unknown id and the unknown document name alike.
const NOT_FOUND: &str = "Not found";
/// The redirect URI every app body in these tests registers.
const OOB: &str = "urn:ietf:wg:oauth:2.0:oob";
/// The multipart boundary every test body frames with.
const BOUNDARY: &str = "wired-docs-boundary";

/// A local admin named `nickname` with a bearer token granting `scopes`.
async fn admin(pool: &PgPool, nickname: &str, scopes: &str) -> String {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    common::create_app_and_token(pool, &created, scopes).await
}

/// Creates one app over HTTP as `token` and returns the raw response.
async fn create_app(app: &axum::Router, token: &str, body: Value) -> common::TestResponse {
    TestRequest::post(APPS)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// The total row count of `oauth_apps`, the number the listing's
/// `count` mirrors (Pleroma's whole-table aggregate).
async fn total_apps(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_apps")
        .fetch_one(pool)
        .await
        .expect("the app count reads")
}

/// The `entity["id"]` text of a create answer, owned.
fn entity_id(response: &common::TestResponse) -> String {
    response.json_value()["id"]
        .as_str()
        .expect("an id text")
        .to_owned()
}

/// The `entity["client_id"]` text of a create answer, owned.
fn entity_client_id(response: &common::TestResponse) -> String {
    response.json_value()["client_id"]
        .as_str()
        .expect("a client id")
        .to_owned()
}

/// The id list of a listing answer's `apps` array.
fn listed_ids(page: &Value) -> Vec<String> {
    page["apps"]
        .as_array()
        .expect("the apps array")
        .iter()
        .map(|row| row["id"].as_str().expect("an id text").to_owned())
        .collect()
}

/// The app-CRUD fixture pair: an untrusted `Alpha` and a trusted `Beta`
/// created in that order (so `Beta` is the newer row).
async fn alpha_and_beta(app: &axum::Router, token: &str) -> (String, String, String, String) {
    let first = create_app(app, token, json!({"name": "Alpha", "redirect_uris": OOB})).await;
    let second = create_app(
        app,
        token,
        json!({"name": "Beta", "redirect_uris": OOB, "trusted": true}),
    )
    .await;
    (
        entity_id(&first),
        entity_client_id(&second),
        entity_id(&second),
        first.text(),
    )
}

/// Uploads a `terms-of-service` document whose file is `size` bytes of
/// `x`, and answers what the route said.
async fn upload_sized(app: &axum::Router, token: &str, size: usize) -> common::TestResponse {
    TestRequest::patch(DOC_TOS)
        .bearer(token)
        .multipart(
            BOUNDARY,
            &[MultipartPart::file(
                "file",
                "big.html",
                "text/html",
                Bytes::from(vec![b'x'; size]),
            )],
        )
        .send(app)
        .await
}
