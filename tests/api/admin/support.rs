//! Shared consts and helpers for every submodule of the `admin` area: the
//! moderation-log and stats routes, the canned error bodies the gates
//! assert, and the staff-user fixtures and two-route extractor app the
//! matrix runs on. Everything shared here is `pub(super)`, and a
//! submodule imports exactly the members it calls; a helper only one
//! submodule uses lives there instead.

use axum::{Json, Router, routing::get};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    auth::admin::{AdminUser, StaffUser},
    error::AppError,
    models::user::{Flag, NewUser, User},
    state::AppState,
};

use crate::common;

/// The moderation-log route.
pub(super) const LOG: &str = "/api/v1/pleroma/admin/moderation_log";
/// The stats route.
pub(super) const STATS: &str = "/api/v1/pleroma/admin/stats";
/// The 401 body when no token is presented.
pub(super) const NO_TOKEN: &str = "Invalid credentials";
/// The 401 body when a presented token does not resolve.
pub(super) const INVALID_TOKEN: &str = "The access token is invalid";
/// The 403 body for an app-only (user-unbound) token.
pub(super) const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 body for a valid token whose user is not an admin.
pub(super) const NOT_ADMIN: &str = "User is not an admin.";
/// The 403 body for a valid token whose user is not staff.
pub(super) const NOT_STAFF: &str = "User is not a staff member.";
/// The 403 body of a missing `admin:read` grant on these read routes.
pub(super) const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The 403 body of a missing `admin:write` grant on the write routes.
pub(super) const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";
/// The 404 body of the revoke route for a token no invite carries, and of
/// the webhook routes for an id no row answers.
pub(super) const NOT_FOUND: &str = "Not found";
/// The window-start margin of the date-filter test, hours before the
/// two-day-old entry.
pub(super) const WINDOW_HOURS_BACK_START: i64 = 60;
/// The window-end margin, safely between the one- and two-day-old rows.
pub(super) const WINDOW_HOURS_BACK_END: i64 = 25;

/// A local user named `nickname` whose account is an admin, with a bearer
/// token granting `scopes`.
pub(super) async fn admin(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` whose account is a moderator, with a
/// bearer token granting `scopes`.
pub(super) async fn moderator(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user_with(pool, nickname, |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A local user named `nickname` with no staff role, with a bearer token
/// granting `scopes`.
pub(super) async fn regular(pool: &PgPool, nickname: &str, scopes: &str) -> (User, String) {
    let created = common::create_local_user(pool, nickname).await;
    let token = common::create_app_and_token(pool, &created, scopes).await;
    (created, token)
}

/// A two-route test router guarding each extractor in turn: `AdminUser`
/// then its `admin:read` gate on `/admin`, and `StaffUser` then the same
/// gate on `/staff`. This is the extractor matrix in isolation — the
/// staff routes the admin reports surface serves are gated and tested on
/// the real router in `admin_reports.rs`.
pub(super) fn extractor_app(pool: PgPool) -> Router {
    Router::new()
        .route("/admin", get(admin_view))
        .route("/staff", get(staff_view))
        .with_state(app_state(pool))
}

/// The shared [`AppState`] every test router runs on.
fn app_state(pool: PgPool) -> AppState {
    common::test_app_state(pool, common::test_config())
}

/// Reports the resolved admin and the token's scopes.
async fn admin_view(admin: AdminUser) -> Result<Json<Value>, AppError> {
    admin.require("read", "accounts")?;
    Ok(Json(json!({
        "nickname": admin.0.nickname,
        "is_admin": admin.0.is_admin.get(),
        "scopes": admin.1.scopes,
    })))
}

/// Reports the resolved staff user and the token's scopes.
async fn staff_view(staff: StaffUser) -> Result<Json<Value>, AppError> {
    staff.require("read", "accounts")?;
    Ok(Json(json!({
        "nickname": staff.0.nickname,
        "is_admin": staff.0.is_admin.get(),
        "is_moderator": staff.0.is_moderator.get(),
        "scopes": staff.1.scopes,
    })))
}
