//! The bearer extractors and scope guards of [`wired_be::auth`], tested
//! through a tiny test-only router with one route per extractor (per
//! `docs/ARCHITECTURE.md` decision 13, since the extractors guard no
//! production endpoint yet). Each route answers 200 with what the
//! extractor resolved, so the success side asserts identity, and every
//! rejection arrives as `AppError`'s JSON (`{"error": …}`) with the
//! status codes the ticket pins.

#![cfg(test)]

pub mod common;

use std::sync::Arc;

use axum::{Json, Router, http::StatusCode, routing::get};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    auth::{AppAuth, AuthContext, AuthUser, MaybeAuthUser},
    error::AppError,
    http_client::HttpClient,
    models::user::{Flag, NewUser},
    state::AppState,
    uploads::UploaderBackend,
};

use common::TestRequest;

/// The application name every [`create_app_and_token`] fixture registers.
const TEST_CLIENT: &str = "Test Client";

/// The router under test: each route runs one extractor (and, on the
/// `/scope/…` routes, one scope guard) and reports what it resolved.
fn auth_test_app(pool: PgPool) -> Router {
    let config = Arc::new(common::test_config());
    let state = AppState {
        db: pool,
        config: Arc::clone(&config),
        http_client: HttpClient::from(config.as_ref()),
        uploader: UploaderBackend::from(config.as_ref()),
        instance_stats_cache: Arc::default(),
    };
    Router::new()
        .route("/auth-user", get(auth_user_view))
        .route("/maybe-auth-user", get(maybe_auth_user_view))
        .route("/app-auth", get(app_auth_view))
        .route("/auth-context", get(auth_context_view))
        .route("/scope/statuses", get(statuses_scope))
        .route("/scope/read", get(read_scope))
        .route("/scope/any", get(any_scope))
        .with_state(state)
}

/// `AuthUser`: the resolved user and the token that carried it.
async fn auth_user_view(AuthUser(user, token): AuthUser) -> Json<Value> {
    Json(json!({
        "user_id": user.id.to_string(),
        "nickname": user.nickname,
        "token_scopes": token.scopes,
    }))
}

/// `MaybeAuthUser`: the user or `null`, and the scopes of the token the
/// request presented (absent when it presented none).
async fn maybe_auth_user_view(MaybeAuthUser(user, token): MaybeAuthUser) -> Json<Value> {
    Json(json!({
        "user_id": user.map(|found| found.id.to_string()),
        "token_scopes": token.map(|found| found.scopes),
    }))
}

/// `AppAuth`: the issuing app and whether its token is user-bound.
async fn app_auth_view(AppAuth(app, token): AppAuth) -> Json<Value> {
    Json(json!({
        "client_name": app.client_name,
        "user_id": token.user_id.map(|found| found.to_string()),
    }))
}

/// `AuthContext`: the whole resolved identity.
async fn auth_context_view(AuthContext { token, user, app }: AuthContext) -> Json<Value> {
    Json(json!({
        "user_id": user.map(|found| found.id.to_string()),
        "client_name": app.client_name,
        "token_scopes": token.scopes,
    }))
}

/// Requires the `read:statuses` scope.
async fn statuses_scope(auth: AuthUser) -> Result<&'static str, AppError> {
    auth.require("read:statuses")?;
    Ok("ok")
}

/// Requires the plain `read` scope.
async fn read_scope(auth: AuthUser) -> Result<&'static str, AppError> {
    auth.require("read")?;
    Ok("ok")
}

/// Requires `read:accounts` or `read`.
async fn any_scope(auth: AuthUser) -> Result<&'static str, AppError> {
    auth.require_any(&["read:accounts", "read"])?;
    Ok("ok")
}

/// Moves the token string `token`'s `valid_until` an hour into the past.
async fn expire_token(pool: &PgPool, token: &str) {
    let outcome = sqlx::query(
        "UPDATE oauth_tokens SET valid_until = now() - interval '1 hour' WHERE token = $1",
    )
    .bind(token)
    .execute(pool)
    .await
    .expect("the expiry update runs");
    assert_eq!(outcome.rows_affected(), 1, "the update hit the token row");
}

/// The `error` string of an error response body.
fn error_of(response: &common::TestResponse) -> String {
    let body = response.json_value();
    body["error"]
        .as_str()
        .expect("the error body carries a string")
        .to_owned()
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn auth_user_accepts_a_valid_bearer_token(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read").await;
    let app = auth_test_app(pool);

    let response = TestRequest::get("/auth-user")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "a live token passes");
    let body: Value = response.json();
    assert_eq!(
        body["user_id"],
        alice.id.to_string(),
        "the extractor resolved alice"
    );
    assert_eq!(body["nickname"], "alice", "and carried her row");
    assert_eq!(body["token_scopes"], "read", "with the token beside it");
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn bearer_scheme_is_matched_case_insensitively(pool: PgPool) {
    let bob = common::create_local_user(&pool, "bob").await;
    let token = common::create_app_and_token(&pool, &bob, "read").await;
    let app = auth_test_app(pool);

    for scheme in ["Bearer", "bearer", "BeArEr"] {
        let response = TestRequest::get("/auth-user")
            .header("authorization", &format!("{scheme} {token}"))
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "`{scheme} <token>` is still a bearer header"
        );
    }
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn access_token_query_parameter_is_accepted(pool: PgPool) {
    let carol = common::create_local_user(&pool, "carol").await;
    let token = common::create_app_and_token(&pool, &carol, "read").await;
    let app = auth_test_app(pool);

    let response = TestRequest::get(&format!("/auth-user?access_token={token}"))
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the query parameter authenticates too"
    );
    let body: Value = response.json();
    assert_eq!(body["user_id"], carol.id.to_string(), "same user");
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn header_wins_over_the_query_parameter(pool: PgPool) {
    let dave = common::create_local_user(&pool, "dave").await;
    let token = common::create_app_and_token(&pool, &dave, "read").await;
    let app = auth_test_app(pool);

    let ok = TestRequest::get("/auth-user?access_token=not-a-token")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        ok.status,
        StatusCode::OK,
        "a good header is not poisoned by a junk query parameter"
    );

    let denied = TestRequest::get(&format!("/auth-user?access_token={token}"))
        .bearer("not-a-token")
        .send(&app)
        .await;
    assert_eq!(
        denied.status,
        StatusCode::UNAUTHORIZED,
        "a junk header is not rescued by a good query parameter"
    );
    assert_eq!(
        error_of(&denied),
        "The access token is invalid",
        "the header's token was the one consulted"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn auth_user_without_any_token_is_unauthorized(pool: PgPool) {
    let app = auth_test_app(pool);

    let response = TestRequest::get("/auth-user").send(&app).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED, "no token at all");
    assert_eq!(
        error_of(&response),
        "Invalid credentials",
        "the missing-token wording differs from the invalid one"
    );
    assert_eq!(
        response.header("www-authenticate"),
        Some("Bearer"),
        "a 401 challenges with the bearer scheme"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn auth_user_rejects_an_unknown_token(pool: PgPool) {
    let app = auth_test_app(pool);

    let response = TestRequest::get("/auth-user")
        .bearer("no-such-token")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED, "401");
    assert_eq!(
        error_of(&response),
        "The access token is invalid",
        "an unknown token is invalid, not merely missing"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn auth_user_rejects_an_expired_token(pool: PgPool) {
    let erin = common::create_local_user(&pool, "erin").await;
    let token = common::create_app_and_token(&pool, &erin, "read").await;
    expire_token(&pool, &token).await;
    let app = auth_test_app(pool);

    let response = TestRequest::get("/auth-user")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED, "expired 401");
    assert_eq!(
        error_of(&response),
        "The access token is invalid",
        "an expired token shares the invalid wording"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn auth_user_rejects_a_deactivated_users_token(pool: PgPool) {
    let frank = common::create_local_user_with(&pool, "frank", |new| NewUser {
        is_active: Flag::new(false),
        ..new
    })
    .await;
    let token = common::create_app_and_token(&pool, &frank, "read").await;
    let app = auth_test_app(pool);

    let response = TestRequest::get("/auth-user")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN, "403");
    assert_eq!(
        error_of(&response),
        "Your account is currently disabled",
        "a deactivated owner is reported as such, not as an invalid token"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn auth_user_rejects_an_app_only_token(pool: PgPool) {
    let token = common::create_app_only_token(&pool, "read write").await;
    let app = auth_test_app(pool);

    let response = TestRequest::get("/auth-user")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN, "app-only 403");
    assert_eq!(
        error_of(&response),
        "This API requires an authenticated user",
        "a live but user-unbound token is the wrong kind, not an invalid one"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn maybe_auth_user_without_a_token_is_none(pool: PgPool) {
    let app = auth_test_app(pool);

    let response = TestRequest::get("/maybe-auth-user").send(&app).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "an anonymous request still passes"
    );
    assert!(
        response.json_value()["user_id"].is_null(),
        "and resolves to None"
    );
    assert!(
        response.json_value()["token_scopes"].is_null(),
        "with no token presented there is no token to carry either"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn maybe_auth_user_resolves_a_valid_user_token(pool: PgPool) {
    let grace = common::create_local_user(&pool, "grace").await;
    let token = common::create_app_and_token(&pool, &grace, "read").await;
    let app = auth_test_app(pool);

    let response = TestRequest::get("/maybe-auth-user")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "valid token 200");
    assert_eq!(
        response.json_value()["user_id"],
        grace.id.to_string(),
        "the user is carried along"
    );
    assert_eq!(
        response.json_value()["token_scopes"],
        "read",
        "and the token rides along for a scope gate to read"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn maybe_auth_user_rejects_an_invalid_token(pool: PgPool) {
    let app = auth_test_app(pool);

    let response = TestRequest::get("/maybe-auth-user")
        .bearer("no-such-token")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::UNAUTHORIZED,
        "a supplied invalid token is never silently ignored"
    );
    assert_eq!(
        error_of(&response),
        "The access token is invalid",
        "the same 401 wording as AuthUser"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn maybe_auth_user_rejects_a_deactivated_users_token(pool: PgPool) {
    let henry = common::create_local_user_with(&pool, "henry", |new| NewUser {
        is_active: Flag::new(false),
        ..new
    })
    .await;
    let token = common::create_app_and_token(&pool, &henry, "read").await;
    let app = auth_test_app(pool);

    let response = TestRequest::get("/maybe-auth-user")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::UNAUTHORIZED,
        "a deactivated user's token is invalid, not anonymous"
    );
    assert_eq!(
        error_of(&response),
        "The access token is invalid",
        "MaybeAuthUser has no user to report as disabled"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn maybe_auth_user_treats_an_app_only_token_as_none(pool: PgPool) {
    let token = common::create_app_only_token(&pool, "read").await;
    let app = auth_test_app(pool);

    let response = TestRequest::get("/maybe-auth-user")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "app-only passes");
    assert!(
        response.json_value()["user_id"].is_null(),
        "as None, there is no user to hand over"
    );
    assert_eq!(
        response.json_value()["token_scopes"],
        "read",
        "the token itself is still carried: the app-only request presented one"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn app_auth_accepts_any_valid_token(pool: PgPool) {
    let ivan = common::create_local_user(&pool, "ivan").await;
    let user_token = common::create_app_and_token(&pool, &ivan, "read").await;
    let app_only_token = common::create_app_only_token(&pool, "read").await;
    let app = auth_test_app(pool);

    let as_user = TestRequest::get("/app-auth")
        .bearer(&user_token)
        .send(&app)
        .await;
    assert_eq!(as_user.status, StatusCode::OK, "a user token passes");
    let body = as_user.json_value();
    assert_eq!(body["client_name"], TEST_CLIENT, "the app is resolved");
    assert_eq!(
        body["user_id"],
        ivan.id.to_string(),
        "and the token is user-bound"
    );

    let as_app = TestRequest::get("/app-auth")
        .bearer(&app_only_token)
        .send(&app)
        .await;
    assert_eq!(
        as_app.status,
        StatusCode::OK,
        "an app-only token passes too"
    );
    assert!(
        as_app.json_value()["user_id"].is_null(),
        "and has no user behind it"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn app_auth_rejects_missing_invalid_and_deactivated_tokens(pool: PgPool) {
    let judy = common::create_local_user_with(&pool, "judy", |new| NewUser {
        is_active: Flag::new(false),
        ..new
    })
    .await;
    let stale = common::create_app_and_token(&pool, &judy, "read").await;
    let app = auth_test_app(pool);

    let none = TestRequest::get("/app-auth").send(&app).await;
    assert_eq!(none.status, StatusCode::UNAUTHORIZED, "no token 401");
    assert_eq!(
        error_of(&none),
        "Invalid credentials",
        "the missing-token wording"
    );

    let unknown = TestRequest::get("/app-auth")
        .bearer("nope")
        .send(&app)
        .await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED, "unknown 401");
    assert_eq!(error_of(&unknown), "The access token is invalid", "…");

    let deactivated = TestRequest::get("/app-auth")
        .bearer(&stale)
        .send(&app)
        .await;
    assert_eq!(
        deactivated.status,
        StatusCode::UNAUTHORIZED,
        "AppAuth needs no user, but a deactivated owner still invalidates the token"
    );
    assert_eq!(
        error_of(&deactivated),
        "The access token is invalid",
        "reported as invalid, not as disabled"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn auth_context_carries_token_user_and_app(pool: PgPool) {
    let ken = common::create_local_user(&pool, "ken").await;
    let user_token = common::create_app_and_token(&pool, &ken, "read write").await;
    let app_only_token = common::create_app_only_token(&pool, "read").await;
    let app = auth_test_app(pool);

    let as_user = TestRequest::get("/auth-context")
        .bearer(&user_token)
        .send(&app)
        .await;
    assert_eq!(as_user.status, StatusCode::OK, "a user token passes");
    let body = as_user.json_value();
    assert_eq!(body["user_id"], ken.id.to_string(), "with the user");
    assert_eq!(body["client_name"], TEST_CLIENT, "and the app");
    assert_eq!(body["token_scopes"], "read write", "and the token");

    let as_app = TestRequest::get("/auth-context")
        .bearer(&app_only_token)
        .send(&app)
        .await;
    assert_eq!(as_app.status, StatusCode::OK, "an app-only token passes");
    assert!(
        as_app.json_value()["user_id"].is_null(),
        "with the user slot empty"
    );

    let denied = TestRequest::get("/auth-context")
        .bearer("no-such-token")
        .send(&app)
        .await;
    assert_eq!(denied.status, StatusCode::UNAUTHORIZED, "invalid 401");
    assert_eq!(error_of(&denied), "The access token is invalid", "…");
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn scope_guard_grants_through_the_hierarchy(pool: PgPool) {
    let leo = common::create_local_user(&pool, "leo").await;
    let broad = common::create_app_and_token(&pool, &leo, "read").await;
    let exact = common::create_app_and_token(&pool, &leo, "read:statuses").await;
    let wrong_tree = common::create_app_and_token(&pool, &leo, "write").await;
    let app = auth_test_app(pool);

    let by_broad = TestRequest::get("/scope/statuses")
        .bearer(&broad)
        .send(&app)
        .await;
    assert_eq!(
        by_broad.status,
        StatusCode::OK,
        "a `read` token passes `read:statuses`"
    );

    let by_exact = TestRequest::get("/scope/statuses")
        .bearer(&exact)
        .send(&app)
        .await;
    assert_eq!(by_exact.status, StatusCode::OK, "the exact scope passes");

    let denied = TestRequest::get("/scope/statuses")
        .bearer(&wrong_tree)
        .send(&app)
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "403");
    assert_eq!(
        error_of(&denied),
        "Insufficient permissions: read:statuses.",
        "Pleroma's wording, naming the missing scope"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn scope_guard_denies_a_narrower_token_a_broader_scope(pool: PgPool) {
    let mia = common::create_local_user(&pool, "mia").await;
    let narrow = common::create_app_and_token(&pool, &mia, "read:accounts").await;
    let app = auth_test_app(pool);

    let response = TestRequest::get("/scope/read")
        .bearer(&narrow)
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN, "403");
    assert_eq!(
        error_of(&response),
        "Insufficient permissions: read.",
        "a `read:accounts` token does not satisfy plain `read`"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn require_any_passes_on_one_granted_alternative(pool: PgPool) {
    let nora = common::create_local_user(&pool, "nora").await;
    let narrow = common::create_app_and_token(&pool, &nora, "read:accounts").await;
    let broad = common::create_app_and_token(&pool, &nora, "read").await;
    let unrelated = common::create_app_and_token(&pool, &nora, "write:statuses").await;
    let app = auth_test_app(pool);

    for (label, token) in [("read:accounts", &narrow), ("read", &broad)] {
        let response = TestRequest::get("/scope/any")
            .bearer(token)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "a `{label}` token grants one of the alternatives"
        );
    }

    let denied = TestRequest::get("/scope/any")
        .bearer(&unrelated)
        .send(&app)
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "403");
    assert_eq!(
        error_of(&denied),
        "Insufficient permissions: read:accounts | read.",
        "the denial joins every missing scope with `|`, Pleroma-style"
    );
}

#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_user_check_precedes_the_scope_check(pool: PgPool) {
    let token = common::create_app_only_token(&pool, "read").await;
    let app = auth_test_app(pool);

    let response = TestRequest::get("/scope/statuses")
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "an app-only token fails the extractor before any scope runs"
    );
    assert_eq!(
        error_of(&response),
        "This API requires an authenticated user",
        "so the answer is the user rejection, not a scope one"
    );
}
