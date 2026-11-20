//! The inventory's Pleroma admin API, user-management half.
//!
//! The rows of the single table's `Admin API, Pleroma's own
//! (/api/v1/pleroma/admin)` header that fall inside its `users` scope;
//! [`super::pleroma_admin_other`] holds the rest of that surface, so the
//! two files together keep the section under one screen of table each.

use super::{AuthClass, RouteSpec};

/// The section's rows, in the table's order.
pub(super) const ROWS: &[RouteSpec] = &[
    // ── Admin API, Pleroma's own (`/api/v1/pleroma/admin`) ───────────────
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/users",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/:nickname",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/invites",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/activate",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/deactivate",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/approve",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/:nickname/toggle_activation",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/pleroma/admin/users/tag",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/users/tag",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/:nickname/permission_group",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/:nickname/permission_group/:permission_group",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users/:nickname/permission_group/:permission_group",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/users/:nickname/permission_group/:permission_group",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users/permission_group/:permission_group",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "DELETE",
        path: "/api/v1/pleroma/admin/users/permission_group/:permission_group",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users/follow",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users/unfollow",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/suggest",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/unsuggest",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/:nickname/credentials",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/:nickname/credentials",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/:nickname/password_reset",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/force_password_reset",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/confirm_email",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PATCH",
        path: "/api/v1/pleroma/admin/users/resend_confirmation_email",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "PUT",
        path: "/api/v1/pleroma/admin/users/disable_mfa",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users/invite_token",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users/revoke_invite",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "POST",
        path: "/api/v1/pleroma/admin/users/email_invite",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/:nickname/statuses",
        auth: AuthClass::Admin,
    },
    RouteSpec {
        method: "GET",
        path: "/api/v1/pleroma/admin/users/:nickname/chats",
        auth: AuthClass::Admin,
    },
];
