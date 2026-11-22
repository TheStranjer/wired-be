//! The fixtures and request helpers every accounts test shares.

use axum::Router;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::user::{Flag, User};

use crate::common;

/// The instance origin the fixtures live on (`Config::for_tests`).
pub(super) const BASE: &str = "https://test.example";
/// The body every account miss on these routes carries (decision 8).
pub(super) const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;
/// The 401 body `MaybeAuthUser` renders for a token that does not resolve.
pub(super) const INVALID_TOKEN: &str = r#"{"error":"The access token is invalid"}"#;
/// The 400 body when the `acct` parameter is absent or blank.
pub(super) const MISSING_ACCT: &str = r#"{"error":"Missing parameter: acct"}"#;
/// The 401 body `AuthUser` renders when no token was presented at all.
pub(super) const NO_TOKEN: &str = r#"{"error":"Invalid credentials"}"#;
/// The 403 body `AuthUser` renders for an app-only token.
pub(super) const USER_REQUIRED: &str = r#"{"error":"This API requires an authenticated user"}"#;
/// The 403 body the scope check renders for a token that lacks the scope.
pub(super) const NO_SCOPE: &str = r#"{"error":"Insufficient permissions: read:accounts."}"#;

/// `GET /api/v1/accounts/verify_credentials` with `token` as bearer.
pub(super) async fn verify_credentials(app: &Router, token: &str) -> common::TestResponse {
    common::TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(token)
        .send(app)
        .await
}

/// The `source` object of a rendered account, present or panicked over:
/// the tests that ask for it are the ones asserting its shape.
pub(super) fn source_of(body: &Value) -> Value {
    body.get("source")
        .cloned()
        .expect("verify_credentials renders the source block")
}

/// `GET /api/v1/accounts/{id}` for `id`, without credentials.
pub(super) async fn show(app: &Router, id: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/api/v1/accounts/{id}"))
        .send(app)
        .await
}

/// `GET /api/v1/accounts/lookup` with `query` appended verbatim: either
/// empty or starting at the `?`.
pub(super) async fn lookup(app: &Router, query: &str) -> common::TestResponse {
    common::TestRequest::get(&format!("/api/v1/accounts/lookup{query}"))
        .send(app)
        .await
}

/// The identity fields of a rendered [`Account`]: which row came back.
/// `full` is the handle the entity must show, `username` its nickname
/// part and `url` the human-facing profile link.
pub(super) fn assert_identity(body: &Value, account: &User, full: &str, username: &str, url: &str) {
    assert_eq!(body["id"], account.id.to_string(), "id is the uuid text");
    assert_eq!(body["acct"], full, "acct is the stored nickname");
    assert_eq!(body["username"], username, "username drops the domain");
    assert_eq!(body["url"], url, "url is the profile page");
    assert_eq!(body["uri"], account.ap_id, "uri is the actor id");
}

/// The account whose own view is the fullest: a raw note and its rendered
/// form, posting defaults, hidden counters, a role it keeps to itself, a
/// settings blob and one alias.
pub(super) async fn owner_view(pool: &PgPool) -> User {
    common::create_local_user_with(pool, "alice", |mut new| {
        new.raw_bio = "hi **there**".to_owned();
        new.bio = "<p>hi <strong>there</strong></p>".to_owned();
        new.default_scope = "unlisted".to_owned();
        new.sensitive_default = Flag::new(true);
        new.language = Some("eo".to_owned());
        new.no_rich_text = Flag::new(true);
        new.discoverable = Flag::new(true);
        new.followers_count = 5;
        new.hide_followers_count = Flag::new(true);
        new.is_admin = Flag::new(true);
        new.show_role = Flag::new(false);
        new.settings_store = json!({"pleroma_fe": {"theme": "dark"}});
        new.also_known_as = vec!["https://old.example/users/alice".to_owned()];
        new
    })
    .await
}
