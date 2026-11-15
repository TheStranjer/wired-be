//! The admin and staff extractors: [`AdminUser`] and [`StaffUser`], the
//! role gate in front of every `/api/v1/pleroma/admin` endpoint.
//!
//! The reference is Pleroma's
//! `lib/pleroma/web/plugs/user_is_admin_plug.ex`,
//! `user_is_staff_plug.ex` and `ensure_privileged_plug.ex`, over the
//! scope rules of `docs/ARCHITECTURE.md` decision 9.
//!
//! Both run on `super::user_identity`'s back — no token at all is 401
//! "Invalid credentials", an unknown or expired one 401 "The access token
//! is invalid", an app-only token 403 "This API requires an authenticated
//! user" — and then demand the role: [`AdminUser`] accepts an admin only
//! (`users.is_admin`), answering anyone else 403 `{"error":"User is not an
//! admin."}`; [`StaffUser`] is Pleroma's staff pair, accepting an admin or
//! a moderator and answering the rest 403 `{"error":"User is not a staff
//! member."}`.
//!
//! The role is only half of Pleroma's admin gate: its scope plugs then ask
//! `admin:read`/`admin:write` (or the narrow `admin:read:<area>` /
//! `admin:write:<area>` alias). [`AdminUser::require`] and
//! [`StaffUser::require`] are that half: the handler names the endpoint
//! class (`"read"` or `"write"`) and the area it belongs to, and the
//! check passes when the token grants the class root or the area-scoped
//! sub-scope — Pleroma's `OAuthScopesPlug` does the same with e.g.
//! `scopes: ["admin:read:reports"]` over the admin routes
//! (`lib/pleroma/web/admin_api/controllers/report_controller.ex`). A token
//! that grants neither is 403
//! `{"error":"Insufficient permissions: admin:read."}`.

use axum::{extract::FromRequestParts, http::request::Parts};

use crate::{
    error::{AppError, AppResult},
    models::{oauth_token::OauthToken, user::User},
    oauth::scopes::Scopes,
    state::AppState,
};

use super::user_identity;

/// Prefix every admin scope hangs under: the class and area build
/// `admin:read:accounts` from `("read", "accounts")`.
const ADMIN_PREFIX: &str = "admin:";
/// 403 body for a valid user token whose account is not an admin
/// (Pleroma's `UserIsAdminPlug`, and the wording this ticket pins).
const NOT_ADMIN: &str = "User is not an admin.";
/// 403 body for a valid user token whose account is neither an admin nor
/// a moderator (Pleroma's `UserIsStaffPlug`).
const NOT_STAFF: &str = "User is not a staff member.";

/// Extractor for admin endpoints that demand the admin role:
/// `AdminUser(user, token)`.
///
/// Rejections are those of the shared user-token gate plus 403
/// `"User is not an admin."` for a valid user token whose account is not
/// an admin — a moderator is not an admin for this purpose, which is
/// Pleroma's `require_admin` pipeline.
/// The scope half of the gate is [`AdminUser::require`], which the handler
/// calls once the extractor has resolved the role.
#[derive(Debug)]
pub struct AdminUser(
    /// The admin behind the request.
    pub User,
    /// The token the request authenticated with.
    pub OauthToken,
);

/// Extractor for admin endpoints staff may use: `StaffUser(user, token)`,
/// passing admins and moderators alike (Pleroma's `admin_api` pipeline
/// plug `UserIsStaffPlug`).
///
/// Rejections are those of the shared user-token gate plus 403
/// `"User is not a staff member."` for a valid user token whose account
/// holds neither role; the scope half is
/// [`StaffUser::require`].
#[derive(Debug)]
pub struct StaffUser(
    /// The staff user (admin or moderator) behind the request.
    pub User,
    /// The token the request authenticated with.
    pub OauthToken,
);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let (user, token) = user_identity(parts, state).await?;
        if user.is_admin.get() {
            return Ok(Self(user, token));
        }
        Err(AppError::Forbidden(NOT_ADMIN.to_owned()))
    }
}

impl FromRequestParts<AppState> for StaffUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let (user, token) = user_identity(parts, state).await?;
        if user.is_admin.get() || user.is_moderator.get() {
            return Ok(Self(user, token));
        }
        Err(AppError::Forbidden(NOT_STAFF.to_owned()))
    }
}

impl AdminUser {
    /// The scope half of Pleroma's admin gate for this request's token:
    /// the endpoint class (`"read"` or `"write"`) of the area (e.g.
    /// `"accounts"`), granted by the class root `admin:read` /
    /// `admin:write` or by the area-scoped `admin:read:<area>` /
    /// `admin:write:<area>` (see the module's `require_admin_scope`).
    ///
    /// # Errors
    ///
    /// [`AppError::Forbidden`] with Pleroma's
    /// `"Insufficient permissions: admin:<class>."` wording when the
    /// token grants neither.
    pub fn require(&self, class: &str, area: &str) -> AppResult<()> {
        require_admin_scope(&self.1, class, area)
    }
}

impl StaffUser {
    /// [`AdminUser::require`] for the staff pair.
    ///
    /// # Errors
    ///
    /// As [`AdminUser::require`].
    pub fn require(&self, class: &str, area: &str) -> AppResult<()> {
        require_admin_scope(&self.1, class, area)
    }
}

/// Whether `token`'s scopes grant the admin endpoint class `class` of the
/// area `area`, through the Mastodon hierarchy of
/// [`Scopes::contains`]: the broad `admin:read` / `admin:write` root
/// always grants the endpoints of its class, and the narrow
/// `admin:<class>:<area>` grants exactly that area's endpoints. A token
/// that grants neither is refused with the root named, as
/// `o_auth_scopes_plug.ex` words a failed single-scope check
/// (`"Insufficient permissions: admin:read."`).
///
/// # Errors
///
/// [`AppError::Forbidden`] with that wording when the check fails.
fn require_admin_scope(token: &OauthToken, class: &str, area: &str) -> AppResult<()> {
    let root = format!("{ADMIN_PREFIX}{class}");
    let scoped = format!("{root}:{area}");
    let granted = Scopes::parse(&token.scopes);
    if granted.contains(&root) || granted.contains(&scoped) {
        return Ok(());
    }
    Err(AppError::Forbidden(format!(
        "Insufficient permissions: {root}."
    )))
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    /// A stored-looking token row granting `scopes`, minted right now.
    fn token_with(scopes: &str) -> OauthToken {
        let now = chrono::Utc::now();
        OauthToken {
            id: uuid::Uuid::now_v7(),
            token: "admin-secret".to_owned(),
            refresh_token: "refresh-secret".to_owned(),
            user_id: None,
            app_id: uuid::Uuid::now_v7(),
            scopes: scopes.to_owned(),
            valid_until: now + Duration::days(1),
            inserted_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn the_admin_roots_grant_every_class_endpoint() {
        require_admin_scope(&token_with("admin:read"), "read", "reports")
            .expect("`admin:read` grants any read area");
        require_admin_scope(&token_with("admin:write"), "write", "users")
            .expect("`admin:write` grants any write area");
    }

    #[test]
    fn area_scopes_grant_their_own_area_only() {
        require_admin_scope(&token_with("admin:read:reports"), "read", "reports")
            .expect("the area scope grants its own area");
        let err = require_admin_scope(&token_with("admin:read:reports"), "read", "accounts")
            .expect_err("and no other area");
        assert_eq!(
            err.to_string(),
            "Insufficient permissions: admin:read.",
            "the refusal names the class root, Pleroma's wording"
        );
    }

    #[test]
    fn ordinary_scopes_never_grant_admin_endpoints() {
        for scopes in ["read", "write", "read write follow"] {
            let err = require_admin_scope(&token_with(scopes), "read", "reports")
                .expect_err("an ordinary scope is no admin grant");
            assert_eq!(
                err.to_string(),
                "Insufficient permissions: admin:read.",
                "the refusal words every miss alike"
            );
        }
    }

    #[test]
    fn the_write_class_never_grants_reads() {
        require_admin_scope(&token_with("admin:write"), "write", "reports")
            .expect("`admin:write` grants its own class");
        assert!(
            require_admin_scope(&token_with("admin:write"), "read", "reports").is_err(),
            "`admin:write` never grants a read (Pleroma's `|`-less scope pair)"
        );
    }
}
